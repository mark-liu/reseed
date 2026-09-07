//! The cases of `test_context_reset_halt.py`, transcribed against
//! `reseed hook halt` and `reseed hook override`.

use serde_json::{json, Value};
use std::fs;
use std::path::Path;
use std::process::Command;
use std::time::SystemTime;

const SESSION: &str = "sess-1";

fn bin() -> Command {
    Command::new(env!("CARGO_BIN_EXE_reseed"))
}

fn transcript(home: &Path, ctx_tokens: u64, model: &str, usage: bool) -> std::path::PathBuf {
    let path = home.join("t.jsonl");
    let mut msg = json!({"model": model, "content": [{"type": "text", "text": "hi"}]});
    if usage {
        msg["usage"] = json!({"input_tokens": ctx_tokens});
    }
    fs::write(
        &path,
        json!({"type": "assistant", "message": msg}).to_string() + "\n",
    )
    .unwrap();
    path
}

fn run(hook: &str, home: &Path, payload: &Value, extra_env: &[(&str, &str)]) -> (i32, String) {
    let mut cmd = bin();
    cmd.args(["hook", hook])
        .env("HOME", home)
        .env_remove("CLAUDE_NO_RESET_HALT")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null());
    for (k, v) in extra_env {
        cmd.env(k, v);
    }
    let output = cmd
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
        String::from_utf8_lossy(&output.stdout).trim().to_string(),
    )
}

fn halt(
    home: &Path,
    transcript: Option<&Path>,
    session: &str,
    extra_env: &[(&str, &str)],
) -> (i32, String) {
    let mut payload = json!({"session_id": session});
    if let Some(t) = transcript {
        payload["transcript_path"] = json!(t.to_str().unwrap());
    }
    run("halt", home, &payload, extra_env)
}

fn decision(stdout: &str) -> Option<String> {
    if stdout.is_empty() {
        return None;
    }
    let v: Value = serde_json::from_str(stdout).unwrap();
    v["hookSpecificOutput"]["permissionDecision"]
        .as_str()
        .map(String::from)
}

fn arm(home: &Path, tier: i64, ttl: i64, session: &str) {
    let d = home.join(".claude/cache/context-reset-guard");
    fs::create_dir_all(&d).unwrap();
    let now = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_secs_f64();
    let body = json!({"tier": tier, "expires": now + ttl as f64});
    fs::write(d.join(format!("{session}.override.json")), body.to_string()).unwrap();
}

fn marker(home: &Path, session: &str) -> Option<Value> {
    let p = home
        .join(".claude/cache/context-reset-guard")
        .join(format!("{session}.override.json"));
    fs::read_to_string(p)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
}

// --- the halt itself --------------------------------------------------------

#[test]
fn test_under_the_line_allows() {
    let tmp = tempfile::tempdir().unwrap();
    let t = transcript(tmp.path(), 120_000, "claude-opus-5", true);
    assert_eq!(halt(tmp.path(), Some(&t), SESSION, &[]), (0, String::new()));
}

