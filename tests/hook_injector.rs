//! The background-job injector branch of `reseed hook nudge|halt|guard`.
//!
//! `reseed watch --inject` clears an armed background job and types 'go'
//! itself, so telling the operator to "/clear then go" there is stale advice.
//! The injector text must show only when the injector will really act (a job,
//! a fresh `watch.log`, no kill file, a bundle armed or distilling) and every
//! other shape keeps the manual advice unchanged.

use serde_json::{json, Value};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, SystemTime};

const SESSION: &str = "inj-1";
const LIVE: &str = "injector is live";
const FALLBACK: &str = "if it is still here 2 min later, /clear then 'go'";

#[derive(Clone, Copy, PartialEq)]
enum Watch {
    Fresh,
    Stale,
    Off,
    Absent,
}

fn transcript(home: &Path, ctx: u64) -> PathBuf {
    let path = home.join("t.jsonl");
    let msg = json!({
        "model": "claude-opus-5",
        "usage": {"input_tokens": ctx},
        "content": [{"type": "text", "text": "done: nothing about resetting"}],
    });
    fs::write(
        &path,
        json!({"type": "assistant", "message": msg}).to_string() + "\n",
    )
    .unwrap();
    path
}

fn setup(home: &Path, watch: Watch, armed: bool) {
    let reseed = home.join(".claude/reseed");
    fs::create_dir_all(reseed.join("pending")).unwrap();
    if armed {
        fs::write(reseed.join("pending").join(SESSION), "x").unwrap();
    }
    if watch != Watch::Absent {
        let log = reseed.join("watch.log");
        fs::write(&log, "tick\n").unwrap();
        if watch == Watch::Stale {
            let old = SystemTime::now() - Duration::from_secs(600);
            fs::File::options()
                .write(true)
                .open(&log)
                .unwrap()
                .set_modified(old)
                .unwrap();
        }
    }
    if watch == Watch::Off {
        fs::write(reseed.join("watch.off"), "").unwrap();
    }
}

/// `(exit code, stdout, stderr)` for one firing, with the site message dir
/// and operator name kept out so only compiled defaults are under test.
fn run(hook: &str, home: &Path, ctx: u64, job: bool) -> (i32, String, String) {
    let payload = json!({
        "session_id": SESSION,
        "transcript_path": transcript(home, ctx).to_str().unwrap(),
    });
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_reseed"));
    cmd.args(["hook", hook])
        .env("HOME", home)
        .env("RESEED_MESSAGES", home.join("no-messages"))
        .env_remove("RESEED_OPERATOR")
        .env_remove("CLAUDE_NO_RESET_HALT")
        .env_remove("CLAUDE_JOB_DIR")
        .env_remove("STATUSLINE_RESET_LINE")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if job {
        cmd.env("CLAUDE_JOB_DIR", home.join("job"));
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
        String::from_utf8_lossy(&output.stderr).trim().to_string(),
    )
}

fn nudge_text(stdout: &str) -> (String, String) {
    let v: Value = serde_json::from_str(stdout).unwrap();
    (
        v["systemMessage"].as_str().unwrap().to_string(),
        v["hookSpecificOutput"]["additionalContext"]
            .as_str()
            .unwrap()
            .to_string(),
    )
}

// --- nudge ------------------------------------------------------------------

#[test]
fn nudge_live_drops_manual_advice_in_the_early_band_and_past_the_line() {
    for ctx in [250_000, 310_000] {
        let tmp = tempfile::tempdir().unwrap();
        setup(tmp.path(), Watch::Fresh, true);
        let (rc, out, _) = run("nudge", tmp.path(), ctx, true);
        let (system, context) = nudge_text(&out);
        assert_eq!(rc, 0);
        assert!(
            context.contains(LIVE) && context.contains("rename-thread"),
            "{ctx}"
        );
        assert!(!context.contains("/clear then"), "{ctx}: {context}");
        assert!(system.contains("auto-clears this job") && system.contains(FALLBACK));
    }
}

#[test]
fn nudge_manual_when_the_injector_will_not_act() {
    for (watch, job) in [
        (Watch::Stale, true),
        (Watch::Off, true),
        (Watch::Absent, true),
        (Watch::Fresh, false),
    ] {
        let tmp = tempfile::tempdir().unwrap();
        setup(tmp.path(), watch, true);
        let (_, out, _) = run("nudge", tmp.path(), 250_000, job);
        let (system, context) = nudge_text(&out);
        assert!(!context.contains(LIVE), "{context}");
        assert!(context.contains("/clear then") && system.contains("/clear then"));
    }
}

