//! The 18 cases of `test_context_reseed_nudge.py`, transcribed one for one
//! against `reseed hook nudge`. Isolation: each test points HOME at a tmp
//! dir, matching the Python suite's fixture pattern.

use serde_json::{json, Value};
use std::fs;
use std::path::Path;
use std::process::Command;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const REARM_INTERVAL_SECS: u64 = 480;

fn bin() -> Command {
    Command::new(env!("CARGO_BIN_EXE_reseed"))
}

fn transcript(
    home: &Path,
    session: &str,
    ctx_tokens: u64,
    model: &str,
    sidechain_ctx: Option<u64>,
) -> String {
    // Under .claude/projects/ so spawn::rearm's detached `arm --quiet` (which
    // resolves the transcript by session id, not by the hook's own payload) finds it.
    let dir = home.join(".claude/projects/proj");
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join(format!("{session}.jsonl"));
    let mut lines = vec![json!({"message": {"role": "user", "content": "go"}}).to_string()];
    if let Some(s) = sidechain_ctx {
        lines.push(
            json!({"type": "assistant", "isSidechain": true,
                   "message": {"model": model, "usage": {"input_tokens": s}}})
            .to_string(),
        );
    }
    lines.push(
        json!({"type": "assistant", "message": {"model": model, "usage": {
            "input_tokens": 1000,
            "cache_read_input_tokens": ctx_tokens - 1000,
            "cache_creation_input_tokens": 0,
        }}})
        .to_string(),
    );
    fs::write(&path, lines.join("\n") + "\n").unwrap();
    path.to_str().unwrap().to_string()
}

fn plant_fake_reseed_here(home: &Path) -> std::path::PathBuf {
    let bindir = home.join(".local/bin");
    fs::create_dir_all(&bindir).unwrap();
    let marker = home.join("spawns.txt");
    let script = bindir.join("reseed-here");
    fs::write(
        &script,
        "#!/bin/bash\necho \"$CLAUDE_CODE_SESSION_ID\" >> \"$MARKER\"\n",
    )
    .unwrap();
    // Not used by the Rust rearm (it re-execs the binary, not reseed-here);
    // kept only so path shape matches the Python fixture's isolation intent.
    let _ = script;
    marker
}

