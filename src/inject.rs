//! The injection half of `reseed watch`: types `/clear`, waits for the
//! reload to be proven delivered, then types `go`. Background jobs only,
//! because only they have a daemon-owned pty to type into; a terminal
//! session is left to Mark's hands (spec D2).
//!
//! Nothing here runs without `--inject`, and every pass re-reads the kill
//! file, so stopping it is one `touch ~/.claude/reseed/watch.off`.

use crate::{arm, control, emit, paths, pty, screen, sentinel, usage, watch};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
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
    /// Scope a proof run to one job; `None` is the production sweep.
    pub only: Option<String>,
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
    let projects = paths::projects()?;
    let now = SystemTime::now();

    let mut actions = Vec::new();
    let mut seen: BTreeSet<String> = BTreeSet::new();
    for cand in candidates(now)? {
        if opts.only.as_ref().is_some_and(|j| *j != cand.job) {
            continue;
        }
        seen.insert(cand.sid.clone());
        let entry = state.sessions.get(&cand.sid).cloned();
        let action = match entry.map(|e| (e.stage, e)) {
            Some((Stage::Done, _)) => continue,
            Some((Stage::Failed, e)) if !cooled_down(&e, now) => continue,
            Some((Stage::ClearSent, e)) => after_clear(
                &control,
                &InFlight {
                    sid: &cand.sid,
                    job: &cand.job,
                    transcript: &cand.transcript,
                },
                &e,
                now,
                opts,
                &mut state,
            )?,
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
    actions.extend(drive_in_flight(
        &control, &jobs, &projects, &seen, now, opts, &mut state,
    )?);
    if !opts.dry_run {
        state.save(&state_path)?;
    }
    Ok(actions)
}

/// Arms mid-flight that no candidate covered this pass.
fn in_flight_entries(state: &State, seen: &BTreeSet<String>) -> Vec<(String, Entry)> {
    state
        .sessions
        .iter()
        .filter(|(sid, e)| {
            !seen.contains(*sid) && matches!(e.stage, Stage::ClearSent | Stage::GoSent)
        })
        .map(|(sid, e)| (sid.clone(), e.clone()))
        .collect()
}

/// A `/clear` mints a NEW session id, so the arm sid stops being a live
/// session the instant the clear lands and never comes back as a candidate.
/// Without this the state machine sticks at `clear_sent` and `go` is never
/// typed: drive an in-flight arm by its JOB, which does survive the clear.
fn drive_in_flight(
    control: &control::Control,
    jobs: &[control::Job],
    projects: &Path,
    seen: &BTreeSet<String>,
    now: SystemTime,
    opts: &InjectOpts,
    state: &mut State,
) -> Result<Vec<Action>> {
    let pending = in_flight_entries(state, seen);
    let mut actions = Vec::new();
    for (sid, entry) in pending {
        if opts.only.as_ref().is_some_and(|j| *j != entry.job) {
            continue;
        }
        if !jobs.iter().any(|j| j.short == entry.job) {
            continue;
        }
        let Some(transcript) = watch::transcript_for(projects, &sid) else {
            continue;
        };
        actions.push(match entry.stage {
            Stage::ClearSent => after_clear(
                control,
                &InFlight {
                    sid: &sid,
                    job: &entry.job,
                    transcript: &transcript,
                },
                &entry,
                now,
                opts,
                state,
            )?,
            _ => {
                state.sessions.insert(
                    sid.clone(),
                    Entry {
                        stage: Stage::Done,
                        ..entry.clone()
                    },
                );
                Action::new(&sid, &entry.job, "done", "clear and go both delivered")
            }
        });
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
    if job.is_busy() {
        return Ok(Action::new(&cand.sid, &cand.job, "skip", "worker is busy"));
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
    // Staleness is repairable, so it is checked last: an armed bundle goes
    // stale in ten minutes and only a hook re-armed it, which left the
    // injector skipping the same session forever (D4). Re-arm here instead
    // and clear on the next pass, against freshly read state.
    let stale = (!sentinel::is_fresh(&cand.arm, now))
        .then(|| "sentinel is stale".to_string())
        .or_else(|| bundle_trails_transcript(&cand.sid, &cand.transcript));
    if let Some(why) = stale {
        if opts.dry_run {
            return Ok(Action::new(
                &cand.sid,
                &cand.job,
                "would-rearm",
                format!("dry run, {why}"),
            ));
        }
        return Ok(match rearm(&cand.sid, cand.arm.pid) {
            Ok(()) => Action::new(
                &cand.sid,
                &cand.job,
                "rearm",
                format!("{why}, re-armed; clearing next pass"),
            ),
            Err(e) => Action::new(
                &cand.sid,
                &cand.job,
                "skip",
                format!("{why}, re-arm failed: {e}"),
            ),
        });
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

/// What `after_clear` needs about a session. Held separately from `Candidate`
/// because a clear mints a new session id, so the arm sid it carries is no
/// longer a live session by the time the follow-up runs.
struct InFlight<'a> {
    sid: &'a str,
    job: &'a str,
    transcript: &'a Path,
}

/// `go` is only earned once the reload is proven to have reached a context:
/// an identity-tier emit row for this arm, and a `hook_success` attachment
/// in the session that received it (spec D9).
fn after_clear(
    control: &control::Control,
    t: &InFlight,
    entry: &Entry,
    now: SystemTime,
    opts: &InjectOpts,
    state: &mut State,
) -> Result<Action> {
    let (sid, job, transcript) = (t.sid, t.job, t.transcript);
    let since = UNIX_EPOCH + Duration::from_secs(entry.at);
    match delivered_since(sid, since)? {
        Some(new_sid) => {
            if opts.dry_run {
                return Ok(Action::new(sid, job, "would-go", "dry run"));
            }
            control.reply(job, "go")?;
            state.sessions.insert(
                sid.to_string(),
                Entry {
                    stage: Stage::GoSent,
                    at: secs(now),
                    ..entry.clone()
                },
            );
            Ok(Action::new(
                sid,
                job,
                "go",
                format!("reload delivered into {}", &new_sid[..8.min(new_sid.len())]),
            ))
        }
        None if secs(now).saturating_sub(entry.at) < CLEAR_GRACE.as_secs() => Ok(Action::new(
            sid,
            job,
            "wait",
            "clear sent, reload not proven yet",
        )),
        None if entry.attempts >= MAX_ATTEMPTS => {
            state.sessions.insert(
                sid.to_string(),
                Entry {
                    stage: Stage::Failed,
                    at: secs(now),
                    ..entry.clone()
                },
            );
            Ok(Action::new(
                sid,
                job,
                "fail",
                "no reload after the retry; left for Mark",
            ))
        }
        None => {
            // The one detector for the guard having been wrong. A real /clear
            // is a command entry; a concatenated one arrives as an ordinary
            // prompt with the command buried inside it.
            if let Some(why) = concatenation_seen(transcript, since) {
                disarm_fleet(&why)?;
                state.sessions.insert(
                    sid.to_string(),
                    Entry {
                        stage: Stage::Failed,
                        at: secs(now),
                        ..entry.clone()
                    },
                );
                return Ok(Action::new(sid, job, "tripwire", why));
            }
            state.sessions.remove(sid);
            Ok(Action::new(
                sid,
                job,
                "retry",
                "no reload after the clear; will re-arm the attempt",
            ))
        }
    }
}

/// The session id that received this arm's reload, if the emit log and the
/// receiving transcript both say so.
/// `emit.log` truncates `sid` to eight chars but writes `arm` whole, so an
/// eight-char equality test never matched a real row and no clear ever
/// earned its `go`. Prefix, to accept both shapes.
fn arm_matches(row_arm: &str, arm8: &str) -> bool {
    row_arm.starts_with(arm8)
}

fn delivered_since(arm_sid: &str, since: SystemTime) -> Result<Option<String>> {
    let log = paths::emit_log()?;
    let projects = paths::projects()?;
    let since_secs = secs(since);
    let arm8: String = arm_sid.chars().take(8).collect();
    for row in emit::read_rows(&log)?.into_iter().rev() {
        if !arm_matches(&row.arm, &arm8) || !is_identity_tier(&row.tier) {
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

/// Distil the session again so its bundle is current, then let the next pass
/// clear it. Synchronous: `arm::run` writes the sentinel last, so on return
/// the bundle is whole rather than half-written.
fn rearm(sid: &str, pid: Option<u32>) -> Result<()> {
    arm::run(arm::ArmOpts {
        sid: Some(sid.to_string()),
        quiet: true,
        pid,
    })
    .map(|_| ())
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

fn concatenation_seen(transcript: &Path, since: SystemTime) -> Option<String> {
    use std::io::{Read, Seek, SeekFrom};
    let mut file = std::fs::File::open(transcript).ok()?;
    let len = file.metadata().ok()?.len();
    file.seek(SeekFrom::Start(len.saturating_sub(TAIL_BYTES)))
        .ok()?;
    let mut tail = String::new();
    file.read_to_string(&mut tail).ok()?;
    let since_secs = secs(since);
    for line in tail.lines().skip(1) {
        let row: serde_json::Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if row.get("type").and_then(|t| t.as_str()) != Some("user") {
            continue;
        }
        // Tool output is the harness quoting itself, and reseed's own nudge
        // text rides in on it. Only a keystroke can concatenate.
        if row.get("toolUseResult").is_some() {
            continue;
        }
        // Only this clear's own window. An untimestamped row is foreign, not
        // a submit: every real user row carries one.
        let fresh = row
            .get("timestamp")
            .and_then(|t| t.as_str())
            .and_then(emit::parse_ts_secs)
            .is_some_and(|ts| ts >= since_secs);
        if !fresh {
            continue;
        }
        let text = match typed_text(&row["message"]["content"]) {
            Some(text) => text,
            None => continue,
        };
        // A genuine slash command is wrapped in a command-name element.
        if text.contains("<command-name>") {
            continue;
        }
        let text = text.trim_end();
        for typed in ["/clear", "/compact"] {
            if text.ends_with(typed) && text.len() > typed.len() {
                return Some(format!("a typed prompt ends with {typed}"));
            }
        }
    }
    None
}

/// The prompt as a human typed it, or `None` for anything the harness built.
/// A paste splices at the cursor and the return submits, so the concatenated
/// shape is always a plain text row ending in the command.
fn typed_text(content: &serde_json::Value) -> Option<String> {
    match content {
        serde_json::Value::String(s) => Some(s.clone()),
        serde_json::Value::Array(items) => {
            let mut out = Vec::with_capacity(items.len());
            for block in items {
                if block.get("type").and_then(|t| t.as_str()) != Some("text") {
                    return None;
                }
                out.push(block.get("text").and_then(|t| t.as_str())?.to_string());
            }
            Some(out.join("\n"))
        }
        _ => None,
    }
}

/// The tripwire means a session was typed into blind. Stop this host and let
/// Mark find out from the kill file; there is no fleet propagation.
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

    fn entry(stage: Stage, job: &str) -> Entry {
        Entry {
            stage,
            at: 0,
            attempts: 1,
            job: job.into(),
            arm_sid: "a0e9d562-111e".into(),
        }
    }

    #[test]
    fn arm_row_is_matched_whole_or_truncated() {
        assert!(arm_matches(
            "a0e9d562-111e-4a48-a876-9fd9fec11ff0",
            "a0e9d562"
        ));
        assert!(arm_matches("a0e9d562", "a0e9d562"));
        assert!(!arm_matches(
            "b8816add-0000-0000-0000-000000000000",
            "a0e9d562"
        ));
    }

    #[test]
    fn a_cleared_arm_is_still_driven_once_it_stops_being_a_candidate() {
        let mut state = State::default();
        state
            .sessions
            .insert("a0e9d562-111e".into(), entry(Stage::ClearSent, "a0e9d562"));
        state
            .sessions
            .insert("done-one".into(), entry(Stage::Done, "zzz"));
        let picked = in_flight_entries(&state, &BTreeSet::new());
        assert_eq!(picked.len(), 1);
        assert_eq!(picked[0].0, "a0e9d562-111e");
    }

    #[test]
    fn an_arm_the_candidate_pass_already_handled_is_not_driven_twice() {
        let mut state = State::default();
        state
            .sessions
            .insert("a0e9d562-111e".into(), entry(Stage::ClearSent, "a0e9d562"));
        let seen: BTreeSet<String> = ["a0e9d562-111e".to_string()].into_iter().collect();
        assert!(in_flight_entries(&state, &seen).is_empty());
    }

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

    /// Fixtures are the real row shapes: a typed prompt is a plain string or
    /// text blocks, and hook output arrives as a `tool_result` with a
    /// `toolUseResult` sibling.
    fn transcript_with(rows: &[&str]) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.jsonl");
        let mut body = String::from("{\"type\":\"summary\"}\n");
        for row in rows {
            body.push_str(row);
            body.push('\n');
        }
        std::fs::write(&path, body).unwrap();
        (dir, path)
    }

    fn at(ts: &str) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(emit::parse_ts_secs(ts).unwrap())
    }

    #[test]
    fn the_reseed_nudge_in_tool_output_is_not_a_concatenation() {
        // The bug that kill-switched partly: the injector's own Stop-hook text
        // rides in on a tool result and used to trip the tripwire.
        let row = r#"{"type":"user","timestamp":"2026-09-08T05:27:06Z","toolUseResult":{"stdout":"ok"},"message":{"content":[{"type":"tool_result","tool_use_id":"t1","content":"bundle armed: /clear then 'go' at the next natural break"}]}}"#;
        let (_d, path) = transcript_with(&[row]);
        assert_eq!(concatenation_seen(&path, at("2026-09-08T05:20:00Z")), None);
    }

    #[test]
    fn a_typed_prompt_ending_in_clear_is_a_concatenation() {
        let row = r#"{"type":"user","timestamp":"2026-09-08T05:27:06Z","message":{"content":"build drill 11 from the spec/clear"}}"#;
        let (_d, path) = transcript_with(&[row]);
        let why = concatenation_seen(&path, at("2026-09-08T05:20:00Z")).unwrap();
        assert!(why.contains("/clear"), "{why}");
    }

    #[test]
    fn a_real_slash_command_is_not_a_concatenation() {
        let row = r#"{"type":"user","timestamp":"2026-09-08T05:27:06Z","message":{"content":"<command-name>/clear</command-name>\n\ngo"}}"#;
        let (_d, path) = transcript_with(&[row]);
        assert_eq!(concatenation_seen(&path, at("2026-09-08T05:20:00Z")), None);
    }

    #[test]
    fn a_prompt_merely_discussing_clear_is_not_a_concatenation() {
        let row = r#"{"type":"user","timestamp":"2026-09-08T05:27:06Z","message":{"content":[{"type":"text","text":"why did /clear not fire on this session"}]}}"#;
        let (_d, path) = transcript_with(&[row]);
        assert_eq!(concatenation_seen(&path, at("2026-09-08T05:20:00Z")), None);
    }

    #[test]
    fn a_concatenation_from_before_this_clear_is_out_of_window() {
        let row = r#"{"type":"user","timestamp":"2026-09-08T05:10:00Z","message":{"content":"old draft/clear"}}"#;
        let (_d, path) = transcript_with(&[row]);
        assert_eq!(concatenation_seen(&path, at("2026-09-08T05:20:00Z")), None);
    }
}
