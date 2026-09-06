//! Tier selection for `reseed reload`, transcribed case for case from
//! `scripts/tests/test_reseed_clear_hook.py`. That suite is the contract the
//! port has to meet; `tests/reload.rs` only ever covered the inline cap, so
//! `src/reload.rs` tier selection was untested from both directions.
//!
//! Behaviours locked here, unchanged from the Python:
//!
//!   tier 1 (exact session id)  fresh -> raw reload; stale -> stale wrapper
//!   tier 2 (job lineage)       fresh -> raw; one stale -> wrapper; 2+ -> GC
//!   tier 3 (cwd heuristic)     named, never auto-loaded, freshness-gated
//!   unarmed                    silent no-op

use std::fs::{self, File, FileTimes};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, SystemTime};

/// Comfortably past the hook's `FRESH_MIN` of 10 minutes.
const STALE_AGE: u64 = 20 * 60;
const STALE_MARKER: &str = "was armed";
const OPT_OUT_MARKER: &str = "UNRELATED task";

/// An isolated HOME with the pending dir the hook reads.
fn home() -> tempfile::TempDir {
    let t = tempfile::tempdir().unwrap();
    fs::create_dir_all(t.path().join(".claude/reseed/pending")).unwrap();
    t
}

fn pending(home: &Path) -> PathBuf {
    home.join(".claude/reseed/pending")
}

/// Write a sentinel plus its `.cwd` sidecar exactly as `reseed-here` does.
fn arm(home: &Path, key: &str, cwd: &str, stale: bool, body: Option<&str>) -> PathBuf {
    let s = pending(home).join(key);
    let text = body
        .map(str::to_string)
        .unwrap_or_else(|| format!("Read {key}/narrative.md and continue."));
    fs::write(&s, text).unwrap();
    let sidecar = pending(home).join(format!("{key}.cwd"));
    fs::write(&sidecar, cwd).unwrap();
    if stale {
        let when = SystemTime::now() - Duration::from_secs(STALE_AGE);
        for p in [&s, &sidecar] {
            let f = File::options().write(true).open(p).unwrap();
            f.set_times(FileTimes::new().set_modified(when)).unwrap();
        }
    }
    s
}

