//! The daemon's per-worker pty socket.
//!
//! One auth frame and the daemon replays that session's rendered screen back.
//! A read writes nothing: on a quiet session the replay burst is followed by no
//! further traffic, so it leaves no trace. Attaching a real client is the
//! opposite - it carries a window size and resizes the session's pty - which is
//! why this speaks the socket directly. `type_input` is the one write: raw
//! keystrokes, as an attached client sends them.

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::collections::HashMap;
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// Frame kinds on the wire. The u32 length prefix counts the payload only and
/// excludes the kind byte; reading it as inclusive misaligns every frame after
/// the first and silently yields an empty screen. Kind 0 is output from the
/// host and keystrokes from a client (CC 2.1.271 `Bit`, `IIe=0`).
const KIND_OUTPUT: u8 = 0;
const KIND_INPUT: u8 = 0;
const KIND_CONTROL: u8 = 1;

const READ_TIMEOUT: Duration = Duration::from_millis(if cfg!(test) { 50 } else { 2000 });
/// The replay arrives in one burst; a busy session's is a few hundred KB.
const BURST: Duration = Duration::from_secs(5);
const MAX_STREAM: usize = 8 << 20;

#[derive(Debug, Clone, Deserialize)]
pub struct Worker {
    #[serde(rename = "ptySock")]
    pub pty_sock: PathBuf,
    #[serde(rename = "ptyAuth")]
    pub pty_auth: String,
}

#[derive(Debug, Deserialize)]
struct Roster {
    workers: HashMap<String, Worker>,
}

pub fn workers() -> Result<HashMap<String, Worker>> {
    let path = crate::paths::roster()?;
    let raw =
        std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    let roster: Roster =
        serde_json::from_str(&raw).with_context(|| format!("parsing {}", path.display()))?;
    Ok(roster.workers)
}

fn frame(kind: u8, body: &[u8]) -> Vec<u8> {
    let mut f = Vec::with_capacity(5 + body.len());
    f.extend_from_slice(&(body.len() as u32).to_be_bytes());
    f.push(kind);
    f.extend_from_slice(body);
    f
}

/// Connect, authenticate, and take the replay burst the host sends first, with
/// whether the socket then went quiet rather than still flowing at the deadline.
fn open(worker: &Worker) -> Result<(UnixStream, Vec<u8>, bool)> {
    let mut sock = UnixStream::connect(&worker.pty_sock)
        .with_context(|| format!("connecting {}", worker.pty_sock.display()))?;
    sock.set_read_timeout(Some(READ_TIMEOUT))?;
    sock.set_write_timeout(Some(READ_TIMEOUT))?;

    let auth = serde_json::json!({ "t": "auth", "token": worker.pty_auth }).to_string();
    sock.write_all(&frame(KIND_CONTROL, auth.as_bytes()))
        .context("sending the auth frame")?;
    let (replay, quiet) = drain(&mut sock)?;
    Ok((sock, replay, quiet))
}

/// The rendered screen for one job, as the concatenated output frames.
pub fn read_screen(worker: &Worker) -> Result<Vec<u8>> {
    Ok(output_of(&open(worker)?.1))
}

/// Type `keys` into the session, exactly as if a human pressed them. Nothing
/// checks the box first; callers read it before and after.
pub fn type_input(worker: &Worker, keys: &[u8]) -> Result<()> {
    let (mut sock, _, _) = open(worker)?;
    send(&mut sock, keys)
}

/// Type `keys` only if this connection's own replay went quiet and passes `check`,
/// so the proof and the keystrokes share one socket. Returns whether it typed.
pub fn type_if(worker: &Worker, keys: &[u8], check: impl FnOnce(&[u8]) -> bool) -> Result<bool> {
    let (mut sock, replay, quiet) = open(worker)?;
    // Any repaint reaches this socket, so silence leaves only a keystroke in flight.
    if !quiet || !whole_frames(&replay) || !check(&output_of(&replay)) {
        return Ok(false);
    }
    // Output queued while `check` parsed means the screen moved under the proof.
    if arrived_since(&mut sock)? {
        return Ok(false);
    }
    send(&mut sock, keys)?;
    Ok(true)
}

