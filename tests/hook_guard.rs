//! The cases of `test_context_reset_guard.py`, transcribed against
//! `reseed hook guard`. Isolation: HOME points at a tmp dir per test.

use serde_json::{json, Value};
use std::fs;
use std::path::Path;
use std::process::Command;
use std::time::{Duration, SystemTime};

const SESSION: &str = "sess-1";

fn bin() -> Command {
    Command::new(env!("CARGO_BIN_EXE_reseed"))
}

fn transcript(
    home: &Path,
    ctx_tokens: u64,
    reply: &str,
    model: &str,
    sidechain_reply: Option<&str>,
    usage: bool,
) -> std::path::PathBuf {
    let path = home.join("t.jsonl");
    let mut entry = json!({
        "type": "assistant",
        "message": {"model": model, "content": [{"type": "text", "text": reply}]},
    });
    if usage {
        entry["message"]["usage"] = json!({"input_tokens": ctx_tokens});
    }
    let mut lines = vec![entry.to_string()];
    if let Some(sr) = sidechain_reply {
        lines.push(
            json!({
                "type": "assistant", "isSidechain": true,
                "message": {"model": model, "content": [{"type": "text", "text": sr}],
                            "usage": {"input_tokens": ctx_tokens}},
            })
            .to_string(),
        );
    }
    fs::write(&path, lines.join("\n") + "\n").unwrap();
    path
}

fn run(
    home: &Path,
    transcript: Option<&Path>,
    session: &str,
    stop_hook_active: bool,
) -> (i32, String) {
    let mut payload = json!({"session_id": session, "stop_hook_active": stop_hook_active});
    if let Some(t) = transcript {
        payload["transcript_path"] = json!(t.to_str().unwrap());
    }
    let output = bin()
        .args(["hook", "guard"])
        .env("HOME", home)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .and_then(|mut c| {
            use std::io::Write;
            c.stdin
                .take()
                .unwrap()
                .write_all(payload.to_string().as_bytes())?;
            c.wait_with_output()
        })
        .unwrap();
    (
        output.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&output.stderr).to_string(),
    )
}

fn state(home: &Path, session: &str) -> Option<Value> {
    let p = home
        .join(".claude/cache/context-reset-guard")
        .join(format!("{session}.json"));
    fs::read_to_string(p)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
}

// --- core delivery contract --------------------------------------------

#[test]
fn test_below_line_is_silent() {
    let tmp = tempfile::tempdir().unwrap();
    let t = transcript(tmp.path(), 120_000, "all done", "claude-opus-5", None, true);
    assert_eq!(
        run(tmp.path(), Some(&t), SESSION, false),
        (0, String::new())
    );
}

#[test]
fn test_past_line_with_no_mention_blocks() {
    let tmp = tempfile::tempdir().unwrap();
    let t = transcript(
        tmp.path(),
        317_000,
        "done: rebuilt\nnext: your call",
        "claude-opus-5",
        None,
        true,
    );
    let (rc, err) = run(tmp.path(), Some(&t), SESSION, false);
    assert_eq!(rc, 2);
    assert!(err.contains("RESET NOW (317k)"));
    assert!(err.contains("past the 300k reset line"));
}

#[test]
fn test_reply_that_already_says_clear_costs_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let t = transcript(
        tmp.path(),
        317_000,
        "done: x\nRESET NOW: /clear then go",
        "claude-opus-5",
        None,
        true,
    );
    assert_eq!(
        run(tmp.path(), Some(&t), SESSION, false),
        (0, String::new())
    );
    assert_eq!(state(tmp.path(), SESSION).unwrap(), json!({"tier": 1}));
}

#[test]
fn test_reseed_here_also_counts_as_delivered() {
    let tmp = tempfile::tempdir().unwrap();
    let t = transcript(
        tmp.path(),
        317_000,
        "run `! reseed-here` when you sit down",
        "claude-opus-5",
        None,
        true,
    );
    assert_eq!(run(tmp.path(), Some(&t), SESSION, false).0, 0);
}

// --- never loop, never nag ------------------------------------------------

#[test]
fn test_stop_hook_active_never_blocks() {
    let tmp = tempfile::tempdir().unwrap();
    let t = transcript(
        tmp.path(),
        317_000,
        "still nothing about resetting",
        "claude-opus-5",
        None,
        true,
    );
    assert_eq!(run(tmp.path(), Some(&t), SESSION, true), (0, String::new()));
}

#[test]
fn test_same_tier_only_blocks_once() {
    let tmp = tempfile::tempdir().unwrap();
    let t = transcript(
        tmp.path(),
        317_000,
        "nothing here",
        "claude-opus-5",
        None,
        true,
    );
    assert_eq!(run(tmp.path(), Some(&t), SESSION, false).0, 2);
    assert_eq!(
        run(tmp.path(), Some(&t), SESSION, false),
        (0, String::new())
    );
}

