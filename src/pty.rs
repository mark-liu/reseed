//! The daemon's per-worker pty socket, read-only.
//!
//! One auth frame and the daemon replays that session's rendered screen back.
//! Nothing is ever written to the session: on a quiet session the replay burst
//! is followed by no further traffic, so the read leaves no trace. Attaching a
//! real client is the opposite - it carries a window size and resizes the
//! session's pty - which is why this speaks the socket directly.

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::collections::HashMap;
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// Frame kinds on the wire. The u32 length prefix counts the payload only and
/// excludes the kind byte; reading it as inclusive misaligns every frame after
/// the first and silently yields an empty screen.
const KIND_OUTPUT: u8 = 0;
const KIND_CONTROL: u8 = 1;

const READ_TIMEOUT: Duration = Duration::from_secs(2);
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

/// The rendered screen for one job, as the concatenated output frames.
pub fn read_screen(worker: &Worker) -> Result<Vec<u8>> {
    let mut sock = UnixStream::connect(&worker.pty_sock)
        .with_context(|| format!("connecting {}", worker.pty_sock.display()))?;
    sock.set_read_timeout(Some(READ_TIMEOUT))?;
    sock.set_write_timeout(Some(READ_TIMEOUT))?;

    let auth = serde_json::json!({ "t": "auth", "token": worker.pty_auth }).to_string();
    let mut frame = Vec::with_capacity(5 + auth.len());
    frame.extend_from_slice(&(auth.len() as u32).to_be_bytes());
    frame.push(KIND_CONTROL);
    frame.extend_from_slice(auth.as_bytes());
    sock.write_all(&frame).context("sending the auth frame")?;

    let mut buf = Vec::new();
    let mut chunk = [0u8; 65536];
    let deadline = Instant::now() + BURST;
    while Instant::now() < deadline {
        match sock.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                buf.extend_from_slice(&chunk[..n]);
                if buf.len() > MAX_STREAM {
                    bail!("screen replay exceeded {MAX_STREAM} bytes");
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
            Err(e) if e.kind() == std::io::ErrorKind::TimedOut => break,
            Err(e) => return Err(e).context("reading the screen replay"),
        }
    }
    Ok(output_of(&buf))
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

    fn frame(kind: u8, body: &[u8]) -> Vec<u8> {
        let mut f = (body.len() as u32).to_be_bytes().to_vec();
        f.push(kind);
        f.extend_from_slice(body);
        f
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
