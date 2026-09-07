//! `reseed reload`: the `SessionStart(clear)` emission and the inline cap
//! that keeps it in context (spec 11b deviation 1). Over the cap Claude Code
//! writes the stdout to a file and injects only a path, which loses the
//! candidate block and the task carry-over with it.

use std::fs::{self, File, FileTimes};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, SystemTime};

/// Mirrors `reload::STDOUT_CAP`, which is private to the binary.
const CAP: usize = 8 * 1024;
const SID: &str = "aabbccdd-1111-2222-3333-444455556666";
const SLUG: &str = "weekly-review-20260904";

fn bin() -> Command {
    Command::new(env!("CARGO_BIN_EXE_reseed"))
}

/// A generation-1 bundle: its own first turn is `/clear` + "go", so the
/// emission carries the provenance banner as well as the reload text.
fn plant_bundle(home: &Path) -> PathBuf {
    let dir = home.join(".claude/reseed").join(SID);
    fs::create_dir_all(&dir).unwrap();
    fs::write(
        dir.join("narrative.md"),
        format!(
            "# Reseed narrative\n\n**user:**\n\n<command-name>/clear</command-name>\n\ngo\n\n\
             **assistant:**\n\nResuming: applying the {SLUG} harness cards.\n\n**user:**\n\n\
             apply {SLUG} harness cards from decisions.json\n"
        ),
    )
    .unwrap();
    fs::write(dir.join("context-files.md"), "_(none)_\n").unwrap();
    dir
}

/// A reload text the size the arm step really writes, about 2.4 KB.
fn reload_text(bundle: &Path) -> String {
    let b = bundle.display();
    let mut s = format!(
        "Read {b}/narrative.md in full, then read {b}/context-files.md, and continue where we \
         left off. Before any of that, check whether the bundle owns its own subject: if its \
         first user turn is a real prompt it owns the subject and you resume inside it. "
    );
    while s.len() < 2_400 {
        s.push_str(
            "Local work proceeds; name the inherited subject back and get a yes before any \
             outward-facing write, because a resumed authorisation is not a live one. ",
        );
    }
    s.push('\n');
    s
}

fn plant_sentinel(home: &Path, reload: &str) -> PathBuf {
    let pending = home.join(".claude/reseed/pending");
    fs::create_dir_all(&pending).unwrap();
    let path = pending.join(SID);
    fs::write(&path, reload).unwrap();
    fs::write(
        pending.join(format!("{SID}.cwd")),
        home.display().to_string(),
    )
    .unwrap();
    path
}

/// Eight candidates at full width are about 24 KB on their own.
fn fat_ledger(home: &Path) -> PathBuf {
    let path = home.join("ledger.md");
    let body: Vec<String> = (1..=12)
        .map(|i| {
            format!(
                "2026-09-0{} | {SLUG} card {i}, {}",
                i % 9,
                "x".repeat(4_000)
            )
        })
        .collect();
    fs::write(&path, body.join("\n")).unwrap();
    path
}

fn short_ledger(home: &Path) -> PathBuf {
    let path = home.join("ledger.md");
    let body: Vec<String> = (1..=3)
        .map(|i| format!("2026-09-0{i} | {SLUG} card {i}, short line | resume: nothing"))
        .collect();
    fs::write(&path, body.join("\n")).unwrap();
    path
}

fn backdate(path: &Path, secs: u64) {
    let when = SystemTime::now() - Duration::from_secs(secs);
    let f = File::options().write(true).open(path).unwrap();
    f.set_times(FileTimes::new().set_modified(when)).unwrap();
}

fn run_reload(home: &Path, ledger: &Path) -> String {
    run_reload_with(home, ledger, None)
}