/// Whether `buf` ends on a frame boundary; a cut frame is a repaint not yet seen.
fn whole_frames(buf: &[u8]) -> bool {
    let mut i = 0;
    while i + 5 <= buf.len() {
        i += 5 + u32::from_be_bytes([buf[i], buf[i + 1], buf[i + 2], buf[i + 3]]) as usize;
    }
    i == buf.len()
}

/// Whether any byte or a hangup reached the socket since the drain. It consumes
/// what it reads, so only a caller that is about to give up may ask.
fn arrived_since(sock: &mut UnixStream) -> Result<bool> {
    sock.set_nonblocking(true)?;
    let arrived = match sock.read(&mut [0u8; 1]) {
        Ok(_) => true,
        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => false,
        Err(e) => return Err(e).context("checking for output after the proof"),
    };
    sock.set_nonblocking(false)?;
    Ok(arrived)
}

fn send(sock: &mut UnixStream, keys: &[u8]) -> Result<()> {
    sock.write_all(&frame(KIND_INPUT, keys))
        .context("sending the input frame")?;
    sock.flush().context("flushing the input frame")
}

fn drain(sock: &mut UnixStream) -> Result<(Vec<u8>, bool)> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 65536];
    let deadline = Instant::now() + BURST;
    while Instant::now() < deadline {
        match sock.read(&mut chunk) {
            // A hangup proves nothing: no later repaint could reach this socket.
            Ok(0) => return Ok((buf, false)),
            Ok(n) => {
                buf.extend_from_slice(&chunk[..n]);
                if buf.len() > MAX_STREAM {
                    bail!("screen replay exceeded {MAX_STREAM} bytes");
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => return Ok((buf, true)),
            Err(e) if e.kind() == std::io::ErrorKind::TimedOut => return Ok((buf, true)),
            Err(e) => return Err(e).context("reading the screen replay"),
        }
    }
    Ok((buf, false))
}

/// Concatenate the output frames, dropping the daemon's control chatter.
pub fn output_of(buf: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(buf.len());
    let mut i = 0;
    while i + 5 <= buf.len() {
        let n = u32::from_be_bytes([buf[i], buf[i + 1], buf[i + 2], buf[i + 3]]) as usize;
        let body = i + 5;
        if body + n > buf.len() {
            break;
        }
        if buf[i + 4] == KIND_OUTPUT {
            out.extend_from_slice(&buf[body..body + n]);
        }
        i = body + n;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixListener;

    #[test]
    fn typing_authenticates_then_sends_one_input_frame() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("pty.sock");
        let listener = UnixListener::bind(&sock).unwrap();
        let host = std::thread::spawn(move || {
            let (mut conn, _) = listener.accept().unwrap();
            let mut wire = Vec::new();
            let mut head = [0u8; 5];
            conn.read_exact(&mut head).unwrap();
            let mut auth = vec![0u8; u32::from_be_bytes(head[..4].try_into().unwrap()) as usize];
            conn.read_exact(&mut auth).unwrap();
            wire.extend_from_slice(&head);
            wire.extend(auth);
            conn.write_all(&frame(KIND_OUTPUT, b"replay")).unwrap();
            conn.shutdown(std::net::Shutdown::Write).unwrap();
            conn.read_to_end(&mut wire).unwrap();
            wire
        });
        let worker = Worker {
            pty_sock: sock,
            pty_auth: "tok".into(),
        };
        type_input(&worker, b"/clear").unwrap();
        let wire = host.join().unwrap();
        let auth = br#"{"t":"auth","token":"tok"}"#;
        let expected = [frame(KIND_CONTROL, auth), frame(KIND_INPUT, b"/clear")].concat();
        assert_eq!(wire, expected);
    }

    /// One-connection host: records the auth frame, runs `serve`, then records every
    /// byte the client sends until it hangs up.
    fn host(
        serve: impl FnOnce(&mut UnixStream) + Send + 'static,
    ) -> (Worker, std::thread::JoinHandle<Vec<u8>>, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("pty.sock");
        let listener = UnixListener::bind(&sock).unwrap();
        let handle = std::thread::spawn(move || {
            let (mut conn, _) = listener.accept().unwrap();
            let mut head = [0u8; 5];
            conn.read_exact(&mut head).unwrap();
            let mut auth = vec![0u8; u32::from_be_bytes(head[..4].try_into().unwrap()) as usize];
            conn.read_exact(&mut auth).unwrap();
            serve(&mut conn);
            let mut rest = Vec::new();
            conn.read_to_end(&mut rest).unwrap();
            rest
        });
        let worker = Worker {
            pty_sock: sock,
            pty_auth: "tok".into(),
        };
        (worker, handle, dir)
    }

    #[test]
    fn a_quiet_whole_replay_that_passes_the_check_gets_the_keys() {
        let (worker, h, _dir) = host(|c| c.write_all(&frame(KIND_OUTPUT, b"box")).unwrap());
        assert!(type_if(&worker, b"\r", |s| s == b"box").unwrap());
        assert_eq!(h.join().unwrap(), frame(KIND_INPUT, b"\r"));
    }

    #[test]
    fn a_hangup_after_the_replay_gets_no_keys() {
        let (worker, h, _dir) = host(|c| {
            c.write_all(&frame(KIND_OUTPUT, b"box")).unwrap();
            c.shutdown(std::net::Shutdown::Write).unwrap();
        });
        assert!(!type_if(&worker, b"\r", |_| true).unwrap());
        assert!(h.join().unwrap().is_empty());
    }

    #[test]
    fn a_cut_frame_gets_no_keys() {
        let (worker, h, _dir) = host(|c| {
            c.write_all(&frame(KIND_OUTPUT, b"box")).unwrap();
            c.write_all(&[0, 0, 0, 9, KIND_OUTPUT, b'r', b'e']).unwrap();
        });
        assert!(!type_if(&worker, b"\r", |_| true).unwrap());
        assert!(h.join().unwrap().is_empty());
    }

    #[test]
    fn output_arriving_while_the_check_runs_gets_no_keys() {
        let (go, wait) = std::sync::mpsc::channel::<()>();
        let (done, painted) = std::sync::mpsc::channel::<()>();
        let (worker, h, _dir) = host(move |c| {
            c.write_all(&frame(KIND_OUTPUT, b"box")).unwrap();
            wait.recv().unwrap();
            c.write_all(&frame(KIND_OUTPUT, b"x")).unwrap();
            done.send(()).unwrap();
        });
        let typed = type_if(&worker, b"\r", |_| {
            go.send(()).unwrap();
            painted.recv().unwrap();
            true
        });
        assert!(!typed.unwrap());
        assert!(h.join().unwrap().is_empty());
    }

    #[test]
    fn output_frames_are_kept_and_control_frames_dropped() {
        let mut wire = frame(KIND_CONTROL, br#"{"t":"hello"}"#);
        wire.extend(frame(KIND_OUTPUT, b"\x1b[64;1H"));
        wire.extend(frame(KIND_CONTROL, br#"{"t":"ping"}"#));
        wire.extend(frame(KIND_OUTPUT, b"\xe2\x9d\xaf"));
        assert_eq!(output_of(&wire), b"\x1b[64;1H\xe2\x9d\xaf".to_vec());
    }

    #[test]
    fn a_length_that_counted_the_kind_byte_would_misalign_everything() {
        // The bug this cost a session: treating the prefix as inclusive drops
        // the payload's last byte and starts the next frame one byte early.
        let wire = [
            frame(KIND_CONTROL, br#"{"t":"hello"}"#),
            frame(KIND_OUTPUT, b"OK"),
        ]
        .concat();
        assert_eq!(output_of(&wire), b"OK".to_vec());
    }

    #[test]
    fn a_truncated_trailing_frame_is_ignored() {
        let mut wire = frame(KIND_OUTPUT, b"full");
        wire.extend_from_slice(&[0, 0, 0, 9, KIND_OUTPUT, b'c', b'u', b't']);
        assert_eq!(output_of(&wire), b"full".to_vec());
    }
}
