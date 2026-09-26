//! The daemon's per-worker pty socket.
//!
//! One auth frame and the daemon replays that session's rendered screen back.
//! A read writes nothing: on a quiet session the replay burst is followed by no
//! further traffic, so it leaves no trace. Attaching a real client is the
//! opposite - it carries a window size and resizes the session's pty - which is
//! why this speaks the socket directly. `type_input` is the one write: raw
//! keystrokes, as an attached client sends them.
//!
//! The replay alone cannot say how wide the terminal is, and the render needs
//! to know: the roster names the REPL the daemon spawned, its controlling tty
//! answers `TIOCGWINSZ`, and the size is sampled on both sides of every replay.

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::collections::HashMap;
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
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
    /// The REPL the daemon spawned, and when (UTC, `ps lstart` shape): together
    /// they name one process, where the pid alone names whoever holds it today.
    #[serde(rename = "replPid", default)]
    pub repl_pid: Option<u32>,
    #[serde(rename = "replProcStart", default)]
    pub repl_proc_start: Option<String>,
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

/// One read of a session's screen: the output frames, and the pty's (rows, cols)
/// that held across the read. Without it the render guesses from the stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Replay {
    pub stream: Vec<u8>,
    pub size: Option<(u16, u16)>,
}

fn frame(kind: u8, body: &[u8]) -> Vec<u8> {
    let mut f = Vec::with_capacity(5 + body.len());
    f.extend_from_slice(&(body.len() as u32).to_be_bytes());
    f.push(kind);
    f.extend_from_slice(body);
    f
}

/// The connection after the auth frame: the raw replay burst, and whether the
/// socket then went quiet rather than still flowing at the deadline.
struct Opened {
    sock: UnixStream,
    raw: Vec<u8>,
    quiet: bool,
}

fn open(worker: &Worker) -> Result<Opened> {
    let mut sock = UnixStream::connect(&worker.pty_sock)
        .with_context(|| format!("connecting {}", worker.pty_sock.display()))?;
    sock.set_read_timeout(Some(READ_TIMEOUT))?;
    sock.set_write_timeout(Some(READ_TIMEOUT))?;

    let auth = serde_json::json!({ "t": "auth", "token": worker.pty_auth }).to_string();
    sock.write_all(&frame(KIND_CONTROL, auth.as_bytes()))
        .context("sending the auth frame")?;
    let (raw, quiet) = drain(&mut sock)?;
    Ok(Opened { sock, raw, quiet })
}

/// The rendered screen for one job, with the pty's size when the roster names
/// its REPL. The size is sampled before and after the replay: a resize between
/// them pairs bytes with a geometry they were not drawn for, so that read fails.
pub fn read_screen(worker: &Worker) -> Result<Replay> {
    read_with(worker, Tty::of(worker)?)
}

fn read_with(worker: &Worker, tty: Option<Tty>) -> Result<Replay> {
    let before = sample(tty.as_ref())?;
    let opened = open(worker)?;
    let after = sample(tty.as_ref())?;
    if before != after {
        bail!("the pty resized during the read: {before:?} then {after:?}");
    }
    replay_of(worker, &opened.raw, after)
}

/// The output frames with the size they were measured at. The hello is the
/// connection's own word on which REPL it serves, and it must be the roster's.
fn replay_of(worker: &Worker, raw: &[u8], size: Option<(u16, u16)>) -> Result<Replay> {
    if let (Some(hello), Some(pid)) = (repl_pid_of(raw), worker.repl_pid) {
        if hello != pid {
            bail!("the hello names REPL pid {hello}, the roster {pid}");
        }
    }
    Ok(Replay {
        stream: output_of(raw),
        size,
    })
}

/// Type `keys` into the session, exactly as if a human pressed them. Nothing
/// checks the box first; callers read it before and after.
pub fn type_input(worker: &Worker, keys: &[u8]) -> Result<()> {
    let mut opened = open(worker)?;
    send(&mut opened.sock, keys)
}

