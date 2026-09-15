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
/// Between a keystroke and reading its echo back; CC redraws well inside this.
const ECHO: Duration = Duration::from_millis(if cfg!(test) { 20 } else { 800 });
const ECHO_READS: usize = 3;
const DEL: u8 = 0x7f;

/// A stage that never completes is retried once, then parked this long so a
/// misjudged session is not typed into again and again (spec D6).
const COOLDOWN: Duration = Duration::from_secs(15 * 60);
/// How long a clear gets to prove its reload before recovery is the right
/// move. Mark's call 2026-09-09: 10 s, not 45. The 45 was sized off the reload
/// hook's 30 s timeout, but the sweep only runs every 30 s anyway, so the
/// grace only ever decided whether the verdict came on the FIRST tick after
/// the clear or the second. What made waiting safe to shorten is the
/// session-id guard in `recover`: a clear that has not actually landed is now
/// refused on the daemon's own view rather than on elapsed time.
const CLEAR_GRACE: Duration = Duration::from_secs(10);
const MAX_ATTEMPTS: u32 = 2;

/// Tiers that mean the hook matched this session by identity and loaded its
/// bundle. `3-*` is the cwd heuristic, which also logs `arm=` and can inject
/// nothing, so it is not proof (spec D9).
fn is_identity_tier(tier: &str) -> bool {
    // The `-stale` variants are the SAME identity match behind an opt-out
    // wrapper, and the wrapper still puts the reload in the new context. They
    // are 51 of ~240 rows on this host, and rejecting them made a delivered
    // reload read as undelivered - harmless when that only skipped `go`, a
    // double paste now that it reaches `recover`.
    let tier = tier.strip_suffix("-stale").unwrap_or(tier);
    matches!(tier, "1" | "1b" | "2") || tier.starts_with("2-jobmatch")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Stage {
    ClearSent,
    GoSent,
    /// The hook dropped the arm and the injector delivered the reload itself.
    Recovered,
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

    let pending_dir = paths::pending()?;
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
                &jobs,
                &InFlight {
                    sid: &cand.sid,
                    job: &cand.job,
                    transcript: Some(&cand.transcript),
                    pending: &pending_dir,
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
            _ => send_clear(&jobs, &cand, now, opts, &mut state)?,
        };
        actions.push(action);
    }
    actions.extend(drive_in_flight(
        &control, &jobs, &projects, &seen, now, opts, &mut state,
    )?);
    actions.extend(drive_stranded(
        &control,
        &jobs,
        &pending_dir,
        &seen,
        now,
        opts,
        &mut state,
    )?);
    if !opts.dry_run {
        state.save(&state_path)?;
    }
    Ok(actions)
}

