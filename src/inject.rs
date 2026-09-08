//! The injection half of `reseed watch`: types `/clear`, waits for the
//! reload to be proven delivered, then types `go`. Background jobs only,
//! because only they have a daemon-owned pty to type into; a terminal
//! session is left to Mark's hands (spec D2).
//!
//! Nothing here runs without `--inject`, and every pass re-reads the kill
//! file, so stopping it is one `touch ~/.claude/reseed/watch.off`.

use crate::{control, emit, parse, paths, pty, screen, sentinel, usage, watch};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Gap between the two box reads that bound the type-under-us window.
const SETTLE: Duration = Duration::from_secs(3);

/// A stage that never completes is retried once, then parked this long so a
/// misjudged session is not typed into again and again (spec D6).
const COOLDOWN: Duration = Duration::from_secs(15 * 60);
/// The reload hook has a 30 s timeout, so a clear can take that long to
/// prove itself before the retry is the right move.
const CLEAR_GRACE: Duration = Duration::from_secs(45);
const MAX_ATTEMPTS: u32 = 2;

/// Tiers that mean the hook matched this session by identity and loaded its
/// bundle. `3-*` is the cwd heuristic, which also logs `arm=` and can inject
/// nothing, so it is not proof (spec D9).
fn is_identity_tier(tier: &str) -> bool {
    matches!(tier, "1" | "1b" | "2" | "2-jobmatch") || tier.starts_with("2-jobmatch")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Stage {
    ClearSent,
    GoSent,
    Done,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Entry {
    pub stage: Stage,
    pub at: u64,
    #[serde(default)]
    pub attempts: u32,
    #[serde(default)]
    pub job: String,
    /// The session that was cleared. The reload lands in a NEW session id,
    /// so this is the only handle that survives the clear.
    #[serde(default)]
    pub arm_sid: String,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct State {
    #[serde(default)]
    pub sessions: BTreeMap<String, Entry>,
}

impl State {
    pub fn load(path: &Path) -> Self {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|b| serde_json::from_str(&b).ok())
            .unwrap_or_default()
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        crate::atomic::write(path, serde_json::to_string_pretty(self)?.as_bytes())
    }
}

/// One session's outcome for this pass, for `watch.log` and `--json`.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Action {
    pub sid8: String,
    pub job: String,
    pub did: String,
    pub why: String,
}

impl Action {
    fn new(sid: &str, job: &str, did: &str, why: impl Into<String>) -> Self {
        Self {
            sid8: sid.chars().take(8).collect(),
            job: job.to_string(),
            did: did.to_string(),
            why: why.into(),
        }
    }
}

/// A background session past the line with a bundle armed for it.
pub struct Candidate {
    pub sid: String,
    pub job: String,
    pub transcript: PathBuf,
    pub arm: sentinel::Arm,
}

pub struct InjectOpts {
    pub dry_run: bool,
}

pub fn run(opts: &InjectOpts) -> Result<Vec<Action>> {
    let kill = paths::kill_file()?;
    if kill.exists() {
        return Ok(vec![Action::new(
            "-",
            "-",
            "off",
            format!("{}", kill.display()),
        )]);
    }
    let state_path = paths::watch_state()?;
    let mut state = State::load(&state_path);
    let control = control::Control::discover()?;
    let jobs = control.list()?;
    let now = SystemTime::now();

    let mut actions = Vec::new();
    for cand in candidates(now)? {
        let entry = state.sessions.get(&cand.sid).cloned();
        let action = match entry.map(|e| (e.stage, e)) {
            Some((Stage::Done, _)) => continue,
            Some((Stage::Failed, e)) if !cooled_down(&e, now) => continue,
            Some((Stage::ClearSent, e)) => after_clear(&control, &cand, &e, now, opts, &mut state)?,
            Some((Stage::GoSent, e)) => {
                state.sessions.insert(
                    cand.sid.clone(),
                    Entry {
                        stage: Stage::Done,
                        ..e
                    },
                );
                Action::new(&cand.sid, &cand.job, "done", "clear and go both delivered")
            }
            _ => send_clear(&control, &jobs, &cand, now, opts, &mut state)?,
        };
        actions.push(action);
    }
    if !opts.dry_run {
        state.save(&state_path)?;
    }
    Ok(actions)
}

fn cooled_down(entry: &Entry, now: SystemTime) -> bool {
    secs(now).saturating_sub(entry.at) >= COOLDOWN.as_secs()
}

fn secs(t: SystemTime) -> u64 {
    t.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs()
}

/// Guards, then one `/clear`. Order is cheapest-first, and every refusal
/// names the guard so `watch.log` says why a session was left alone.
fn send_clear(
    control: &control::Control,
    jobs: &[control::Job],
    cand: &Candidate,
    now: SystemTime,
    opts: &InjectOpts,
    state: &mut State,
) -> Result<Action> {
    let Some(job) = jobs.iter().find(|j| j.short == cand.job) else {
        return Ok(Action::new(
            &cand.sid,
            &cand.job,
            "skip",
            "no live daemon worker for this job",
        ));
    };
    if !job.is_idle() {
        return Ok(Action::new(&cand.sid, &cand.job, "skip", "worker is busy"));
    }
    if !sentinel::is_fresh(&cand.arm, now) {
        return Ok(Action::new(
            &cand.sid,
            &cand.job,
            "skip",
            "sentinel is stale",
        ));
    }
    if let Some(why) = bundle_trails_transcript(&cand.sid, &cand.transcript) {
        return Ok(Action::new(&cand.sid, &cand.job, "skip", why));
    }
    if distill_running(&cand.sid) {
        return Ok(Action::new(
            &cand.sid,
            &cand.job,
            "skip",
            "a distill for this session is still running",
        ));
    }
    if attached_client(&cand.job) {
        return Ok(Action::new(
            &cand.sid,
            &cand.job,
            "skip",
            "a client is attached, so someone may be typing",
        ));
    }
    let box_state = prompt_box(&cand.job)?;
    if !box_state.may_type() {
        return Ok(Action::new(
            &cand.sid,
            &cand.job,
            "skip",
            box_state.reason(),
        ));
    }
    if opts.dry_run {
        return Ok(Action::new(
            &cand.sid,
            &cand.job,
            "would-clear",
            format!("dry run, {}", box_state.reason()),
        ));
    }
    let attempts = state
        .sessions
        .get(&cand.sid)
        .map(|e| e.attempts)
        .unwrap_or(0);
    control.reply(&cand.job, "/clear")?;
    state.sessions.insert(
        cand.sid.clone(),
        Entry {
            stage: Stage::ClearSent,
            at: secs(now),
            attempts: attempts + 1,
            job: cand.job.clone(),
            arm_sid: cand.sid.clone(),
        },
    );
    Ok(Action::new(&cand.sid, &cand.job, "clear", "typed /clear"))
}

/// `go` is only earned once the reload is proven to have reached a context:
/// an identity-tier emit row for this arm, and a `hook_success` attachment
/// in the session that received it (spec D9).
fn after_clear(
    control: &control::Control,
    cand: &Candidate,
    entry: &Entry,
    now: SystemTime,
    opts: &InjectOpts,
    state: &mut State,
) -> Result<Action> {
    let since = UNIX_EPOCH + Duration::from_secs(entry.at);
    match delivered_since(&cand.sid, since)? {
        Some(new_sid) => {
            if opts.dry_run {
                return Ok(Action::new(&cand.sid, &cand.job, "would-go", "dry run"));
            }
            control.reply(&cand.job, "go")?;
            state.sessions.insert(
                cand.sid.clone(),
                Entry {
                    stage: Stage::GoSent,
                    at: secs(now),
                    ..entry.clone()
                },
            );
            Ok(Action::new(
                &cand.sid,
                &cand.job,
                "go",
                format!("reload delivered into {}", &new_sid[..8.min(new_sid.len())]),
            ))
        }
        None if secs(now).saturating_sub(entry.at) < CLEAR_GRACE.as_secs() => Ok(Action::new(
            &cand.sid,
            &cand.job,
            "wait",
            "clear sent, reload not proven yet",
        )),
        None if entry.attempts >= MAX_ATTEMPTS => {
            state.sessions.insert(
                cand.sid.clone(),
                Entry {
                    stage: Stage::Failed,
                    at: secs(now),
                    ..entry.clone()
                },
            );
            Ok(Action::new(
                &cand.sid,
                &cand.job,
                "fail",
                "no reload after the retry; left for Mark",
            ))
        }
        None => {
            // The one detector for the guard having been wrong. A real /clear
            // is a command entry; a concatenated one arrives as an ordinary
            // prompt with the command buried inside it.
            if let Some(why) = concatenation_seen(&cand.transcript) {
                disarm_fleet(&why)?;
                state.sessions.insert(
                    cand.sid.clone(),
                    Entry {
                        stage: Stage::Failed,
                        at: secs(now),
                        ..entry.clone()
                    },
                );
                return Ok(Action::new(&cand.sid, &cand.job, "tripwire", why));
            }
            state.sessions.remove(&cand.sid);
            Ok(Action::new(
                &cand.sid,
                &cand.job,
                "retry",
                "no reload after the clear; will re-arm the attempt",
            ))
        }
    }
}

/// The session id that received this arm's reload, if the emit log and the
/// receiving transcript both say so.
fn delivered_since(arm_sid: &str, since: SystemTime) -> Result<Option<String>> {
    let log = paths::emit_log()?;
    let projects = paths::projects()?;
    let since_secs = secs(since);
    let arm8: String = arm_sid.chars().take(8).collect();
    for row in emit::read_rows(&log)?.into_iter().rev() {
        if row.arm != arm8 || !is_identity_tier(&row.tier) {
            continue;
        }
        if emit::parse_ts_secs(&row.ts).unwrap_or(0) < since_secs {
            continue;
        }
        if watch::delivered(
            &projects,
            &row.sid,
            emit::parse_ts_secs(&row.ts).unwrap_or(0),
        ) {
            return Ok(Some(row.sid));
        }
    }
    Ok(None)
}

/// The bundle has to be at least as new as the transcript it summarises,
/// else the clear would load a brief that misses the last few turns.
fn bundle_trails_transcript(sid: &str, transcript: &Path) -> Option<String> {
    let narrative = match paths::bundle(sid) {
        Ok(dir) => dir.join("narrative.md"),
        Err(_) => return Some("bundle path unresolvable".into()),
    };
    let (Ok(nm), Ok(tm)) = (
        std::fs::metadata(&narrative).and_then(|m| m.modified()),
        std::fs::metadata(transcript).and_then(|m| m.modified()),
    ) else {
        return Some("bundle or transcript mtime unreadable".into());
    };
    (nm < tm).then(|| "bundle is older than the transcript".to_string())
}

/// A detached `reseed-here` can still be distilling while the previous
/// sentinel reads fresh, so a clear now would load the older bundle.
fn distill_running(sid: &str) -> bool {
    let short: String = sid.chars().take(8).collect();
    let Ok(out) = std::process::Command::new("pgrep")
        .args(["-fl", "reseed"])
        .output()
    else {
        return false;
    };
    let text = String::from_utf8_lossy(&out.stdout);
    text.lines()
        .any(|l| (l.contains("distill") || l.contains("reseed-here")) && l.contains(&short))
}

/// The nudge band, not just the reset line: the statusline asks for a clear at
/// `early`, so waiting for tier 1 leaves an armed session idle through the whole
/// 240-300k window. An unknown ctx stays out, never guessed into scope.
fn in_clear_band(tier: u8, ctx: Option<u64>, early: u64) -> bool {
    tier > 0 || ctx.is_some_and(|c| c >= early)
}

/// Background sessions past the line that have a bundle armed for them.
fn candidates(now: SystemTime) -> Result<Vec<Candidate>> {
    let pending = paths::pending()?;
    let projects = paths::projects()?;
    let mut out = Vec::new();
    for (sid, job) in watch::live_bg_sessions()? {
        let Some(transcript) = watch::transcript_for(&projects, &sid) else {
            continue;
        };
        let (tier, ctx, _line, early) = usage::context_state(&transcript);
        if !in_clear_band(tier, ctx, early) {
            continue;
        }
        let Some(arm) = sentinel::read(&pending, &sentinel::key(&sid)) else {
            continue;
        };
        let _ = now;
        out.push(Candidate {
            sid,
            job,
            transcript,
            arm,
        });
    }
    Ok(out)
}

/// Nobody can type into a background session without attaching to it, so an
/// attached client is the only window in which the box can change under us.
fn attached_client(job: &str) -> bool {
    std::process::Command::new("pgrep")
        .args(["-f", &format!("claude attach {job}")])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(true)
}

/// Read the box twice, a beat apart. One read proves what was there; two
/// identical reads narrow the window in which a keystroke could land between
/// the look and the paste.
fn prompt_box(job: &str) -> Result<screen::BoxState> {
    let Some(worker) = pty::workers()?.remove(job) else {
        return Ok(screen::BoxState::NotRecognised(
            "no pty socket for this job".into(),
        ));
    };
    let first = screen::classify(&pty::read_screen(&worker)?);
    if !first.may_type() {
        return Ok(first);
    }
    std::thread::sleep(SETTLE);
    let second = screen::classify(&pty::read_screen(&worker)?);
    if second != first {
        return Ok(screen::BoxState::NotRecognised(
            "the box changed between two reads".into(),
        ));
    }
    Ok(second)
}

/// Only the transcript tail matters: a concatenated submit is the newest user
/// message, and these files reach tens of megabytes.
const TAIL_BYTES: u64 = 64 * 1024;

fn concatenation_seen(transcript: &Path) -> Option<String> {
    use std::io::{Read, Seek, SeekFrom};
    let mut file = std::fs::File::open(transcript).ok()?;
    let len = file.metadata().ok()?.len();
    file.seek(SeekFrom::Start(len.saturating_sub(TAIL_BYTES)))
        .ok()?;
    let mut tail = String::new();
    file.read_to_string(&mut tail).ok()?;
    for line in tail.lines().skip(1) {
        let row: serde_json::Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if row.get("type").and_then(|t| t.as_str()) != Some("user") {
            continue;
        }
        let text = parse::content_to_string(&row["message"]["content"]);
        // A genuine slash command is wrapped in a command-name element.
        if text.contains("<command-name>") {
            continue;
        }
        for typed in ["/clear", "/compact"] {
            if let Some(at) = text.find(typed) {
                if at > 0 {
                    return Some(format!("a user message carries {typed} at offset {at}"));
                }
            }
        }
    }
    None
}

/// The tripwire means a session was typed into blind. Stop every host, not
/// just this one, and let Mark find out from the kill file.
fn disarm_fleet(why: &str) -> Result<()> {
    let kill = paths::kill_file()?;
    if let Some(parent) = kill.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&kill, format!("tripwire: {why}\n"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Not a unit test: a hand-run probe that classifies a real session's box.
    /// `RESEED_PROBE_JOB=<short> cargo test -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn probe_a_live_prompt_box() {
        let job = std::env::var("RESEED_PROBE_JOB").expect("set RESEED_PROBE_JOB");
        for one in job.split(',') {
            let verdict = prompt_box(one).expect("reading the box");
            println!("{one}: {verdict:?} -> may_type={}", verdict.may_type());
        }
    }

    #[test]
    fn the_clear_band_starts_at_the_nudge_line_not_the_reset_line() {
        // 260k armed on partly sat idle because tier was still 0 (2026-09-08).
        assert!(in_clear_band(0, Some(260_000), 240_000));
        assert!(in_clear_band(0, Some(240_000), 240_000));
        assert!(in_clear_band(1, Some(300_000), 240_000));
        assert!(!in_clear_band(0, Some(239_999), 240_000));
        assert!(!in_clear_band(0, None, 240_000));
    }

    #[test]
    fn only_identity_tiers_prove_a_reload() {
        assert!(is_identity_tier("1"));
        assert!(is_identity_tier("1b"));
        assert!(is_identity_tier("2"));
        assert!(is_identity_tier("2-jobmatch"));
        assert!(!is_identity_tier("3"));
        assert!(!is_identity_tier("3-declined-cwdonly-fresh"));
    }

    #[test]
    fn state_round_trips_through_disk() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("watch-state.json");
        let mut s = State::default();
        s.sessions.insert(
            "abc".into(),
            Entry {
                stage: Stage::ClearSent,
                at: 42,
                attempts: 1,
                job: "b8816add".into(),
                arm_sid: "abc".into(),
            },
        );
        s.save(&path).unwrap();
        let back = State::load(&path);
        assert_eq!(back.sessions["abc"].stage, Stage::ClearSent);
        assert_eq!(back.sessions["abc"].job, "b8816add");
    }

    #[test]
    fn a_missing_state_file_is_an_empty_state_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        assert!(State::load(&dir.path().join("nope.json"))
            .sessions
            .is_empty());
    }

    #[test]
    fn a_failed_session_is_left_alone_until_the_cooldown_expires() {
        let now = SystemTime::now();
        let fresh = Entry {
            stage: Stage::Failed,
            at: secs(now),
            attempts: 2,
            job: "j".into(),
            arm_sid: "s".into(),
        };
        assert!(!cooled_down(&fresh, now));
        let old = Entry {
            at: secs(now) - COOLDOWN.as_secs() - 1,
            ..fresh
        };
        assert!(cooled_down(&old, now));
    }
}
