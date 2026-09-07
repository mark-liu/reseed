//! The background daemon's control socket: the only local channel that can
//! type into a live Claude Code session. Its `reply` op writes the text into
//! the session's pty as bracketed paste plus a carriage return, so `/clear`
//! expands exactly as a typed one does. The per-session messaging socket
//! cannot do this: its user frames carry `skipSlashCommands`, so a slash
//! command arrives there as literal text (verified against 2.1.263).

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::MetadataExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// The daemon answers any other value with `EPROTO` naming the version it
/// speaks, so a CC upgrade fails loudly here instead of typing blind.
const PROTO: u32 = 1;
const TIMEOUT: Duration = Duration::from_secs(5);

/// One background job as `op: "list"` reports it.
#[derive(Debug, Clone, Deserialize)]
pub struct Job {
    pub short: String,
    #[serde(rename = "sessionId")]
    pub session_id: Option<String>,
    pub tempo: Option<String>,
    pub state: Option<String>,
    pub cwd: Option<String>,
    pub pid: Option<u32>,
}

impl Job {
    /// Typing into a busy worker queues the text mid-turn, so only an idle
    /// one is a safe target.
    pub fn is_idle(&self) -> bool {
        self.tempo.as_deref() == Some("idle")
    }
}

pub struct Control {
    sock: PathBuf,
    key: String,
}

impl Control {
    pub fn new(sock: PathBuf, key: String) -> Self {
        Self { sock, key }
    }

    pub fn socket(&self) -> &Path {
        &self.sock
    }

    /// Env overrides first (the tests drive a fake daemon), then the live
    /// daemon: the key file, and whichever `control.sock` under
    /// `/tmp/cc-daemon-<uid>/` answers a `list`.
    pub fn discover() -> Result<Self> {
        let key = match std::env::var("RESEED_CONTROL_KEY") {
            Ok(k) if !k.is_empty() => k,
            _ => read_key(&key_path()?)?,
        };
        if let Ok(sock) = std::env::var("RESEED_CONTROL_SOCK") {
            if !sock.is_empty() {
                return Ok(Self::new(PathBuf::from(sock), key));
            }
        }
        let mut last: Option<anyhow::Error> = None;
        for sock in candidate_sockets() {
            let c = Self::new(sock, key.clone());
            match c.list() {
                Ok(_) => return Ok(c),
                Err(e) => last = Some(e),
            }
        }
        match last {
            Some(e) => Err(e.context("no daemon control socket answered")),
            None => bail!("no daemon control socket found under /tmp/cc-daemon-*"),
        }
    }

    pub fn list(&self) -> Result<Vec<Job>> {
        let resp = self.call(&serde_json::json!({"proto": PROTO, "op": "list"}))?;
        let jobs = resp.get("jobs").cloned().unwrap_or(serde_json::Value::Null);
        Ok(serde_json::from_value(jobs).unwrap_or_default())
    }

    /// Type `text` into the job's pty and press return.
    pub fn reply(&self, short: &str, text: &str) -> Result<()> {
        self.call(&serde_json::json!({
            "proto": PROTO,
            "op": "reply",
            "short": short,
            "text": text,
            "auth": self.key,
        }))
        .map(|_| ())
    }

    fn call(&self, frame: &serde_json::Value) -> Result<serde_json::Value> {
        let mut frame = frame.clone();
        if frame.get("auth").is_none() {
            frame["auth"] = serde_json::Value::String(self.key.clone());
        }
        let stream = UnixStream::connect(&self.sock)
            .with_context(|| format!("connect {}", self.sock.display()))?;
        stream.set_read_timeout(Some(TIMEOUT))?;
        stream.set_write_timeout(Some(TIMEOUT))?;
        let mut w = &stream;
        writeln!(w, "{frame}")?;
        w.flush()?;
        let mut line = String::new();
        BufReader::new(&stream).read_line(&mut line)?;
        let resp: serde_json::Value =
            serde_json::from_str(line.trim()).with_context(|| format!("daemon said: {line:?}"))?;
        if resp.get("ok").and_then(serde_json::Value::as_bool) != Some(true) {
            let code = resp.get("code").and_then(|c| c.as_str()).unwrap_or("");
            let msg = resp.get("error").and_then(|e| e.as_str()).unwrap_or("");
            bail!("daemon refused ({code}): {msg}");
        }
        Ok(resp)
    }
}