#[test]
fn test_next_tier_escalates() {
    let tmp = tempfile::tempdir().unwrap();
    let t1 = transcript(tmp.path(), 317_000, "nope", "claude-opus-5", None, true);
    assert_eq!(run(tmp.path(), Some(&t1), SESSION, false).0, 2);
    let t2 = transcript(tmp.path(), 390_000, "nope", "claude-opus-5", None, true);
    let (rc, err) = run(tmp.path(), Some(&t2), SESSION, false);
    assert_eq!(rc, 2);
    assert!(err.contains("RESET NOW (390k)"));
    assert_eq!(state(tmp.path(), SESSION).unwrap(), json!({"tier": 2}));
}

#[test]
fn test_drop_below_line_rearms_for_the_same_session_id() {
    let tmp = tempfile::tempdir().unwrap();
    let t1 = transcript(tmp.path(), 317_000, "nope", "claude-opus-5", None, true);
    assert_eq!(run(tmp.path(), Some(&t1), SESSION, false).0, 2);
    let t2 = transcript(tmp.path(), 40_000, "ok", "claude-opus-5", None, true);
    assert_eq!(run(tmp.path(), Some(&t2), SESSION, false).0, 0);
    assert_eq!(state(tmp.path(), SESSION).unwrap(), json!({"tier": 0}));
    let t3 = transcript(tmp.path(), 317_000, "nope", "claude-opus-5", None, true);
    assert_eq!(run(tmp.path(), Some(&t3), SESSION, false).0, 2);
}

// --- what counts as "the reply" -------------------------------------------

#[test]
fn test_subagent_prose_is_not_the_reply() {
    let tmp = tempfile::tempdir().unwrap();
    let t = transcript(
        tmp.path(),
        317_000,
        "done: swept",
        "claude-opus-5",
        Some("recommend /clear now"),
        true,
    );
    assert_eq!(run(tmp.path(), Some(&t), SESSION, false).0, 2);
}

// --- window + arming variants ----------------------------------------------

#[test]
fn test_small_window_uses_the_capped_line() {
    let tmp = tempfile::tempdir().unwrap();
    let t = transcript(tmp.path(), 160_000, "nope", "claude-haiku-4-5", None, true);
    let (rc, err) = run(tmp.path(), Some(&t), SESSION, false);
    assert_eq!(rc, 2);
    assert!(err.contains("past the 150k reset line"));
}

#[test]
fn test_armed_sentinel_drops_the_manual_distill_step() {
    let tmp = tempfile::tempdir().unwrap();
    let pending = tmp.path().join(".claude/reseed/pending");
    fs::create_dir_all(&pending).unwrap();
    fs::write(pending.join(SESSION), "bundle").unwrap();
    let t = transcript(tmp.path(), 317_000, "nope", "claude-opus-5", None, true);
    let (rc, err) = run(tmp.path(), Some(&t), SESSION, false);
    assert_eq!(rc, 2);
    assert!(err.contains("already distilled and armed"));
    assert!(!err.contains("! reseed-here"));
}

#[test]
fn test_stale_sentinel_falls_back_to_manual_distill() {
    let tmp = tempfile::tempdir().unwrap();
    let pending = tmp.path().join(".claude/reseed/pending");
    fs::create_dir_all(&pending).unwrap();
    let sentinel = pending.join(SESSION);
    fs::write(&sentinel, "bundle").unwrap();
    let old = SystemTime::now() - Duration::from_secs(3600);
    fs::File::options()
        .write(true)
        .open(&sentinel)
        .unwrap()
        .set_modified(old)
        .unwrap();
    let t = transcript(tmp.path(), 317_000, "nope", "claude-opus-5", None, true);
    let (rc, err) = run(tmp.path(), Some(&t), SESSION, false);
    assert_eq!(rc, 2);
    assert!(err.contains("! reseed-here"));
}

// --- fail-open contract -----------------------------------------------------

#[test]
fn test_unknown_size_is_not_tier_zero_and_is_silent() {
    let tmp = tempfile::tempdir().unwrap();
    let t = transcript(tmp.path(), 317_000, "nope", "claude-opus-5", None, false);
    assert_eq!(
        run(tmp.path(), Some(&t), SESSION, false),
        (0, String::new())
    );
    assert!(state(tmp.path(), SESSION).is_none());
}

#[test]
fn test_missing_transcript_path_is_silent() {
    let tmp = tempfile::tempdir().unwrap();
    assert_eq!(run(tmp.path(), None, SESSION, false), (0, String::new()));
}

#[test]
fn test_malformed_stdin_is_silent() {
    let tmp = tempfile::tempdir().unwrap();
    let output = bin()
        .args(["hook", "guard"])
        .env("HOME", tmp.path())
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .and_then(|mut c| {
            use std::io::Write;
            c.stdin.take().unwrap().write_all(b"not json")?;
            c.wait_with_output()
        })
        .unwrap();
    assert_eq!(output.status.code(), Some(0));
    assert!(output.stderr.is_empty());
}

#[test]
fn test_nonexistent_transcript_is_silent() {
    let tmp = tempfile::tempdir().unwrap();
    let missing = tmp.path().join("missing.jsonl");
    assert_eq!(
        run(tmp.path(), Some(&missing), SESSION, false),
        (0, String::new())
    );
}