/// Invoke `reload` with a SessionStart payload on stdin and a clean env.
fn run(home: &Path, sid: &str, cwd: &str, job: Option<&str>, ledger: Option<&Path>) -> String {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_reseed"));
    cmd.arg("reload")
        .env("HOME", home)
        .env("PWD", cwd)
        .env("RESEED_OPERATOR", "Mark")
        .env_remove("RESEED_MESSAGES")
        .env_remove("CLAUDE_CODE_SESSION_ID");
    match job {
        Some(j) => cmd.env("CLAUDE_JOB_DIR", j),
        None => cmd.env_remove("CLAUDE_JOB_DIR"),
    };
    match ledger {
        Some(l) => cmd.env("RESEED_PARK_LEDGER", l),
        None => cmd.env_remove("RESEED_PARK_LEDGER"),
    };
    let mut child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let payload = format!(r#"{{"session_id": "{sid}", "cwd": "{cwd}"}}"#);
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(payload.as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success(), "reload must always exit 0");
    String::from_utf8(out.stdout).expect("stdout is valid UTF-8")
}

// --- unarmed -----------------------------------------------------------------

#[test]
fn no_pending_dir_is_silent() {
    let tmp = tempfile::tempdir().unwrap();
    assert_eq!(run(tmp.path(), "s1", "/work", None, None), "");
}

#[test]
fn armed_for_nobody_is_silent() {
    let h = home();
    arm(h.path(), "other-session", "/elsewhere", false, None);
    assert_eq!(run(h.path(), "s1", "/work", None, None), "");
}

// --- tier 1: exact session id ------------------------------------------------

#[test]
fn tier1_fresh_emits_raw_reload() {
    let h = home();
    arm(h.path(), "s1", "/work", false, Some("RELOAD-BODY"));
    let out = run(h.path(), "s1", "/work", None, None);
    assert!(out.contains("RELOAD-BODY"));
    assert!(!out.contains(STALE_MARKER), "raw, not the stale wrapper");
    assert!(!pending(h.path()).join("s1").exists(), "not consumed");
}

/// The 2026-07-25 fix: identity is certain, so reload rather than discard.
#[test]
fn tier1_stale_emits_stale_wrapper() {
    let h = home();
    arm(h.path(), "s1", "/work", true, Some("RELOAD-BODY"));
    let out = run(h.path(), "s1", "/work", None, None);
    assert!(out.contains("RELOAD-BODY"));
    assert!(out.contains(STALE_MARKER));
    assert!(
        out.contains(OPT_OUT_MARKER),
        "opt-out for an unrelated first prompt"
    );
    assert!(!pending(h.path()).join("s1").exists());
}

#[test]
fn tier1_stale_reports_age_in_minutes() {
    let h = home();
    arm(h.path(), "s1", "/work", true, None);
    assert!(run(h.path(), "s1", "/work", None, None).contains("20m ago"));
}

#[test]
fn tier1_consumes_cwd_sidecar() {
    let h = home();
    arm(h.path(), "s1", "/work", false, None);
    run(h.path(), "s1", "/work", None, None);
    assert!(!pending(h.path()).join("s1.cwd").exists());
}

// --- tier 2: job lineage -----------------------------------------------------

#[test]
fn tier2_fresh_job_match_emits_raw() {
    let h = home();
    arm(h.path(), "job7-abc", "/work", false, Some("JOB-BODY"));
    let out = run(h.path(), "new-id", "/work", Some("/jobs/job7"), None);
    assert!(out.contains("JOB-BODY") && !out.contains(STALE_MARKER));
}

#[test]
fn tier2_single_stale_job_match_emits_stale_wrapper() {
    let h = home();
    arm(h.path(), "job7-abc", "/work", true, Some("JOB-BODY"));
    let out = run(h.path(), "new-id", "/work", Some("/jobs/job7"), None);
    assert!(out.contains("JOB-BODY") && out.contains(STALE_MARKER));
    assert!(!pending(h.path()).join("job7-abc").exists());
}

#[test]
fn tier2_two_stale_in_one_job_is_gc_not_a_guess() {
    let h = home();
    arm(h.path(), "job7-abc", "/work", true, None);
    arm(h.path(), "job7-def", "/work", true, None);
    let out = run(h.path(), "new-id", "/elsewhere", Some("/jobs/job7"), None);
    assert_eq!(out, "");
    assert!(!pending(h.path()).join("job7-abc").exists());
    assert!(!pending(h.path()).join("job7-def").exists());
}

#[test]
fn tier2_fresh_wins_over_stale_sibling() {
    let h = home();
    arm(h.path(), "job7-old", "/work", true, None);
    arm(h.path(), "job7-new", "/work", false, Some("FRESH-BODY"));
    let out = run(h.path(), "new-id", "/work", Some("/jobs/job7"), None);
    assert!(out.contains("FRESH-BODY") && !out.contains(STALE_MARKER));
}

// --- tier 3: cwd heuristic ---------------------------------------------------

/// 2026-08-18: a cwd-only match is reported, never auto-loaded, left on disk.
#[test]
fn tier3_unique_fresh_cwd_match_is_named_not_loaded() {
    let h = home();
    arm(h.path(), "stranger", "/work/sub", false, Some("CWD-BODY"));
    let out = run(h.path(), "new-id", "/work", None, None);
    assert!(!out.contains("CWD-BODY") && out.contains("NOT auto-loaded"));
    assert!(pending(h.path()).join("stranger").exists());
}

/// 2026-07-25 case: same-cwd arms. Picking one could cross-load a stranger.
#[test]
fn tier3_two_fresh_candidates_never_auto_loads() {
    let h = home();
    arm(h.path(), "a", "/work", false, Some("BODY-A"));
    arm(h.path(), "b", "/work", false, Some("BODY-B"));
    let out = run(h.path(), "new-id", "/work", None, None);
    assert!(!out.contains("BODY-A") && !out.contains("BODY-B"));
    assert!(
        pending(h.path()).join("a").exists(),
        "fresh candidates are not consumed"
    );
    assert!(pending(h.path()).join("b").exists());
}

/// Injecting nothing is the same silent loss the stale path fixes: name them.
#[test]
fn tier3_ambiguous_surfaces_the_candidates() {
    let h = home();
    arm(h.path(), "a", "/work", false, None);
    arm(h.path(), "b", "/work", false, None);
    let out = run(h.path(), "new-id", "/work", None, None);
    assert!(out.contains("2 sessions"));
    assert!(out.contains(&pending(h.path()).join("a").display().to_string()));
    assert!(out.contains(&pending(h.path()).join("b").display().to_string()));
    assert!(out.contains(OPT_OUT_MARKER));
}

#[test]
fn tier3_single_candidate_says_nothing_about_ambiguity() {
    let h = home();
    arm(h.path(), "only", "/work", false, Some("CWD-BODY"));
    let out = run(h.path(), "new-id", "/work", None, None);
    assert!(!out.contains("sessions under this directory"));
}

/// One stale cwd-only arm is reported (2026-08-06 stopped the GC), never loaded.
#[test]
fn tier3_single_stale_arm_is_named_not_reloaded() {
    let h = home();
    arm(h.path(), "stranger", "/work", true, Some("CWD-BODY"));
    let out = run(h.path(), "new-id", "/work", None, None);
    assert!(!out.contains("CWD-BODY") && out.contains("stale arm"));
    assert!(pending(h.path()).join("stranger").exists());
}

#[test]
fn tier3_ignores_arm_outside_this_root() {
    let h = home();
    arm(h.path(), "stranger", "/somewhere/else", false, None);
    let out = run(h.path(), "new-id", "/work", None, None);
    assert_eq!(out, "");
    assert!(
        pending(h.path()).join("stranger").exists(),
        "not ours to GC"
    );
}

// --- safety ------------------------------------------------------------------

/// The stale path re-arms via `reseed-here`; absent under a test HOME it must
/// not fail the emission.
#[test]
fn stale_path_survives_missing_rearm_binary() {
    let h = home();
    arm(h.path(), "s1", "/work", true, Some("RELOAD-BODY"));
    assert!(run(h.path(), "s1", "/work", None, None).contains("RELOAD-BODY"));
}

// --- task list carry-over ------------------------------------------------------

/// `~/.claude/tasks/<sid>/<n>.json` the way TaskCreate writes it, plus the
/// `.lock` the carry-over must not copy.
fn tasks(home: &Path, sid: &str, items: &[(&str, &str)]) -> PathBuf {
    let d = home.join(".claude/tasks").join(sid);
    fs::create_dir_all(&d).unwrap();
    for (i, (subject, status)) in items.iter().enumerate() {
        let n = i + 1;
        fs::write(
            d.join(format!("{n}.json")),
            format!(
                "{{\n  \"id\": \"{n}\",\n  \"subject\": \"{subject}\",\n  \"status\": \
                 \"{status}\",\n  \"blocks\": [],\n  \"blockedBy\": []\n}}\n"
            ),
        )
        .unwrap();
    }
    fs::write(d.join(".lock"), "").unwrap();
    d
}

fn names_in(dir: &Path) -> Vec<String> {
    let mut v: Vec<String> = fs::read_dir(dir)
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    v.sort();
    v
}

#[test]
fn tier2_carries_task_list_into_new_session() {
    let h = home();
    tasks(
        h.path(),
        "job7-abc",
        &[
            ("open one", "pending"),
            ("done one", "completed"),
            ("live", "in_progress"),
        ],
    );
    arm(h.path(), "job7-abc", "/work", false, Some("JOB-BODY"));
    let out = run(h.path(), "new-id", "/work", Some("/jobs/job7"), None);
    let new = h.path().join(".claude/tasks/new-id");
    assert_eq!(names_in(&new), ["1.json", "2.json", "3.json"]);
    assert!(out.contains("3 tasks, 2 open"), "got: {out}");
}

#[test]
fn tier2_stale_also_carries_task_list() {
    let h = home();
    tasks(h.path(), "job7-abc", &[("open one", "pending")]);
    arm(h.path(), "job7-abc", "/work", true, Some("JOB-BODY"));
    let out = run(h.path(), "new-id", "/work", Some("/jobs/job7"), None);
    assert!(h.path().join(".claude/tasks/new-id/1.json").exists());
    assert!(out.contains("1 tasks, 1 open"), "got: {out}");
}

#[test]
fn tier1_same_session_does_not_copy() {
    let h = home();
    tasks(h.path(), "same-id", &[("open one", "pending")]);
    arm(h.path(), "same-id", "/work", false, Some("BODY"));
    assert!(!run(h.path(), "same-id", "/work", None, None).contains("carried over"));
}

#[test]
fn cwd_only_report_never_adopts_tasks() {
    let h = home();
    tasks(h.path(), "stranger", &[("theirs", "pending")]);
    arm(h.path(), "stranger", "/work", false, Some("BODY"));
    let out = run(h.path(), "new-id", "/work", Some("/jobs/other"), None);
    assert!(!h.path().join(".claude/tasks/new-id").exists());
    assert!(!out.contains("carried over"));
}

#[test]
fn carry_never_clobbers_a_started_list() {
    let h = home();
    tasks(h.path(), "job7-abc", &[("old", "pending")]);
    tasks(h.path(), "new-id", &[("already here", "in_progress")]);
    arm(h.path(), "job7-abc", "/work", false, Some("JOB-BODY"));
    run(h.path(), "new-id", "/work", Some("/jobs/job7"), None);
    let kept = fs::read_to_string(h.path().join(".claude/tasks/new-id/1.json")).unwrap();
    assert!(kept.contains("already here"));
}

// --- park-ledger pre-grep (r-3, weekly review 2026-09-04) ---------------------

/// A bundle whose narrative names a slug, plus a two-thread ledger.
fn bundle_with_ledger(home: &Path) -> (PathBuf, PathBuf) {
    let bundle = home
        .join(".claude/reseed")
        .join("8b887405-411c-4048-9b14-6ed9596194f2");
    fs::create_dir_all(&bundle).unwrap();
    fs::write(
        bundle.join("narrative.md"),
        "# Reseed narrative\n\n**user:**\n\napply weekly-review-20260904 harness cards\n\n\
         **assistant:**\n\nApplying the sixteen decisions to the harness scripts.\n",
    )
    .unwrap();
    let ledger = home.join("scratch/parked/host.md");
    fs::create_dir_all(ledger.parent().unwrap()).unwrap();
    fs::write(
        &ledger,
        "2026-08-29 | OWED, avax failover 21shares2 identity swap pending | resume: x\n\
         2026-09-04 | OWED, apply weekly-review-20260904 is PLANNED NOT EXECUTED | resume: y\n",
    )
    .unwrap();
    (bundle, ledger)
}

#[test]
fn fresh_reload_injects_only_matching_park_lines() {
    let h = home();
    let (bundle, ledger) = bundle_with_ledger(h.path());
    let body = format!("Read {}/narrative.md and continue.", bundle.display());
    arm(h.path(), "sess-park", "/work", false, Some(&body));
    let out = run(h.path(), "sess-park", "/work", None, Some(&ledger));
    assert!(out.contains("Park-ledger CANDIDATE lines"));
    assert!(out.contains("weekly-review-20260904 is PLANNED"));
    assert!(!out.contains("avax failover"));
    assert!(
        out.find("narrative.md") < out.find("Park-ledger CANDIDATE"),
        "the brief must precede the candidates"
    );
    assert!(out.contains("ANOTHER THREAD'S WORK") && !out.contains("YOUR thread"));
}

#[test]
fn stale_reload_injects_park_lines_too() {
    let h = home();
    let (bundle, ledger) = bundle_with_ledger(h.path());
    let body = format!("Read {}/narrative.md and continue.", bundle.display());
    arm(h.path(), "sess-park", "/work", true, Some(&body));
    let out = run(h.path(), "sess-park", "/work", None, Some(&ledger));
    assert!(out.contains(STALE_MARKER) && out.contains("weekly-review-20260904 is PLANNED"));
}

#[test]
fn no_match_says_none_and_absent_ledger_is_silent() {
    let h = home();
    let (bundle, ledger) = bundle_with_ledger(h.path());
    fs::write(
        &ledger,
        "2026-08-01 | OWED, ken postfix rbl tuning | resume: z\n",
    )
    .unwrap();
    let body = format!("Read {}/narrative.md and continue.", bundle.display());

    arm(h.path(), "sess-park", "/work", false, Some(&body));
    let out = run(h.path(), "sess-park", "/work", None, Some(&ledger));
    assert!(out.contains("Park-ledger CANDIDATE lines") && out.contains("\nnone\n"));

    arm(h.path(), "sess-park", "/work", false, Some(&body));
    let out = run(
        h.path(),
        "sess-park",
        "/work",
        None,
        Some(&h.path().join("absent.md")),
    );
    assert!(!out.contains("Park-ledger CANDIDATE lines") && out.contains("narrative.md"));
}
