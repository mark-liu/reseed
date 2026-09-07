//! The injection half of `reseed watch`: types `/clear`, waits for the
//! reload to be proven delivered, then types `go`. Background jobs only,
//! because only they have a daemon-owned pty to type into; a terminal
//! session is left to Mark's hands (spec D2).
//!
//! Nothing here runs without `--inject`, and every pass re-reads the kill
//! file, so stopping it is one `touch ~/.claude/reseed/watch.off`.

use crate::{control, emit, paths, sentinel, usage, watch};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

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
    if opts.dry_run {
        return Ok(Action::new(&cand.sid, &cand.job, "would-clear", "dry run"));
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

/// Background sessions past the line that have a bundle armed for them.
fn candidates(now: SystemTime) -> Result<Vec<Candidate>> {
    let pending = paths::pending()?;
    let projects = paths::projects()?;
    let mut out = Vec::new();
    for (sid, job) in watch::live_bg_sessions()? {
        let Some(transcript) = watch::transcript_for(&projects, &sid) else {
            continue;
        };
        let (tier, _ctx, _line, _early) = usage::context_state(&transcript);
        if tier == 0 {
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

#[cfg(test)]
mod tests {
    use super::*;

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