// --- halt -------------------------------------------------------------------

#[test]
fn halt_live_still_denies_without_the_manual_wait() {
    let tmp = tempfile::tempdir().unwrap();
    setup(tmp.path(), Watch::Fresh, true);
    let (rc, out, _) = run("halt", tmp.path(), 310_000, true);
    let v: Value = serde_json::from_str(&out).unwrap();
    let reason = v["hookSpecificOutput"]["permissionDecisionReason"]
        .as_str()
        .unwrap();
    assert_eq!(rc, 0);
    assert_eq!(v["hookSpecificOutput"]["permissionDecision"], "deny");
    assert!(reason.contains(LIVE) && !reason.contains("STOP and wait"));
    assert!(reason.contains("override reset"));
    assert!(v["systemMessage"].as_str().unwrap().contains(FALLBACK));
}

#[test]
fn halt_terminal_keeps_the_manual_wait_and_no_banner() {
    let tmp = tempfile::tempdir().unwrap();
    setup(tmp.path(), Watch::Fresh, true);
    let (_, out, _) = run("halt", tmp.path(), 310_000, false);
    let v: Value = serde_json::from_str(&out).unwrap();
    let reason = v["hookSpecificOutput"]["permissionDecisionReason"]
        .as_str()
        .unwrap();
    assert!(reason.contains("STOP and wait") && !reason.contains(LIVE));
    assert!(v.get("systemMessage").is_none());
}

// --- Stop guard -------------------------------------------------------------

#[test]
fn guard_live_past_the_line_never_blocks() {
    let tmp = tempfile::tempdir().unwrap();
    setup(tmp.path(), Watch::Fresh, true);
    let (rc, out, err) = run("guard", tmp.path(), 310_000, true);
    assert_eq!((rc, err.as_str()), (0, ""));
    let v: Value = serde_json::from_str(&out).unwrap();
    assert!(v["systemMessage"].as_str().unwrap().contains(FALLBACK));
}

#[test]
fn guard_with_a_stale_watch_still_blocks() {
    let tmp = tempfile::tempdir().unwrap();
    setup(tmp.path(), Watch::Stale, true);
    let (rc, _, err) = run("guard", tmp.path(), 310_000, true);
    assert_eq!(rc, 2);
    assert!(err.contains("RESET NOW"), "{err}");
}

#[test]
fn guard_early_band_live_line() {
    let tmp = tempfile::tempdir().unwrap();
    setup(tmp.path(), Watch::Fresh, true);
    let (rc, out, _) = run("guard", tmp.path(), 250_000, true);
    let msg = serde_json::from_str::<Value>(&out).unwrap()["systemMessage"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(rc, 0);
    assert!(msg.contains("bundle armed: the injector auto-clears") && msg.contains(FALLBACK));
    assert!(!msg.contains("next natural break"), "{msg}");
}

/// The port re-arms by re-exec rather than through `reseed-here`, so an
/// unarmed job always has a distill running and is a candidate once it lands.
#[test]
fn guard_early_band_unarmed_job_reports_the_running_distill() {
    let tmp = tempfile::tempdir().unwrap();
    setup(tmp.path(), Watch::Fresh, false);
    let (_, out, _) = run("guard", tmp.path(), 250_000, true);
    let msg = serde_json::from_str::<Value>(&out).unwrap()["systemMessage"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(
        msg.contains("distill running: the injector auto-clears"),
        "{msg}"
    );
}

#[test]
fn guard_early_band_terminal_line() {
    let tmp = tempfile::tempdir().unwrap();
    setup(tmp.path(), Watch::Fresh, true);
    let (_, out, _) = run("guard", tmp.path(), 250_000, false);
    let msg = serde_json::from_str::<Value>(&out).unwrap()["systemMessage"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(msg.contains("bundle armed: /clear then 'go'"), "{msg}");
}

/// The operator name reaches the model-facing step through the environment,
/// never the binary.
#[test]
fn injector_step_names_nobody_by_default() {
    let tmp = tempfile::tempdir().unwrap();
    setup(tmp.path(), Watch::Fresh, true);
    let (_, out, _) = run("halt", tmp.path(), 310_000, true);
    assert!(
        out.contains("Do NOT advise the operator to /clear"),
        "{out}"
    );
}
