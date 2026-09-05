//! `reseed arm`: sentinel-last ordering, sidecar contents, `--quiet`,
//! missing sid, and lock contention (spec 5.2).

use std::fs;
use std::process::{Command, Stdio};

fn bin() -> Command {
    Command::new(env!("CARGO_BIN_EXE_reseed"))
}

fn plant_transcript(home: &std::path::Path, sid: &str) -> std::path::PathBuf {
    let dir = home.join(".claude/projects/proj");
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join(format!("{sid}.jsonl"));
    fs::write(
        &path,
        format!(
            "{}\n{}\n",
            r#"{"message":{"role":"user","content":"go"}}"#,
            r#"{"message":{"role":"assistant","content":[{"type":"text","text":"ok"}]}}"#
        ),
    )
    .unwrap();
    path
}

fn run_arm(home: &std::path::Path, sid: &str, quiet: bool) -> std::process::Output {
    let mut args = vec!["arm", "--sid", sid];
    if quiet {
        args.push("--quiet");
    }
    bin()
        .args(&args)
        .env("HOME", home)
        .env_remove("CLAUDE_CODE_SESSION_ID")
        .env_remove("PWD")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .unwrap()
}

#[test]
fn sentinel_written_last_a_failed_distill_leaves_none() {
    let tmp = tempfile::tempdir().unwrap();
    // No transcript planted: the distill must fail and no sentinel appears.
    let out = run_arm(tmp.path(), "nope-sid", true);
    assert!(!out.status.success());
    let pending = tmp.path().join(".claude/reseed/pending/nope-sid");
    assert!(!pending.exists());
}

#[test]
fn cwd_job_pid_sidecars_are_written() {
    let tmp = tempfile::tempdir().unwrap();
    plant_transcript(tmp.path(), "sid1");
    let mut cmd = bin();
    let out = cmd
        .args(["arm", "--sid", "sid1", "--quiet", "--pid", "4242"])
        .env("HOME", tmp.path())
        .env("CLAUDE_JOB_DIR", "/tmp/jobs/job-abc")
        .env_remove("CLAUDE_CODE_SESSION_ID")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let pending = tmp.path().join(".claude/reseed/pending");
    assert!(pending.join("sid1.cwd").exists());
    assert_eq!(
        fs::read_to_string(pending.join("sid1.job")).unwrap(),
        "job-abc"
    );
    assert_eq!(
        fs::read_to_string(pending.join("sid1.pid")).unwrap(),
        "4242"
    );
    assert!(pending.join("sid1").exists());
}

#[test]
fn quiet_prints_nothing_on_stdout() {
    let tmp = tempfile::tempdir().unwrap();
    plant_transcript(tmp.path(), "sid2");
    let out = run_arm(tmp.path(), "sid2", true);
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stdout).trim().is_empty());
}

#[test]
fn missing_sid_exits_1_with_the_exact_message() {
    let tmp = tempfile::tempdir().unwrap();
    let output = bin()
        .args(["arm"])
        .env("HOME", tmp.path())
        .env_remove("CLAUDE_CODE_SESSION_ID")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    let err = String::from_utf8_lossy(&output.stderr);
    assert!(err.contains("CLAUDE_CODE_SESSION_ID"));
    assert!(err.contains("is unset"));
}

#[test]
fn lock_contention_exits_0_with_already_distilling() {
    let tmp = tempfile::tempdir().unwrap();
    plant_transcript(tmp.path(), "sid3");
    let pending = tmp.path().join(".claude/reseed/pending");
    fs::create_dir_all(&pending).unwrap();
    fs::write(pending.join("sid3.lock"), "").unwrap();
    let out = run_arm(tmp.path(), "sid3", true);
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("already distilling"));
    // No new sentinel: the held lock stopped the distill from running.
    assert!(!pending.join("sid3").exists());
}