/// An arm whose session was cleared but whose reload never landed, left
/// behind by a build that dropped the state entry on `retry`. The state map
/// is the only handle `drive_in_flight` has, so those are invisible to it and
/// stayed stranded for good; the arm itself is the durable evidence.
///
/// The discriminator is the daemon's own view: the job is still alive, the
/// arm names a session that job no longer runs, and the arm was never
/// consumed. A hook that had delivered would have deleted it.
fn drive_stranded(
    control: &control::Control,
    jobs: &[control::Job],
    pending: &Path,
    seen: &BTreeSet<String>,
    now: SystemTime,
    opts: &InjectOpts,
    state: &mut State,
) -> Result<Vec<Action>> {
    let mut actions = Vec::new();
    for arm in sentinel::list(pending) {
        let Some(sid) = arm.path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let Some(armjob) = arm.job.as_deref().map(str::trim).filter(|j| !j.is_empty()) else {
            continue; // a foreground TUI arm: no job to type into
        };
        if opts.only.as_ref().is_some_and(|j| j != armjob) {
            continue;
        }
        // Anything the state map knows about is already driven above, and
        // that is what keeps the 45s window between `/clear` and the hook's
        // consume from being read as a strand.
        if seen.contains(sid) || state.sessions.contains_key(sid) {
            continue;
        }
        let Some(job) = jobs.iter().find(|j| j.short == armjob) else {
            continue; // the job is gone, so there is nothing to deliver into
        };
        // The clear is what replaced the session id. Same id still running
        // means no clear happened and the context is intact - never type a
        // reload into a session that never lost one.
        if job.session_id.as_deref().unwrap_or(sid) == sid {
            continue;
        }
        let entry = Entry {
            stage: Stage::ClearSent,
            at: secs(arm.mtime),
            attempts: 1,
            job: armjob.to_string(),
            arm_sid: sid.to_string(),
        };
        let t = InFlight {
            sid,
            job: armjob,
            transcript: None,
            pending,
        };
        actions.push(recover(control, jobs, &t, &entry, now, opts, state)?);
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
    let pending_dir = paths::pending()?;
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
                jobs,
                &InFlight {
                    sid: &sid,
                    job: &entry.job,
                    transcript: Some(&transcript),
                    pending: &pending_dir,
                },
                &entry,
                now,
                opts,
                state,
            )?,
            _ => {
                let closed = match entry.stage {
                    Stage::Recovered => "clear delivered, reload handed over by the injector",
                    _ => "clear and go both delivered",
                };
                state.sessions.insert(
                    sid.clone(),
                    Entry {
                        stage: Stage::Done,
                        ..entry.clone()
                    },
                );
                Action::new(&sid, &entry.job, "done", closed)
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
    // Read-back cannot undo a return Mark presses between our keystrokes and the read.
    if attached_client(&cand.job) {
        return Ok(Action::new(
            &cand.sid,
            &cand.job,
            "skip",
            "a client is attached, so someone may be typing",
        ));
    }
    let box_state = prompt_box(&cand.job, screen::Dim::Ignored)?;
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
        return Ok(match rearm(&cand.sid, cand.arm.pid, job.cwd.as_deref()) {
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
    let Some(worker) = pty::workers()?.remove(&cand.job) else {
        return Ok(Action::new(
            &cand.sid,
            &cand.job,
            "skip",
            "no pty socket for this job",
        ));
    };
    let attempts = state
        .sessions
        .get(&cand.sid)
        .map(|e| e.attempts)
        .unwrap_or(0);
    let typed = type_command(&worker, "/clear")?;
    let exact = typed == Typed::Submitted;
    // A refusal parks the session for the cooldown: a dictation interim reads
    // empty on every pass, and typing into it each tick is the harm.
    state.sessions.insert(
        cand.sid.clone(),
        Entry {
            stage: if exact {
                Stage::ClearSent
            } else {
                Stage::Failed
            },
            at: secs(now),
            attempts: attempts + u32::from(exact),
            job: cand.job.clone(),
            arm_sid: cand.sid.clone(),
        },
    );
    let did = if exact { "clear" } else { "refuse" };
    Ok(Action::new(
        &cand.sid,
        &cand.job,
        did,
        typed.reason("/clear"),
    ))
}

/// What `after_clear` needs about a session. Held separately from `Candidate`
/// because a clear mints a new session id, so the arm sid it carries is no
/// longer a live session by the time the follow-up runs.
struct InFlight<'a> {
    sid: &'a str,
    job: &'a str,
    /// `None` on the stranded-arm path, which has no transcript to read and
    /// no concatenation to check: the clear it is recovering from happened
    /// under a previous run of this binary.
    transcript: Option<&'a Path>,
    pending: &'a Path,
}

/// `go` is only earned once the reload is proven to have reached a context:
/// an identity-tier emit row for this arm, and a `hook_success` attachment
/// in the session that received it (spec D9).
fn after_clear(
    control: &control::Control,
    jobs: &[control::Job],
    t: &InFlight,
    entry: &Entry,
    now: SystemTime,
    opts: &InjectOpts,
    state: &mut State,
) -> Result<Action> {
    let (sid, job, transcript) = (t.sid, t.job, t.transcript);
    let since = UNIX_EPOCH + Duration::from_secs(entry.at);
    match delivered_since(sid, since)? {
        Some((new_sid, landing)) => {
            if opts.dry_run {
                return Ok(Action::new(sid, job, "would-go", "dry run"));
            }
            // `go` is typed and read back like the clear, under the same attach
            // guard. A draft there means Mark took over.
            if attached_client(job) {
                return Ok(Action::new(
                    sid,
                    job,
                    "wait",
                    "reload delivered, go held: a client is attached",
                ));
            }
            let box_state = prompt_box(job, screen::Dim::Ignored)?;
            let worker = pty::workers()?.remove(job);
            let (Some(worker), true) = (worker, box_state.may_type()) else {
                let why = format!("reload delivered, go held: {}", box_state.reason());
                if !matches!(box_state, screen::BoxState::Draft { .. }) {
                    return Ok(Action::new(sid, job, "wait", why));
                }
                state.sessions.insert(
                    sid.to_string(),
                    Entry {
                        stage: Stage::Done,
                        at: secs(now),
                        ..entry.clone()
                    },
                );
                return Ok(Action::new(sid, job, "refuse", why));
            };
            let typed = type_command(&worker, go_text(&landing))?;
            if typed != Typed::Submitted {
                state.sessions.insert(
                    sid.to_string(),
                    Entry {
                        stage: Stage::Done,
                        at: secs(now),
                        ..entry.clone()
                    },
                );
                let why = format!("reload delivered, go left for Mark: {}", typed.reason("go"));
                return Ok(Action::new(sid, job, "refuse", why));
            }
            state.sessions.insert(
                sid.to_string(),
                Entry {
                    stage: Stage::GoSent,
                    at: secs(now),
                    ..entry.clone()
                },
            );
            let new8 = &new_sid[..8.min(new_sid.len())];
            let why = match landing {
                watch::Landing::Inline => format!("reload delivered into {new8}"),
                watch::Landing::Persisted(_) => {
                    format!("reload persisted in {new8}; preview carries the pointer, go names the file")
                }
            };
            Ok(Action::new(sid, job, "go", why))
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
            if let Some(why) = transcript.and_then(|p| concatenation_seen(p, since)) {
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
            recover(control, jobs, t, entry, now, opts, state)
        }
    }
}

/// The `/clear` landed but the hook never consumed the arm, so the fresh
/// context got nothing. A cleared sid is dead the instant it is cleared and
/// can never come back as a candidate, so the old "retry" was a silent
/// give-up that left the session empty for good - three of them on partly on
/// 2026-09-09. Hand the armed reload to the new context instead: same job,
/// same socket, and the box is empty because this injector emptied it.
fn recover(
    control: &control::Control,
    jobs: &[control::Job],
    t: &InFlight,
    entry: &Entry,
    now: SystemTime,
    opts: &InjectOpts,
    state: &mut State,
) -> Result<Action> {
    let Some(arm) = sentinel::read(t.pending, &sentinel::key(t.sid)) else {
        // The hook consumed it, so re-typing would double the reload. Kept as
        // Failed, not removed: removal hid two stuck sessions on 2026-09-14.
        state.sessions.insert(
            t.sid.to_string(),
            Entry {
                stage: Stage::Failed,
                at: secs(now),
                ..entry.clone()
            },
        );
        return Ok(Action::new(
            t.sid,
            t.job,
            "fail",
            "arm already consumed but its reload never proved it landed; left for Mark",
        ));
    };
    let live = jobs.iter().find(|j| j.short == t.job);
    // The daemon's own view of which session this job runs. An accepted
    // `/clear` that the TUI never executed is indistinguishable from a clear
    // whose hook declined - both leave no emit row - and the difference
    // matters here, because recovery types a whole reload rather than six
    // characters. If the job still runs the session we cleared, no clear
    // happened and that context is intact: never paste into it.
    if live.and_then(|j| j.session_id.as_deref()).unwrap_or(t.sid) == t.sid {
        return Ok(Action::new(
            t.sid,
            t.job,
            "wait",
            "the clear has not landed yet; the job still runs this session",
        ));
    }
    if live.is_some_and(control::Job::is_busy) {
        return Ok(Action::new(
            t.sid,
            t.job,
            "wait",
            "reload never landed, worker busy; recovering next pass",
        ));
    }
    // The reload is a multi-line paste plus return, so it cannot be typed and
    // read back like `/clear`: keep the attached guard and read dim text as typed.
    if attached_client(t.job) {
        return Ok(Action::new(
            t.sid,
            t.job,
            "wait",
            "reload never landed, a client is attached; recovering next pass",
        ));
    }
    let box_state = prompt_box(t.job, screen::Dim::Typed)?;
    if !box_state.may_type() {
        return Ok(Action::new(
            t.sid,
            t.job,
            "wait",
            format!("reload never landed, {}", box_state.reason()),
        ));
    }
    if opts.dry_run {
        return Ok(Action::new(
            t.sid,
            t.job,
            "would-recover",
            "dry run, reload never landed",
        ));
    }
    control.reply(t.job, arm.reload.trim())?;
    // Consumed here because the hook never will: a surviving arm is what a
    // later /clear in this root would GC, or worse, cross-load.
    sentinel::remove(&arm);
    let key = sentinel::key(t.sid);
    let _ = emit::log(
        &paths::emit_log()?,
        "recover",
        t.sid,
        t.job,
        arm.cwd.as_deref().unwrap_or(""),
        &key,
        "?",
    );
    state.sessions.insert(
        t.sid.to_string(),
        Entry {
            stage: Stage::Recovered,
            at: secs(now),
            attempts: entry.attempts + 1,
            ..entry.clone()
        },
    );
    Ok(Action::new(
        t.sid,
        t.job,
        "recover",
        "hook dropped the arm; delivered the reload by hand",
    ))
}

/// The session id that received this arm's reload, if the emit log and the
/// receiving transcript both say so.
/// `emit.log` truncates `sid` to eight chars but writes `arm` whole, so an
/// eight-char equality test never matched a real row and no clear ever
/// earned its `go`. Prefix, to accept both shapes.
fn arm_matches(row_arm: &str, arm8: &str) -> bool {
    row_arm.starts_with(arm8)
}

/// The kickoff typed after a proven reload. A persisted reload reached the
/// context as a preview whose pointer already says how to read the bundle, so
/// the go defers to it and names no path: the line must fit one box row to be read back.
fn go_text(landing: &watch::Landing) -> &'static str {
    match landing {
        watch::Landing::Inline => "go",
        watch::Landing::Persisted(_) => {
            "go: the reload was too large to inline; follow the pointer in its preview above"
        }
    }
}

fn delivered_since(arm_sid: &str, since: SystemTime) -> Result<Option<(String, watch::Landing)>> {
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
        if let Some(landing) = watch::landing(
            &projects,
            &row.sid,
            emit::parse_ts_secs(&row.ts).unwrap_or(0),
            arm_sid,
        ) {
            return Ok(Some((row.sid, landing)));
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
fn rearm(sid: &str, pid: Option<u32>, cwd: Option<&str>) -> Result<()> {
    arm::run(arm::ArmOpts {
        sid: Some(sid.to_string()),
        quiet: true,
        pid,
        cwd: cwd.map(str::to_string),
    })
    .map(|_| ())
}

/// An attached client is the window in which the box can change under a paste.
/// Leaky: an attach from the agents view runs in-process and spawns no `claude attach`.
fn attached_client(job: &str) -> bool {
    std::process::Command::new("pgrep")
        .args(["-f", &format!("claude attach {job}")])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(true)
}

/// What `type_command` did with the text it typed.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Typed {
    /// Read back whole and alone in the box; return pressed.
    Submitted,
    /// Not submitted; deleted again and the box row reads as it did before.
    Removed(String),
    /// Not submitted, and not provably gone: the box may still hold it.
    Left(String),
}

impl Typed {
    fn reason(&self, label: &str) -> String {
        match self {
            Typed::Submitted => {
                format!("typed {label}, read it back alone in the box, pressed return")
            }
            Typed::Removed(why) => format!("typed {label}, {why}; deleted it again"),
            Typed::Left(why) => format!("typed {label}, {why}; left for Mark"),
        }
    }
}

/// Type `text` into a box already read as empty, read it back whole, and press
/// return only on an exact echo. Any other outcome deletes it when the read
/// proves it sits right before the caret, and never submits.
fn type_command(worker: &pty::Worker, text: &str) -> Result<Typed> {
    let before = screen::row_text(&pty::read_screen(worker)?);
    pty::type_input(worker, text.as_bytes())?;
    let mut echo = screen::Echo::Other("not read".into());
    for _ in 0..ECHO_READS {
        std::thread::sleep(ECHO);
        echo = read_echo(worker, text);
        if !matches!(echo, screen::Echo::Other(_)) {
            break;
        }
    }
    match echo {
        screen::Echo::Exact => {
            let still_exact = |s: &[u8]| screen::echo(s, text) == screen::Echo::Exact;
            if pty::type_if(worker, b"\r", still_exact)? {
                Ok(Typed::Submitted)
            } else {
                remove(
                    worker,
                    text,
                    before,
                    "read it back alone, then the box moved before return",
                )
            }
        }
        screen::Echo::Glued => remove(worker, text, before, "read it back beside other text"),
        screen::Echo::Other(why) => {
            // A slow repaint is the usual cause; an echo that shows up late is
            // still ours to delete, but too late to trust with a return.
            std::thread::sleep(ECHO * 2);
            match read_echo(worker, text) {
                screen::Echo::Other(_) => Ok(Typed::Left(why)),
                _ => remove(
                    worker,
                    text,
                    before,
                    &format!("{why}, then it showed up late"),
                ),
            }
        }
    }
}

fn read_echo(worker: &pty::Worker, text: &str) -> screen::Echo {
    match pty::read_screen(worker) {
        Ok(stream) => screen::echo(&stream, text),
        Err(e) => screen::Echo::Other(format!("reading the screen failed: {e}")),
    }
}

/// Delete `text` from right before the caret and check the row is back.
fn remove(worker: &pty::Worker, text: &str, before: Option<String>, why: &str) -> Result<Typed> {
    let still_ours = |s: &[u8]| {
        matches!(
            screen::echo(s, text),
            screen::Echo::Exact | screen::Echo::Glued
        )
    };
    let mut deleted = false;
    for _ in 0..ECHO_READS {
        if pty::type_if(worker, &vec![DEL; text.len()], still_ours)? {
            deleted = true;
            break;
        }
        std::thread::sleep(ECHO);
    }
    if !deleted {
        return Ok(Typed::Left(format!(
            "{why}; the box moved before the delete, so nothing was deleted"
        )));
    }
    std::thread::sleep(ECHO);
    let after = pty::read_screen(worker)
        .ok()
        .and_then(|s| screen::row_text(&s));
    Ok(if before.is_some() && after == before {
        Typed::Removed(why.to_string())
    } else {
        Typed::Left(format!(
            "{why}; deleted it, but the box row does not read as before"
        ))
    })
}

/// Read the box twice, a beat apart. One read proves what was there; two
/// identical reads narrow the window in which a keystroke could land between
/// the look and the paste.
fn prompt_box(job: &str, dim: screen::Dim) -> Result<screen::BoxState> {
    let Some(worker) = pty::workers()?.remove(job) else {
        return Ok(screen::BoxState::NotRecognised(
            "no pty socket for this job".into(),
        ));
    };
    let first = screen::classify(&pty::read_screen(&worker)?, dim);
    if !first.may_type() {
        return Ok(first);
    }
    std::thread::sleep(SETTLE);
    let second = screen::classify(&pty::read_screen(&worker)?, dim);
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
            for dim in [screen::Dim::Typed, screen::Dim::Ignored] {
                let verdict = prompt_box(one, dim).expect("reading the box");
                println!(
                    "{one} {dim:?}: {verdict:?} -> may_type={}",
                    verdict.may_type()
                );
            }
        }
    }

    /// Not a unit test: types `RESEED_TYPE_TEXT` (default `/clear`) into a THROWAWAY
    /// job through the production path. `RESEED_TYPE_JOB=<short> cargo test -- --ignored`
    #[test]
    #[ignore]
    fn probe_type_into_a_live_job() {
        let job = std::env::var("RESEED_TYPE_JOB").expect("set RESEED_TYPE_JOB");
        let text = std::env::var("RESEED_TYPE_TEXT").unwrap_or_else(|_| "/clear".into());
        let worker = pty::workers().unwrap().remove(&job).expect("no worker");
        let typed = type_command(&worker, &text).expect("typing");
        println!("{job}: {typed:?} -> {}", typed.reason(&text));
    }

    /// A pty host that keeps one prompt box. Keystrokes edit the draft, return
    /// submits it, and each connection gets the box replayed first. A suggestion
    /// is painted only while the draft is empty; an interim and `under`, a draft's
    /// second line, are always painted.
    struct FakeBox {
        worker: pty::Worker,
        draft: std::sync::Arc<std::sync::Mutex<String>>,
        submitted: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
        _dir: tempfile::TempDir,
    }

    fn fake_box(
        draft: &str,
        suggestion: &'static str,
        interim: &'static str,
        under: &'static str,
    ) -> FakeBox {
        racing_box(draft, suggestion, interim, under, None)
    }

    /// `race` = (n, keys): the user types `keys` just as connection `n` opens,
    /// after every earlier read and before that connection's replay.
    fn racing_box(
        draft: &str,
        suggestion: &'static str,
        interim: &'static str,
        under: &'static str,
        race: Option<(usize, &'static str)>,
    ) -> FakeBox {
        use std::io::{Read, Write};
        use std::sync::{Arc, Mutex};
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("pty.sock");
        let listener = std::os::unix::net::UnixListener::bind(&sock).unwrap();
        let draft = Arc::new(Mutex::new(draft.to_string()));
        let submitted = Arc::new(Mutex::new(Vec::new()));
        let (d, s) = (draft.clone(), submitted.clone());
        std::thread::spawn(move || {
            for (n, conn) in listener.incoming().enumerate() {
                let Ok(mut conn) = conn else { return };
                let mut head = [0u8; 5];
                conn.read_exact(&mut head).unwrap();
                let mut auth =
                    vec![0u8; u32::from_be_bytes(head[..4].try_into().unwrap()) as usize];
                conn.read_exact(&mut auth).unwrap();
                assert_eq!(head[4], 1, "auth first");
                if let Some((_, keys)) = race.filter(|&(at, _)| at == n) {
                    d.lock().unwrap().push_str(keys);
                }
                let text = d.lock().unwrap().clone();
                let dim = if text.is_empty() { suggestion } else { "" };
                let screen = format!(
                    "\u{1b}[64;1H\u{1b}[61;1H\u{1b}[K\u{276f}\u{a0}{text}\u{1b}[2m{dim}{interim}\u{1b}[22m\u{1b}[62;1H  {under}\u{1b}[63;1H{}\u{1b}[61;{}H",
                    "\u{2500}".repeat(40),
                    3 + text.chars().count()
                );
                let mut replay = (screen.len() as u32).to_be_bytes().to_vec();
                replay.push(0);
                replay.extend_from_slice(screen.as_bytes());
                conn.write_all(&replay).unwrap();
                conn.shutdown(std::net::Shutdown::Write).unwrap();
                let mut rest = Vec::new();
                conn.read_to_end(&mut rest).unwrap();
                for (kind, keys) in pty_frames(&rest) {
                    assert_eq!(kind, 0, "only keystrokes after auth");
                    for &k in &keys {
                        let mut text = d.lock().unwrap();
                        match k {
                            DEL => {
                                text.pop();
                            }
                            b'\r' => s.lock().unwrap().push(std::mem::take(&mut *text)),
                            k => text.push(char::from(k)),
                        }
                    }
                }
            }
        });
        FakeBox {
            worker: pty::Worker {
                pty_sock: sock,
                pty_auth: "tok".into(),
            },
            draft,
            submitted,
            _dir: dir,
        }
    }

    fn pty_frames(buf: &[u8]) -> Vec<(u8, Vec<u8>)> {
        let mut out = Vec::new();
        let mut i = 0;
        while i + 5 <= buf.len() {
            let n = u32::from_be_bytes(buf[i..i + 4].try_into().unwrap()) as usize;
            out.push((buf[i + 4], buf[i + 5..i + 5 + n].to_vec()));
            i += 5 + n;
        }
        out
    }

    impl FakeBox {
        /// The host serves connections in order, so a replay proves the last
        /// keystroke connection was fully applied.
        fn settled(&self) {
            pty::read_screen(&self.worker).unwrap();
        }
        fn draft(&self) -> String {
            self.settled();
            self.draft.lock().unwrap().clone()
        }
        fn submitted(&self) -> Vec<String> {
            self.settled();
            self.submitted.lock().unwrap().clone()
        }
    }

    #[test]
    fn a_clear_typed_into_an_empty_box_is_returned() {
        let fake = fake_box("", "", "", "");
        assert_eq!(
            type_command(&fake.worker, "/clear").unwrap(),
            Typed::Submitted
        );
        assert_eq!(fake.submitted(), ["/clear"]);
    }

    #[test]
    fn typing_replaces_a_prompt_suggestion_and_the_clear_goes_through() {
        let fake = fake_box("", "go, use dev14", "", "");
        assert_eq!(
            type_command(&fake.worker, "/clear").unwrap(),
            Typed::Submitted
        );
        assert_eq!(fake.submitted(), ["/clear"]);
    }

    #[test]
    fn a_dictation_interim_gets_the_clear_deleted_and_nothing_submitted() {
        let fake = fake_box("", "", "and then ship the fix", "");
        assert!(matches!(
            type_command(&fake.worker, "/clear").unwrap(),
            Typed::Removed(_)
        ));
        assert!(fake.submitted().is_empty());
        assert_eq!(fake.draft(), "");
    }

    #[test]
    fn a_draft_typed_since_the_empty_read_is_restored_exactly() {
        let fake = fake_box("half typed draft", "", "", "");
        assert!(matches!(
            type_command(&fake.worker, "/clear").unwrap(),
            Typed::Removed(_)
        ));
        assert!(fake.submitted().is_empty());
        assert_eq!(fake.draft(), "half typed draft");
    }

    #[test]
    fn a_clear_on_the_blank_first_line_of_a_draft_is_never_returned() {
        // Live on CC 2.1.272: this ran `/clear` with the second line as its args.
        let fake = fake_box("", "", "", "second line draft");
        assert!(matches!(
            type_command(&fake.worker, "/clear").unwrap(),
            Typed::Removed(_)
        ));
        assert!(fake.submitted().is_empty());
        assert_eq!(fake.draft(), "");
    }

    // Connections in order: 0 the pre-type row, 1 the typing, 2 the read-back,
    // 3 the return or the first delete.
    #[test]
    fn a_keystroke_after_the_read_back_stops_the_return() {
        let fake = racing_box("", "", "", "", Some((3, "x")));
        assert!(matches!(
            type_command(&fake.worker, "/clear").unwrap(),
            Typed::Left(_)
        ));
        assert!(fake.submitted().is_empty());
        assert_eq!(fake.draft(), "/clearx");
    }

    #[test]
    fn a_keystroke_after_the_read_back_stops_the_delete() {
        let fake = racing_box("half typed draft", "", "", "", Some((3, "!")));
        assert!(matches!(
            type_command(&fake.worker, "/clear").unwrap(),
            Typed::Left(_)
        ));
        assert!(fake.submitted().is_empty());
        assert_eq!(fake.draft(), "half typed draft/clear!");
    }

    #[test]
    fn a_long_go_is_read_back_whole_before_return() {
        let fake = fake_box("", "", "", "");
        let text = go_text(&watch::Landing::Persisted(None));
        assert_eq!(type_command(&fake.worker, text).unwrap(), Typed::Submitted);
        assert_eq!(fake.submitted(), [text]);
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

    fn in_flight<'a>(sid: &'a str, job: &'a str, t: &'a Path, pending: &'a Path) -> InFlight<'a> {
        InFlight {
            sid,
            job,
            transcript: Some(t),
            pending,
        }
    }

    fn job(short: &str, tempo: &str, session_id: Option<&str>) -> control::Job {
        control::Job {
            short: short.into(),
            session_id: session_id.map(str::to_string),
            tempo: Some(tempo.into()),
            state: None,
            cwd: None,
            pid: None,
        }
    }

    /// The hook DID consume the arm and only the proof failed, so re-typing
    /// would deliver the reload twice into the same context.
    #[test]
    fn a_consumed_arm_is_never_re_delivered() {
        let dir = tempfile::tempdir().unwrap();
        let control = control::Control::new(dir.path().join("nope.sock"), "k".into());
        let mut state = State::default();
        state
            .sessions
            .insert("a0e9d562-111e".into(), entry(Stage::ClearSent, "a0e9d562"));
        let t = dir.path().join("t.jsonl");
        let action = recover(
            &control,
            &[job("a0e9d562", "idle", Some("fresh-sid"))],
            &in_flight("a0e9d562-111e", "a0e9d562", &t, dir.path()),
            &entry(Stage::ClearSent, "a0e9d562"),
            SystemTime::now(),
            &InjectOpts {
                dry_run: false,
                only: None,
            },
            &mut state,
        )
        .unwrap();
        // `fail`, not a new verb: reseed-watch-status.sh only shows actions it knows.
        assert_eq!(action.did, "fail");
        assert!(action.why.contains("already consumed"));
        assert_eq!(state.sessions["a0e9d562-111e"].stage, Stage::Failed);
    }

    #[test]
    fn a_persisted_reload_earns_a_go_that_fits_one_box_row() {
        assert_eq!(go_text(&watch::Landing::Inline), "go");
        let named = go_text(&watch::Landing::Persisted(Some(
            "/p/tool-results/h.txt".into(),
        )));
        let bare = go_text(&watch::Landing::Persisted(None));
        assert_eq!(named, bare);
        assert!(bare.starts_with("go") && bare.ends_with("preview above"));
        // A 100-column terminal leaves 98 cells after the marker; a wrapped go is never returned.
        assert!(bare.len() < 98);
        assert!(
            !bare.contains("/clear"),
            "the concatenation tripwire keys on /clear"
        );
    }

    /// The daemon accepting `/clear` is not the same as the TUI running it,
    /// and both look identical in the emit log. If the job still runs the
    /// session we meant to clear, its context is intact: a 2.3KB paste there
    /// would land in a live near-limit thread.
    #[test]
    fn a_clear_that_never_landed_is_not_recovered_over() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a0e9d562-111e"), "RELOAD BODY").unwrap();
        let control = control::Control::new(dir.path().join("nope.sock"), "k".into());
        let mut state = State::default();
        state
            .sessions
            .insert("a0e9d562-111e".into(), entry(Stage::ClearSent, "a0e9d562"));
        let t = dir.path().join("t.jsonl");
        let action = recover(
            &control,
            // The job still runs the very session the clear was typed into.
            &[job("a0e9d562", "idle", Some("a0e9d562-111e"))],
            &in_flight("a0e9d562-111e", "a0e9d562", &t, dir.path()),
            &entry(Stage::ClearSent, "a0e9d562"),
            SystemTime::now(),
            &InjectOpts {
                dry_run: false,
                only: None,
            },
            &mut state,
        )
        .unwrap();
        assert_eq!(action.did, "wait");
        assert!(action.why.contains("has not landed"));
        assert!(dir.path().join("a0e9d562-111e").exists(), "arm untouched");
    }

    /// A busy worker must postpone the recovery, never abandon it: the entry
    /// stays ClearSent so the next pass comes straight back here.
    #[test]
    fn a_busy_worker_postpones_the_recovery_rather_than_dropping_it() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a0e9d562-111e"), "RELOAD BODY").unwrap();
        let control = control::Control::new(dir.path().join("nope.sock"), "k".into());
        let mut state = State::default();
        state
            .sessions
            .insert("a0e9d562-111e".into(), entry(Stage::ClearSent, "a0e9d562"));
        let t = dir.path().join("t.jsonl");
        let action = recover(
            &control,
            &[job("a0e9d562", "active", Some("fresh-sid"))],
            &in_flight("a0e9d562-111e", "a0e9d562", &t, dir.path()),
            &entry(Stage::ClearSent, "a0e9d562"),
            SystemTime::now(),
            &InjectOpts {
                dry_run: false,
                only: None,
            },
            &mut state,
        )
        .unwrap();
        assert_eq!(action.did, "wait");
        assert_eq!(
            state.sessions["a0e9d562-111e"].stage,
            Stage::ClearSent,
            "the entry must survive or the session is stranded for good"
        );
    }

    #[test]
    fn only_identity_tiers_prove_a_reload() {
        assert!(is_identity_tier("1"));
        assert!(is_identity_tier("1b"));
        assert!(is_identity_tier("2"));
        assert!(is_identity_tier("2-jobmatch"));
        assert!(!is_identity_tier("3"));
        assert!(!is_identity_tier("3-declined-cwdonly-fresh"));
        assert!(is_identity_tier("1-stale"));
        assert!(is_identity_tier("2-stale"));
        assert!(is_identity_tier("2-jobmatch-stale"));
        // The injector's own hand-delivery must never prove a later arm.
        assert!(!is_identity_tier("recover"));
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