/// Type `keys` only if this connection's own replay went quiet and passes `check`,
/// so the proof and the keystrokes share one socket. Returns whether it typed.
pub fn type_if(worker: &Worker, keys: &[u8], check: impl FnOnce(&Replay) -> bool) -> Result<bool> {
    type_if_with(worker, keys, check, Tty::of(worker)?)
}

fn type_if_with(
    worker: &Worker,
    keys: &[u8],
    check: impl FnOnce(&Replay) -> bool,
    tty: Option<Tty>,
) -> Result<bool> {
    let before = sample(tty.as_ref())?;
    let Opened {
        mut sock,
        raw,
        quiet,
    } = open(worker)?;
    let size = sample(tty.as_ref())?;
    // Any repaint reaches this socket, so silence leaves only a keystroke in
    // flight; a resize is a repaint still on its way.
    if !quiet || !whole_frames(&raw) || before != size {
        return Ok(false);
    }
    if !check(&replay_of(worker, &raw, size)?) {
        return Ok(false);
    }
    // Output queued while `check` parsed means the screen moved under the proof,
    // and so does a resize since, whose repaint is not queued yet.
    if arrived_since(&mut sock)? || sample(tty.as_ref())? != size {
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

/// The whole frames in `buf` as (kind, payload); a cut trailing frame is left out.
fn frames(buf: &[u8]) -> impl Iterator<Item = (u8, &[u8])> {
    let mut i = 0;
    std::iter::from_fn(move || {
        if i + 5 > buf.len() {
            return None;
        }
        let n = u32::from_be_bytes([buf[i], buf[i + 1], buf[i + 2], buf[i + 3]]) as usize;
        let body = i + 5;
        if body + n > buf.len() {
            return None;
        }
        i = body + n;
        Some((buf[body - 1], &buf[body..body + n]))
    })
}

/// Concatenate the output frames, dropping the daemon's control chatter.
pub fn output_of(buf: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(buf.len());
    for (kind, body) in frames(buf) {
        if kind == KIND_OUTPUT {
            out.extend_from_slice(body);
        }
    }
    out
}

/// The REPL's pid from the daemon's hello frame (CC 2.1.283 `{"t":"hello",
/// "replPid":N}`): the process whose controlling terminal is the session's pty.
pub fn repl_pid_of(buf: &[u8]) -> Option<u32> {
    frames(buf)
        .filter(|(kind, _)| *kind == KIND_CONTROL)
        .find_map(|(_, body)| {
            let v: serde_json::Value = serde_json::from_slice(body).ok()?;
            if v.get("t")?.as_str()? != "hello" {
                return None;
            }
            u32::try_from(v.get("replPid")?.as_u64()?).ok()
        })
}

/// The REPL's controlling tty, held open across one read's size samples. The
/// stream only bounds the width from below, and a titled session's top rule is
/// short of the width by its title, so a render at that floor wraps the rule
/// and drops the box a row under the caret.
#[derive(Debug)]
struct Tty(std::fs::File);

impl Tty {
    /// `Ok(None)` when the roster names no REPL, so the render must guess. `Err`
    /// when it names one that cannot be proven live on a tty: the pid must still
    /// carry the roster's start time, or the number has been reused since.
    fn of(worker: &Worker) -> Result<Option<Tty>> {
        let (Some(pid), Some(started)) = (worker.repl_pid, worker.repl_proc_start.as_deref())
        else {
            return Ok(None);
        };
        let ps = |field| crate::identity::ps_field(pid, field).context("running ps");
        let live = ps("lstart")?;
        if live != started {
            bail!("REPL pid {pid} started {live:?}, the roster says {started:?}");
        }
        let tty = ps("tty")?;
        if tty.is_empty() || tty.starts_with('?') {
            bail!("REPL pid {pid} has no controlling tty");
        }
        // O_NOCTTY: another session's pty must never become this process's terminal.
        let dev = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOCTTY | libc::O_NONBLOCK)
            .open(format!("/dev/{tty}"))
            .with_context(|| format!("opening /dev/{tty}"))?;
        Ok(Some(Tty(dev)))
    }

    /// `TIOCGWINSZ`: the pty's (rows, cols) right now.
    fn size(&self) -> Result<(u16, u16)> {
        let mut ws = libc::winsize {
            ws_row: 0,
            ws_col: 0,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        // SAFETY: TIOCGWINSZ fills one winsize behind a valid pointer and nothing else.
        let rc = unsafe { libc::ioctl(self.0.as_raw_fd(), libc::TIOCGWINSZ, &mut ws) };
        if rc != 0 {
            bail!("TIOCGWINSZ: {}", std::io::Error::last_os_error());
        }
        Ok((ws.ws_row, ws.ws_col))
    }
}

/// The pty's size now, when there is a tty to ask.
fn sample(tty: Option<&Tty>) -> Result<Option<(u16, u16)>> {
    tty.map(Tty::size).transpose()
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
        let worker = unnamed(sock);
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
        (unnamed(sock), handle, dir)
    }

    /// A worker whose roster row names no REPL, as before CC 2.1.283.
    fn unnamed(sock: PathBuf) -> Worker {
        Worker {
            pty_sock: sock,
            pty_auth: "tok".into(),
            repl_pid: None,
            repl_proc_start: None,
        }
    }

    /// A worker whose roster row names a REPL by pid and start time.
    fn named(pid: u32, started: &str) -> Worker {
        Worker {
            repl_pid: Some(pid),
            repl_proc_start: Some(started.into()),
            ..unnamed("/nonexistent".into())
        }
    }

    /// A real pty pair, so a test can resize the terminal a read is measuring.
    fn pty_pair() -> (std::sync::Arc<std::fs::File>, Tty) {
        use std::os::fd::FromRawFd;
        let (mut master, mut slave) = (0, 0);
        // SAFETY: openpty writes two fds behind valid pointers; name, termios and
        // winsize are optional and left null.
        let rc = unsafe {
            libc::openpty(
                &mut master,
                &mut slave,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        assert_eq!(rc, 0, "openpty");
        // SAFETY: both fds were just opened and nothing else owns them.
        let (master, slave) = unsafe {
            (
                std::fs::File::from_raw_fd(master),
                std::fs::File::from_raw_fd(slave),
            )
        };
        (std::sync::Arc::new(master), Tty(slave))
    }

    fn resize(master: &std::fs::File, rows: u16, cols: u16) {
        let ws = libc::winsize {
            ws_row: rows,
            ws_col: cols,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        // SAFETY: TIOCSWINSZ reads one winsize behind a valid pointer.
        let rc = unsafe { libc::ioctl(master.as_raw_fd(), libc::TIOCSWINSZ, &ws) };
        assert_eq!(rc, 0, "TIOCSWINSZ");
    }

    #[test]
    fn a_quiet_whole_replay_that_passes_the_check_gets_the_keys() {
        let (worker, h, _dir) = host(|c| c.write_all(&frame(KIND_OUTPUT, b"box")).unwrap());
        assert!(type_if(&worker, b"\r", |r| r.stream == b"box").unwrap());
        assert_eq!(h.join().unwrap(), frame(KIND_INPUT, b"\r"));
    }

    #[test]
    fn a_roster_that_names_no_repl_reads_with_no_size_whatever_the_hello_says() {
        let hello = br#"{"t":"hello","replPid":4294967295,"version":"2.1.283"}"#;
        let (worker, h, _dir) = host(|c| {
            c.write_all(&frame(KIND_CONTROL, hello)).unwrap();
            c.write_all(&frame(KIND_OUTPUT, b"box")).unwrap();
        });
        assert!(Tty::of(&worker).unwrap().is_none());
        let replay = read_screen(&worker).unwrap();
        assert_eq!(replay.stream, b"box");
        assert_eq!(replay.size, None);
        drop(replay);
        assert!(h.join().unwrap().is_empty());
    }

    #[test]
    fn a_pid_without_the_rosters_start_time_is_not_the_repl() {
        // This process is alive, but it did not start when the roster says.
        let reused = named(std::process::id(), "Thu Jan  1 00:00:00 1970");
        let err = Tty::of(&reused).unwrap_err().to_string();
        assert!(err.contains("started"), "{err}");
        // A pid no process holds.
        assert!(Tty::of(&named(u32::MAX, "Thu Jan  1 00:00:00 1970")).is_err());
    }

    #[test]
    fn a_hello_that_disagrees_with_the_roster_fails_the_read() {
        let wire = [
            frame(KIND_CONTROL, br#"{"t":"hello","replPid":55291}"#),
            frame(KIND_OUTPUT, b"box"),
        ]
        .concat();
        let err = replay_of(&named(1, "x"), &wire, None)
            .unwrap_err()
            .to_string();
        assert!(err.contains("hello names REPL pid 55291"), "{err}");
        assert_eq!(
            replay_of(&named(55291, "x"), &wire, Some((50, 200))).unwrap(),
            Replay {
                stream: b"box".to_vec(),
                size: Some((50, 200)),
            }
        );
    }

    #[test]
    fn a_steady_pty_gives_its_size_to_the_read_and_the_check() {
        let (master, tty) = pty_pair();
        resize(&master, 50, 200);
        let (worker, h, _dir) = host(|c| c.write_all(&frame(KIND_OUTPUT, b"box")).unwrap());
        let (_, tty2) = pty_pair();
        drop(tty2);
        assert_eq!(read_with(&worker, Some(tty)).unwrap().size, Some((50, 200)));
        assert!(h.join().unwrap().is_empty());

        let (master, tty) = pty_pair();
        resize(&master, 50, 200);
        let (worker, h, _dir) = host(|c| c.write_all(&frame(KIND_OUTPUT, b"box")).unwrap());
        let typed = type_if_with(
            &worker,
            b"\r",
            |r| r.size == Some((50, 200)) && r.stream == b"box",
            Some(tty),
        )
        .unwrap();
        assert!(typed);
        assert_eq!(h.join().unwrap(), frame(KIND_INPUT, b"\r"));
    }

    #[test]
    fn a_resize_during_the_replay_fails_the_read() {
        let (master, tty) = pty_pair();
        resize(&master, 50, 200);
        let during = master.clone();
        let (worker, h, _dir) = host(move |c| {
            resize(&during, 60, 200);
            c.write_all(&frame(KIND_OUTPUT, b"box")).unwrap();
        });
        let err = read_with(&worker, Some(tty)).unwrap_err().to_string();
        assert!(err.contains("resized during the read"), "{err}");
        assert!(h.join().unwrap().is_empty());
    }

    #[test]
    fn a_resize_during_the_check_gets_no_keys() {
        let (master, tty) = pty_pair();
        resize(&master, 50, 200);
        let (worker, h, _dir) = host(|c| c.write_all(&frame(KIND_OUTPUT, b"box")).unwrap());
        let typed = type_if_with(
            &worker,
            b"\r",
            |r| {
                assert_eq!(r.size, Some((50, 200)));
                resize(&master, 51, 200);
                true
            },
            Some(tty),
        )
        .unwrap();
        assert!(!typed);
        assert!(h.join().unwrap().is_empty());
    }

    #[test]
    fn the_repl_pid_comes_from_the_hello_control_frame_alone() {
        let wire = [
            frame(KIND_OUTPUT, br#"{"t":"hello","replPid":1}"#),
            frame(KIND_CONTROL, br#"{"t":"live"}"#),
            frame(
                KIND_CONTROL,
                br#"{"t":"hello","replPid":55291,"version":"2.1.283"}"#,
            ),
        ]
        .concat();
        assert_eq!(repl_pid_of(&wire), Some(55291));
        assert_eq!(repl_pid_of(&frame(KIND_CONTROL, br#"{"t":"ping"}"#)), None);
        assert_eq!(repl_pid_of(b""), None);
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