fn key_path() -> Result<PathBuf> {
    Ok(crate::paths::home()?.join(".claude/daemon/control.key"))
}

fn read_key(path: &Path) -> Result<String> {
    let key = std::fs::read_to_string(path)
        .with_context(|| format!("read daemon control key {}", path.display()))?
        .trim()
        .to_string();
    if key.is_empty() {
        bail!("daemon control key {} is empty", path.display());
    }
    Ok(key)
}

/// `/tmp/cc-daemon-<uid>/<instance>/control.sock`, ours only: a socket owned
/// by another uid belongs to another user's daemon.
fn candidate_sockets() -> Vec<PathBuf> {
    let uid = std::fs::metadata(std::env::var("HOME").unwrap_or_default())
        .map(|m| m.uid())
        .unwrap_or(u32::MAX);
    let root = PathBuf::from(format!("/tmp/cc-daemon-{uid}"));
    let Ok(entries) = std::fs::read_dir(&root) else {
        return Vec::new();
    };
    let mut out: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path().join("control.sock"))
        .filter(|p| p.exists())
        .collect();
    out.sort();
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use std::os::unix::net::UnixListener;
    use std::sync::mpsc;
    use std::thread;
    use tempfile::tempdir;

    /// A one-shot fake daemon: hands back `response` and returns the frame
    /// it was sent, so a test can assert on the exact bytes on the wire.
    fn fake_daemon(sock: &Path, response: &'static str) -> mpsc::Receiver<String> {
        let listener = UnixListener::bind(sock).unwrap();
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            for stream in listener.incoming().take(1).flatten() {
                let mut buf = String::new();
                let mut reader = BufReader::new(&stream);
                reader.read_line(&mut buf).ok();
                let mut w = &stream;
                writeln!(w, "{response}").ok();
                w.flush().ok();
                let mut sink = Vec::new();
                (&stream).read_to_end(&mut sink).ok();
                tx.send(buf).ok();
            }
        });
        rx
    }

    #[test]
    fn reply_sends_the_op_the_daemon_expects() {
        let dir = tempdir().unwrap();
        let sock = dir.path().join("control.sock");
        let rx = fake_daemon(&sock, r#"{"ok":true,"op":"reply"}"#);
        let c = Control::new(sock, "k3y".into());
        c.reply("b8816add", "/clear").unwrap();
        let sent: serde_json::Value = serde_json::from_str(&rx.recv().unwrap()).unwrap();
        assert_eq!(sent["op"], "reply");
        assert_eq!(sent["short"], "b8816add");
        assert_eq!(sent["text"], "/clear");
        assert_eq!(sent["auth"], "k3y");
        assert_eq!(sent["proto"], PROTO);
    }

    /// A refusal must surface the daemon's own words: the proto mismatch is
    /// how a CC upgrade announces itself here.
    #[test]
    fn a_refusal_carries_the_daemons_message() {
        let dir = tempdir().unwrap();
        let sock = dir.path().join("control.sock");
        let _rx = fake_daemon(
            &sock,
            r#"{"ok":false,"code":"EPROTO","error":"proto mismatch (server=2, client=1)"}"#,
        );
        let c = Control::new(sock, "k3y".into());
        let err = c.reply("b8816add", "/clear").unwrap_err().to_string();
        assert!(err.contains("EPROTO"), "{err}");
        assert!(err.contains("server=2"), "{err}");
    }

    #[test]
    fn list_parses_jobs_and_idleness() {
        let dir = tempdir().unwrap();
        let sock = dir.path().join("control.sock");
        let _rx = fake_daemon(
            &sock,
            r#"{"ok":true,"op":"list","jobs":[{"short":"aaaa1111","sessionId":"s-1","tempo":"idle","state":"done","cwd":"/tmp","pid":1},{"short":"bbbb2222","tempo":"active"}]}"#,
        );
        let c = Control::new(sock, "k3y".into());
        let jobs = c.list().unwrap();
        assert_eq!(jobs.len(), 2);
        assert_eq!(jobs[0].session_id.as_deref(), Some("s-1"));
        assert!(jobs[0].is_idle());
        assert!(!jobs[1].is_idle());
    }

    #[test]
    fn an_empty_key_file_is_an_error_not_an_empty_auth() {
        let dir = tempdir().unwrap();
        let key = dir.path().join("control.key");
        std::fs::write(&key, "   \n").unwrap();
        assert!(read_key(&key).is_err());
    }
}