/// The Rust `spawn::rearm` re-execs `current_exe() arm --quiet`, not
/// `reseed-here`. Poll `pending/<key>` (the sentinel written by a real
/// arm) instead of a stub-script marker.
fn spawn_count(home: &Path, session: &str, expected: usize, timeout: Duration) -> usize {
    let path = home.join(".claude/reseed/pending").join(session);
    let deadline = std::time::Instant::now() + timeout;
    loop {
        let exists = path.exists();
        if (exists as usize) >= expected.min(1) {
            return 1;
        }
        if std::time::Instant::now() >= deadline {
            return if exists { 1 } else { 0 };
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn run(
    home: &Path,
    ctx_tokens: u64,
    session: &str,
    model: &str,
    sidechain_ctx: Option<u64>,
    extra: Option<Value>,
) -> (i32, Option<Value>) {
    let t = transcript(home, session, ctx_tokens, model, sidechain_ctx);
    let mut payload = json!({"session_id": session, "transcript_path": t});
    if let Some(Value::Object(map)) = extra {
        for (k, v) in map {
            payload[k] = v;
        }
    }
    let output = bin()
        .args(["hook", "nudge"])
        .env("HOME", home)
        .env_remove("CLAUDE_CODE_SESSION_ID")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
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
    let out = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (
        output.status.code().unwrap_or(-1),
        if out.is_empty() {
            None
        } else {
            Some(serde_json::from_str(&out).unwrap())
        },
    )
}

fn state(home: &Path, session: &str) -> Option<Value> {
    let p = home
        .join(".claude/cache/reseed-nudge")
        .join(format!("{session}.json"));
    fs::read_to_string(p)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
}

fn age_the_arm(home: &Path, session: &str, seconds: u64) {
    let p = home
        .join(".claude/cache/reseed-nudge")
        .join(format!("{session}.json"));
    let mut s: Value = serde_json::from_str(&fs::read_to_string(&p).unwrap()).unwrap();
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs_f64();
    s["armed_at"] = json!(now - seconds as f64);
    fs::write(&p, s.to_string()).unwrap();
}

fn arm_sentinel(home: &Path, session: &str, age_secs: Option<u64>) {
    let d = home.join(".claude/reseed/pending");
    fs::create_dir_all(&d).unwrap();
    let f = d.join(session);
    fs::write(&f, "Read .../narrative.md and continue.\n").unwrap();
    if let Some(age) = age_secs {
        let old = SystemTime::now() - Duration::from_secs(age);
        fs::File::options()
            .write(true)
            .open(&f)
            .unwrap()
            .set_modified(old)
            .unwrap();
    }
}

// --- Tiering ---------------------------------------------------------------

#[test]
fn test_below_tier1_is_silent() {
    // 200k, not the Python fixture's 250k: 250k sits inside the early band
    // (240k) added after that test was written, so the CURRENT script (the
    // parity contract per P2) already prints there; the Python test itself
    // fails against its own script at 250k, confirmed by running it here.
    let tmp = tempfile::tempdir().unwrap();
    plant_fake_reseed_here(tmp.path());
    let (rc, out) = run(tmp.path(), 200_000, "s1", "claude-opus-4-8", None, None);
    assert_eq!(rc, 0);
    assert!(out.is_none());
}

#[test]
fn test_tier1_crossing_nudges() {
    let tmp = tempfile::tempdir().unwrap();
    plant_fake_reseed_here(tmp.path());
    let (rc, out) = run(tmp.path(), 310_000, "s1", "claude-opus-4-8", None, None);
    assert_eq!(rc, 0);
    let out = out.unwrap();
    let msg = out["systemMessage"].as_str().unwrap();
    assert!(msg.contains("~310k"));
    assert!(msg.contains("300k"));
}

#[test]
fn test_small_window_model_still_uses_the_075_cap() {
    let tmp = tempfile::tempdir().unwrap();
    plant_fake_reseed_here(tmp.path());
    let (_, out) = run(
        tmp.path(),
        160_000,
        "s1",
        "claude-haiku-4-5-20251001",
        None,
        None,
    );
    assert!(out.unwrap()["systemMessage"]
        .as_str()
        .unwrap()
        .contains("150k"));
}

#[test]
fn test_sidechain_usage_is_ignored() {
    // 200k, not the Python fixture's 250k: see test_below_tier1_is_silent.
    let tmp = tempfile::tempdir().unwrap();
    plant_fake_reseed_here(tmp.path());
    let (_, out) = run(
        tmp.path(),
        200_000,
        "s1",
        "claude-opus-4-8",
        Some(900_000),
        None,
    );
    assert!(out.is_none());
}

#[test]
fn test_subagent_call_is_ignored_and_leaves_no_state() {
    let tmp = tempfile::tempdir().unwrap();
    let (rc, out) = run(
        tmp.path(),
        310_000,
        "s1",
        "claude-opus-4-8",
        None,
        Some(json!({"agent_id": "a3abd7b9b742722ca"})),
    );
    assert_eq!(rc, 0);
    assert!(out.is_none());
    assert!(state(tmp.path(), "s1").is_none());
    let (_, out2) = run(tmp.path(), 310_000, "s1", "claude-opus-4-8", None, None);
    assert!(out2.is_some());
}

#[test]
fn test_one_nudge_per_tier() {
    let tmp = tempfile::tempdir().unwrap();
    plant_fake_reseed_here(tmp.path());
    assert!(
        run(tmp.path(), 310_000, "s1", "claude-opus-4-8", None, None)
            .1
            .is_some()
    );
    assert!(
        run(tmp.path(), 320_000, "s1", "claude-opus-4-8", None, None)
            .1
            .is_none()
    );
}

#[test]
fn test_tier_drop_rearms_the_nudge() {
    let tmp = tempfile::tempdir().unwrap();
    plant_fake_reseed_here(tmp.path());
    run(tmp.path(), 310_000, "s1", "claude-opus-4-8", None, None);
    run(tmp.path(), 50_000, "s1", "claude-opus-4-8", None, None);
    assert_eq!(state(tmp.path(), "s1").unwrap()["tier"], 0);
    assert!(
        run(tmp.path(), 310_000, "s1", "claude-opus-4-8", None, None)
            .1
            .is_some()
    );
}

// --- Pre-arm -----------------------------------------------------------------

#[test]
fn test_tier1_crossing_spawns_reseed_here() {
    let tmp = tempfile::tempdir().unwrap();
    plant_fake_reseed_here(tmp.path());
    run(tmp.path(), 310_000, "s1", "claude-opus-4-8", None, None);
    assert_eq!(spawn_count(tmp.path(), "s1", 1, Duration::from_secs(5)), 1);
}

#[test]
fn test_rearm_is_rate_limited() {
    let tmp = tempfile::tempdir().unwrap();
    plant_fake_reseed_here(tmp.path());
    run(tmp.path(), 310_000, "s1", "claude-opus-4-8", None, None);
    spawn_count(tmp.path(), "s1", 1, Duration::from_secs(5));
    let armed_at_1 = state(tmp.path(), "s1").unwrap()["armed_at"]
        .as_f64()
        .unwrap();
    run(tmp.path(), 315_000, "s1", "claude-opus-4-8", None, None);
    run(tmp.path(), 318_000, "s1", "claude-opus-4-8", None, None);
    let armed_at_2 = state(tmp.path(), "s1").unwrap()["armed_at"]
        .as_f64()
        .unwrap();
    assert_eq!(
        armed_at_1, armed_at_2,
        "re-armed inside the rate-limit interval"
    );
}

#[test]
fn test_rearm_fires_again_after_the_interval() {
    let tmp = tempfile::tempdir().unwrap();
    plant_fake_reseed_here(tmp.path());
    run(tmp.path(), 310_000, "s1", "claude-opus-4-8", None, None);
    spawn_count(tmp.path(), "s1", 1, Duration::from_secs(5));
    age_the_arm(tmp.path(), "s1", REARM_INTERVAL_SECS + 60);
    run(tmp.path(), 315_000, "s1", "claude-opus-4-8", None, None);
    let armed_at = state(tmp.path(), "s1").unwrap()["armed_at"]
        .as_f64()
        .unwrap();
    assert!(armed_at > 0.0);
}

#[test]
fn test_tier_drop_clears_the_arm_stamp() {
    let tmp = tempfile::tempdir().unwrap();
    plant_fake_reseed_here(tmp.path());
    run(tmp.path(), 310_000, "s1", "claude-opus-4-8", None, None);
    run(tmp.path(), 50_000, "s1", "claude-opus-4-8", None, None);
    assert_eq!(
        state(tmp.path(), "s1").unwrap()["armed_at"]
            .as_f64()
            .unwrap(),
        0.0
    );
}

#[test]
fn test_message_drops_the_manual_step_when_armed() {
    let tmp = tempfile::tempdir().unwrap();
    plant_fake_reseed_here(tmp.path());
    arm_sentinel(tmp.path(), "s1", None);
    let (_, out) = run(tmp.path(), 310_000, "s1", "claude-opus-4-8", None, None);
    let ctx = out.unwrap()["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(ctx.contains("ALREADY distilled and armed"));
    assert!(ctx.contains("do NOT run `! reseed-here`"));
}

#[test]
fn test_message_falls_back_when_not_armed() {
    let tmp = tempfile::tempdir().unwrap();
    plant_fake_reseed_here(tmp.path());
    let (_, out) = run(tmp.path(), 310_000, "s1", "claude-opus-4-8", None, None);
    let ctx = out.unwrap()["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(ctx.contains("fall back to `! reseed-here`"));
}

#[test]
fn test_message_leads_with_the_rename_step() {
    let tmp = tempfile::tempdir().unwrap();
    plant_fake_reseed_here(tmp.path());
    arm_sentinel(tmp.path(), "s1", None);
    let (_, out) = run(tmp.path(), 310_000, "s1", "claude-opus-4-8", None, None);
    let ctx = out.unwrap()["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(ctx.contains("`rename-thread`"));
    assert!(ctx.find("rename-thread").unwrap() < ctx.find("/clear").unwrap());
}

#[test]
fn test_stale_sentinel_is_not_treated_as_armed() {
    let tmp = tempfile::tempdir().unwrap();
    plant_fake_reseed_here(tmp.path());
    arm_sentinel(tmp.path(), "s1", Some(900));
    let (_, out) = run(tmp.path(), 310_000, "s1", "claude-opus-4-8", None, None);
    let ctx = out.unwrap()["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(ctx.contains("fall back to `! reseed-here`"));
}

#[test]
fn test_missing_reseed_here_still_nudges() {
    let tmp = tempfile::tempdir().unwrap();
    let (rc, out) = run(tmp.path(), 310_000, "s1", "claude-opus-4-8", None, None);
    assert_eq!(rc, 0);
    assert!(out.is_some());
}

// --- Fail-open contract ------------------------------------------------------

#[test]
fn test_fail_open_bad_stdin() {
    let output = bin()
        .args(["hook", "nudge"])
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
fn test_fail_open_missing_transcript() {
    let tmp = tempfile::tempdir().unwrap();
    let payload = json!({"session_id": "s1", "transcript_path": "/nope/x.jsonl"});
    let output = bin()
        .args(["hook", "nudge"])
        .env("HOME", tmp.path())
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
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
    assert_eq!(output.status.code(), Some(0));
    assert!(String::from_utf8_lossy(&output.stdout).trim().is_empty());
}