fn run_reload_with(home: &Path, ledger: &Path, messages: Option<&Path>) -> String {
    let mut cmd = bin();
    match messages {
        Some(d) => cmd.env("RESEED_MESSAGES", d),
        None => cmd.env_remove("RESEED_MESSAGES"),
    };
    let mut child = cmd
        .arg("reload")
        .env("HOME", home)
        .env("RESEED_PARK_LEDGER", ledger)
        .env("RESEED_OPERATOR", "Ada")
        .env_remove("CLAUDE_JOB_DIR")
        .env_remove("CLAUDE_CODE_SESSION_ID")
        .env_remove("PWD")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let payload = format!(r#"{{"session_id":"{SID}","cwd":"{}"}}"#, home.display());
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

#[test]
fn a_fresh_emission_fits_the_inline_cap() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path();
    let bundle = plant_bundle(home);
    plant_sentinel(home, &reload_text(&bundle));
    let out = run_reload(home, &fat_ledger(home));

    assert!(
        out.len() <= CAP,
        "emission was {} bytes, over the {CAP} byte cap",
        out.len()
    );
    assert!(out.contains("PROVENANCE"), "banner missing");
    assert!(out.contains("narrative.md"), "reload text missing");
    assert!(out.contains("Park-ledger CANDIDATE lines"), "block missing");
    assert!(out.contains("Shortened to keep this reload inline"));
}

#[test]
fn every_candidate_is_either_shown_or_named_as_left_out() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path();
    let bundle = plant_bundle(home);
    plant_sentinel(home, &reload_text(&bundle));
    let out = run_reload(home, &fat_ledger(home));

    // The matcher keeps the last 8 of the 12 planted lines: 5 through 12.
    for n in 5..=12 {
        assert!(
            out.contains(&format!("{n}: "))
                || out.contains(&format!("Lines {n}"))
                || out.contains(&format!(" {n},"))
                || out.contains(&format!(" {n}.")),
            "candidate line {n} is neither shown nor named as left out"
        );
    }
}

#[test]
fn a_block_that_already_fits_is_emitted_at_full_width() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path();
    let bundle = plant_bundle(home);
    plant_sentinel(home, &reload_text(&bundle));
    let out = run_reload(home, &short_ledger(home));

    assert!(out.len() <= CAP);
    assert!(
        !out.contains("Shortened to keep this reload inline"),
        "a fitting block must not be cut"
    );
    assert!(out.contains("resume: nothing"), "line cut short");
}

#[test]
fn a_stale_emission_fits_the_cap_behind_its_wrapper() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path();
    let bundle = plant_bundle(home);
    let sentinel = plant_sentinel(home, &reload_text(&bundle));
    backdate(&sentinel, 3_600);
    let out = run_reload(home, &fat_ledger(home));

    assert!(
        out.len() <= CAP,
        "stale emission was {} bytes, over the {CAP} byte cap",
        out.len()
    );
    assert!(out.contains("was armed"), "stale wrapper missing");
    assert!(out.contains("UNRELATED task"), "opt-out missing");
}

/// Deviation 5: the doctrine texts are data, so this public crate carries
/// only a neutral fallback and the site's own wording lives outside it.
#[test]
fn a_message_override_replaces_the_compiled_text() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path();
    let bundle = plant_bundle(home);
    plant_sentinel(home, &reload_text(&bundle));
    let messages = home.join("messages");
    fs::create_dir_all(&messages).unwrap();
    fs::write(
        messages.join("provenance.txt"),
        "SITE BANNER for {op}, generation {gen}.\n",
    )
    .unwrap();
    let out = run_reload_with(home, &short_ledger(home), Some(&messages));

    assert!(out.contains("SITE BANNER for Ada, generation 1."));
    assert!(
        !out.contains("PROVENANCE (checked mechanically)"),
        "the override must replace the default, not sit beside it"
    );
}

#[test]
fn an_oversize_reload_text_degrades_to_the_bundle_path() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path();
    let bundle = plant_bundle(home);
    let mut huge = reload_text(&bundle);
    while huge.len() < 12_000 {
        huge.push_str("padding that no reload would really carry. ");
    }
    plant_sentinel(home, &huge);
    let out = run_reload(home, &fat_ledger(home));

    assert!(
        out.len() <= CAP,
        "degraded emission was {} bytes, over the {CAP} byte cap",
        out.len()
    );
    assert!(
        out.contains(&format!("{}/narrative.md", bundle.display())),
        "the bundle path must survive when its text does not"
    );
    assert!(
        !out.contains("padding that no reload"),
        "the oversize text must not be inlined"
    );
}

const OLD_SID: &str = "99887766-5555-4444-3333-222211110000";
const JOB: &str = "b8816add";