#[test]
fn test_past_the_line_denies_with_the_way_out() {
    let tmp = tempfile::tempdir().unwrap();
    let t = transcript(tmp.path(), 317_000, "claude-opus-5", true);
    let (rc, out) = halt(tmp.path(), Some(&t), SESSION, &[]);
    assert_eq!(rc, 0);
    assert_eq!(decision(&out).as_deref(), Some("deny"));
    let reason = serde_json::from_str::<Value>(&out).unwrap()["hookSpecificOutput"]
        ["permissionDecisionReason"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(reason.contains("HALTED: context is ~317k, past the 300k reset line"));
    assert!(reason.contains("/clear") && reason.contains("override reset"));
}

#[test]
fn test_subagent_call_past_the_line_allows() {
    let tmp = tempfile::tempdir().unwrap();
    let t = transcript(tmp.path(), 317_000, "claude-opus-5", true);
    let payload = json!({
        "session_id": SESSION,
        "transcript_path": t.to_str().unwrap(),
        "agent_id": "a3abd7b9b742722ca",
        "agent_type": "general-purpose",
    });
    assert_eq!(run("halt", tmp.path(), &payload, &[]), (0, String::new()));
}

#[test]
fn test_agent_type_alone_is_not_a_subagent() {
    let tmp = tempfile::tempdir().unwrap();
    let t = transcript(tmp.path(), 317_000, "claude-opus-5", true);
    let payload = json!({
        "session_id": SESSION,
        "transcript_path": t.to_str().unwrap(),
        "agent_type": "general-purpose",
    });
    let (_, out) = run("halt", tmp.path(), &payload, &[]);
    assert_eq!(decision(&out).as_deref(), Some("deny"));
}

#[test]
fn test_reason_asks_for_a_title_in_text_not_a_tool_call() {
    let tmp = tempfile::tempdir().unwrap();
    let t = transcript(tmp.path(), 317_000, "claude-opus-5", true);
    let (_, out) = halt(tmp.path(), Some(&t), SESSION, &[]);
    let reason = serde_json::from_str::<Value>(&out).unwrap()["hookSpecificOutput"]
        ["permissionDecisionReason"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(reason.contains("suggested title:"));
    assert!(reason.contains("do not route around it"));
}

#[test]
fn test_small_window_halts_at_the_capped_line() {
    let tmp = tempfile::tempdir().unwrap();
    let t = transcript(tmp.path(), 160_000, "claude-haiku-4-5", true);
    let (_, out) = halt(tmp.path(), Some(&t), SESSION, &[]);
    assert_eq!(decision(&out).as_deref(), Some("deny"));
    assert!(out.contains("past the 150k reset line"));
}

// --- everything that must NOT halt ------------------------------------------

#[test]
fn test_unknown_size_allows() {
    let tmp = tempfile::tempdir().unwrap();
    let t = transcript(tmp.path(), 317_000, "claude-opus-5", false);
    assert_eq!(halt(tmp.path(), Some(&t), SESSION, &[]), (0, String::new()));
}

#[test]
fn test_missing_transcript_allows() {
    let tmp = tempfile::tempdir().unwrap();
    assert_eq!(halt(tmp.path(), None, SESSION, &[]), (0, String::new()));
}

#[test]
fn test_nonexistent_transcript_allows() {
    let tmp = tempfile::tempdir().unwrap();
    let missing = tmp.path().join("gone.jsonl");
    assert_eq!(
        halt(tmp.path(), Some(&missing), SESSION, &[]),
        (0, String::new())
    );
}

#[test]
fn test_malformed_stdin_allows() {
    let tmp = tempfile::tempdir().unwrap();
    let output = bin()
        .args(["hook", "halt"])
        .env("HOME", tmp.path())
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .and_then(|mut c| {
            use std::io::Write;
            c.stdin.take().unwrap().write_all(b"not json")?;
            c.wait_with_output()
        })
        .unwrap();
    assert_eq!(output.status.code(), Some(0));
    assert!(String::from_utf8_lossy(&output.stdout).trim().is_empty());
}

#[test]
fn test_unattended_env_escape_allows() {
    let tmp = tempfile::tempdir().unwrap();
    let t = transcript(tmp.path(), 317_000, "claude-opus-5", true);
    assert_eq!(
        halt(
            tmp.path(),
            Some(&t),
            SESSION,
            &[("CLAUDE_NO_RESET_HALT", "1")]
        ),
        (0, String::new())
    );
}

// --- override scoping --------------------------------------------------------

#[test]
fn test_override_at_this_tier_allows() {
    let tmp = tempfile::tempdir().unwrap();
    arm(tmp.path(), 1, 3600, SESSION);
    let t = transcript(tmp.path(), 317_000, "claude-opus-5", true);
    assert_eq!(halt(tmp.path(), Some(&t), SESSION, &[]), (0, String::new()));
}

#[test]
fn test_override_does_not_cover_the_next_tier() {
    let tmp = tempfile::tempdir().unwrap();
    arm(tmp.path(), 1, 3600, SESSION);
    let t = transcript(tmp.path(), 390_000, "claude-opus-5", true);
    let (_, out) = halt(tmp.path(), Some(&t), SESSION, &[]);
    assert_eq!(decision(&out).as_deref(), Some("deny"));
}

#[test]
fn test_expired_override_halts_again() {
    let tmp = tempfile::tempdir().unwrap();
    arm(tmp.path(), 1, -10, SESSION);
    let t = transcript(tmp.path(), 317_000, "claude-opus-5", true);
    let (_, out) = halt(tmp.path(), Some(&t), SESSION, &[]);
    assert_eq!(decision(&out).as_deref(), Some("deny"));
}

#[test]
fn test_override_for_another_session_does_not_apply() {
    let tmp = tempfile::tempdir().unwrap();
    arm(tmp.path(), 1, 3600, "other");
    let t = transcript(tmp.path(), 317_000, "claude-opus-5", true);
    let (_, out) = halt(tmp.path(), Some(&t), SESSION, &[]);
    assert_eq!(decision(&out).as_deref(), Some("deny"));
}

// --- reset-override.py: only the operator can lift it ------------------------

#[test]
fn test_phrase_past_the_line_arms_this_tier() {
    let tmp = tempfile::tempdir().unwrap();
    let t = transcript(tmp.path(), 317_000, "claude-opus-5", true);
    let payload = json!({
        "session_id": SESSION,
        "transcript_path": t.to_str().unwrap(),
        "prompt": "override reset, just finish this one",
    });
    let (rc, out) = run("override", tmp.path(), &payload, &[]);
    assert_eq!(rc, 0);
    assert_eq!(marker(tmp.path(), SESSION).unwrap()["tier"], 1);
    assert!(out.contains("LIFTED by the operator for this tier (317k"));
    assert_eq!(halt(tmp.path(), Some(&t), SESSION, &[]), (0, String::new()));
}

#[test]
fn test_no_phrase_arms_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let t = transcript(tmp.path(), 317_000, "claude-opus-5", true);
    let payload = json!({"session_id": SESSION, "transcript_path": t.to_str().unwrap(), "prompt": "carry on please"});
    assert_eq!(
        run("override", tmp.path(), &payload, &[]),
        (0, String::new())
    );
    assert!(marker(tmp.path(), SESSION).is_none());
}

#[test]
fn test_keep_going_is_deliberately_not_a_phrase() {
    let tmp = tempfile::tempdir().unwrap();
    let t = transcript(tmp.path(), 317_000, "claude-opus-5", true);
    let payload = json!({"session_id": SESSION, "transcript_path": t.to_str().unwrap(), "prompt": "keep going"});
    assert_eq!(
        run("override", tmp.path(), &payload, &[]),
        (0, String::new())
    );
    assert!(marker(tmp.path(), SESSION).is_none());
}

#[test]
fn test_phrase_under_the_line_arms_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let t = transcript(tmp.path(), 90_000, "claude-opus-5", true);
    let payload = json!({"session_id": SESSION, "transcript_path": t.to_str().unwrap(), "prompt": "override reset"});
    assert_eq!(
        run("override", tmp.path(), &payload, &[]),
        (0, String::new())
    );
    assert!(marker(tmp.path(), SESSION).is_none());
}

#[test]
fn test_override_malformed_stdin_is_silent() {
    let tmp = tempfile::tempdir().unwrap();
    let output = bin()
        .args(["hook", "override"])
        .env("HOME", tmp.path())
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .and_then(|mut c| {
            use std::io::Write;
            c.stdin.take().unwrap().write_all(b"{")?;
            c.wait_with_output()
        })
        .unwrap();
    assert_eq!(output.status.code(), Some(0));
    assert!(String::from_utf8_lossy(&output.stdout).trim().is_empty());
}