/// A generation-0 bundle: its first user turn is a real prompt, so no
/// provenance banner is prepended and `fixed` is the reload text alone.
fn plant_owned_bundle(home: &Path, sid: &str) -> PathBuf {
    let dir = home.join(".claude/reseed").join(sid);
    fs::create_dir_all(&dir).unwrap();
    fs::write(
        dir.join("narrative.md"),
        format!("# Reseed narrative\n\n**user:**\n\napply the {SLUG} harness cards\n"),
    )
    .unwrap();
    fs::write(dir.join("context-files.md"), "_(none)_\n").unwrap();
    dir
}

/// A reload text of exactly `len` bytes naming `bundle`, ending in a marker
/// no other section carries. ASCII throughout, so byte length is char length.
fn reload_text_sized(bundle: &Path, len: usize) -> String {
    const TAIL: &str = " END-OF-RELOAD-TEXT\n";
    let mut s = format!(
        "Read {}/narrative.md in full and continue.",
        bundle.display()
    );
    assert!(s.len() + TAIL.len() <= len, "bundle path alone exceeds len");
    while s.len() + TAIL.len() < len {
        s.push_str(" Local work proceeds; get a yes before any outward-facing write.");
    }
    s.truncate(len - TAIL.len());
    s.push_str(TAIL);
    assert_eq!(s.len(), len);
    s
}

/// A sentinel keyed on `OLD_SID` with a `.job` sidecar, the tier-2 route: the
/// carry-over only runs when the armed key differs from the new session id.
fn plant_job_sentinel(home: &Path, reload: &str) -> PathBuf {
    let pending = home.join(".claude/reseed/pending");
    fs::create_dir_all(&pending).unwrap();
    let path = pending.join(OLD_SID);
    fs::write(&path, reload).unwrap();
    fs::write(pending.join(format!("{OLD_SID}.job")), JOB).unwrap();
    path
}

fn plant_tasks(home: &Path, key: &str, n: usize) {
    let dir = home.join(".claude/tasks").join(key);
    fs::create_dir_all(&dir).unwrap();
    for i in 1..=n {
        fs::write(
            dir.join(format!("{i}.json")),
            format!(r#"{{"id":"{i}","subject":"card {i}","status":"pending"}}"#),
        )
        .unwrap();
    }
}

fn run_reload_in_job(home: &Path, ledger: &Path) -> String {
    let mut child = bin()
        .arg("reload")
        .env("HOME", home)
        .env("RESEED_PARK_LEDGER", ledger)
        .env("RESEED_OPERATOR", "Ada")
        .env("CLAUDE_JOB_DIR", home.join("jobs").join(JOB))
        .env_remove("RESEED_MESSAGES")
        .env_remove("CLAUDE_CODE_SESSION_ID")
        .env_remove("PWD")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let payload = format!(r#"{{"session_id":"{SID}","cwd":"{}"}}"#, home.display());
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

/// Sizes the reload text so the fixed section clears the cap by less than the
/// carry-over note needs. Appending the note unguarded is what overflowed.
fn boundary_emission(home: &Path) -> String {
    let bundle = plant_owned_bundle(home, OLD_SID);
    plant_job_sentinel(home, &reload_text_sized(&bundle, CAP - 40));
    plant_tasks(home, OLD_SID, 8);
    run_reload_in_job(home, &short_ledger(home))
}

#[test]
fn a_task_carry_over_cannot_push_the_emission_over_the_cap() {
    let tmp = tempfile::tempdir().unwrap();
    let out = boundary_emission(tmp.path());

    assert!(
        out.len() <= CAP,
        "emission was {} bytes, over the {CAP} byte cap",
        out.len()
    );
    assert!(out.contains("Task list carried over"), "carry-over missing");
}

#[test]
fn a_reload_that_fits_alone_stays_inline_when_the_carry_over_does_not() {
    let tmp = tempfile::tempdir().unwrap();
    let out = boundary_emission(tmp.path());

    assert!(
        out.contains("END-OF-RELOAD-TEXT"),
        "the reload text was dropped for a bundle pointer to make room for a \
         150-byte advisory note; the note is what gives way, not the brief"
    );
    assert!(
        !out.contains("past the"),
        "degraded to the oversize pointer while the reload text still fit"
    );
}
