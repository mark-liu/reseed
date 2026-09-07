//! The `SessionStart(clear)` hook: pick an armed sentinel and emit its
//! reload text on stdout. Ported from `reseed-clear-hook.sh` and the tier
//! semantics in `reseed-clear-hook.md`. Fails open: never exits non-zero.

use crate::{emit, identity, msg, park, paths, sentinel, spawn};
use anyhow::Result;
use regex::Regex;
use serde::Deserialize;
use std::io::{IsTerminal, Read};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::SystemTime;

const CLEAR_MARKER: &str = "<command-name>/clear</command-name>";
const MAX_GEN_HOPS: u32 = 15;
/// Claude Code drops hook stdout from context and writes it to a file once it
/// passes an inline limit measured on 2026-09-06 as a sharp boundary between
/// 9,831 B delivered and 11,276 B persisted (2,081 records, no sample between).
/// A persisted reload reaches the model as a path, so 8 KB keeps real margin.
pub const STDOUT_CAP: usize = 8 * 1024;

/// Fallbacks for the operator-facing texts. A site overrides any of these by
/// name under `$RESEED_MESSAGES`, so its own doctrine never compiles into
/// this crate. See `msg`.
const PROVENANCE_DEFAULT: &str = "PROVENANCE (checked mechanically): this bundle is generation \
     {gen} of a reseed chain - its own first user turn was `/clear` + \"go\", so its subject was \
     INHERITED down the chain, not chosen for this thread. It is a CANDIDATE, not a brief. Local \
     work (reading, analysis, drafting to ~/scratch/) proceeds; name the inherited subject back \
     to {op} in one line and get a yes BEFORE any outward-facing write (Notion/Slack/GitLab/\
     GitHub create-or-post) - a resumed authorisation is not a live authorisation.";

const PARK_HEAD_DEFAULT: &str = "Park-ledger CANDIDATE lines, keyword-matched at /clear against \
     this bundle's narrative (the ledger file stays closed - grep it directly only for a resource \
     you are about to re-derive, never tail/sed/cat it). Verify EACH against the subject you read \
     in narrative.md: a non-matching line is ANOTHER THREAD'S WORK and its resume: pointer is an \
     address, not an assignment:";

const STALE_HEAD_DEFAULT: &str = "A session reload was armed {age}m ago for the session you just \
     cleared, and re-distilled just now.";

const STALE_RESUME_DEFAULT: &str = "If the first message resumes that work, or is just \"go\" / \
     \"continue\" / \"carry on\", do this: {reload}";

const STALE_UNRELATED_DEFAULT: &str = "If it clearly opens an UNRELATED task, do NOT read the \
     bundle: answer what was asked and note the reload path above in one line.";

const CWD_ONLY_HEAD_DEFAULT: &str = "A reseed reload is armed under this directory but was NOT \
     auto-loaded. It is a {state} arm from job {job}; this session is job {thisjob}. A cwd match \
     alone is not identity - every background job runs in this same directory, so the arm can \
     belong to a different thread.";

const CWD_ONLY_GEN_DEFAULT: &str = "That bundle is also generation {gen} of a reseed chain, so \
     its subject was inherited rather than chosen.";

const CWD_ONLY_DECIDE_DEFAULT: &str = "Nothing is lost - the arm stays on disk. If the first \
     message resumes earlier work, or is just \"go\" / \"continue\", check the subject FIRST and \
     only then decide:";

const CWD_ONLY_UNRELATED_DEFAULT: &str = "If the first message opens an UNRELATED task, ignore \
     the arm and answer what was asked.";

const AMBIGUOUS_HEAD_DEFAULT: &str = "Reseed reloads are armed for {n} sessions under this \
     directory, so none was auto-loaded (picking one could cross-load an unrelated session).";

const AMBIGUOUS_PICK_DEFAULT: &str = "If the first message resumes earlier work, or is just \
     \"go\" / \"continue\", pick by CONTENT, not by mtime: open each and keep the one whose last \
     turns match the work being resumed. Mtime is a coin flip here - the newest arm is as likely \
     to be a concurrent job's unrelated thread, and following it publishes one session's work \
     under another's subject. Candidates:";

const AMBIGUOUS_JOB_DEFAULT: &str = "This session is job {thisjob} - an arm from a DIFFERENT job \
     is almost certainly not yours.";

const AMBIGUOUS_UNRELATED_DEFAULT: &str = "If it opens an UNRELATED task, ignore them and answer \
     what was asked.";

#[derive(Debug, Default, Deserialize)]
pub struct Payload {
    pub session_id: Option<String>,
    pub cwd: Option<String>,
}

/// Read the `SessionStart` payload from stdin, only when stdin is not a
/// terminal (hang-safe, mirrors the shell's `[ -t 0 ] || cat`).
pub fn read_payload() -> Payload {
    if std::io::stdin().is_terminal() {
        return Payload::default();
    }
    let mut buf = String::new();
    if std::io::stdin().read_to_string(&mut buf).is_err() {
        return Payload::default();
    }
    serde_json::from_str(&buf).unwrap_or_default()
}

/// The session's request context: id, cwd, sanitised job id.
struct Env {
    sid: Option<String>,
    cwd: String,
    jobid: Option<String>,
}

fn build_env(payload: Payload) -> Env {
    let sid = payload.session_id.filter(|s| !s.is_empty());
    let cwd = payload
        .cwd
        .filter(|s| !s.is_empty())
        .or_else(|| std::env::var("PWD").ok())
        .or_else(|| {
            std::env::current_dir()
                .ok()
                .and_then(|p| p.to_str().map(String::from))
        })
        .unwrap_or_default();
    let jobid = std::env::var("CLAUDE_JOB_DIR")
        .ok()
        .filter(|s| !s.is_empty())
        .and_then(|d| {
            PathBuf::from(d)
                .file_name()
                .and_then(|n| n.to_str())
                .map(sentinel::key)
        });
    Env { sid, cwd, jobid }
}

/// Run the hook: find a bundle, print its reload, log the tier, clean up.
/// Every internal failure is swallowed; the caller always exits 0.
pub fn run(payload: Payload) -> Result<()> {
    let env = build_env(payload);
    let pending = paths::pending()?;
    if !pending.is_dir() {
        return Ok(());
    }
    let log_path = paths::emit_log()?;
    let now = SystemTime::now();

    // Tier 1: exact session-id key.
    if let Some(sid) = env.sid.as_deref() {
        let key = sentinel::key(sid);
        if let Some(a) = sentinel::read(&pending, &key) {
            return if sentinel::is_fresh(&a, now) {
                deliver(&env, &log_path, "1", &a)
            } else {
                deliver_stale(&env, &log_path, "1-stale", &a)
            };
        }
    }

    // Tier 1b (P7): fresh sentinel whose .pid sidecar is this session's pid.
    if let Some(sid) = env.sid.as_deref() {
        if let Some(pid) = identity::session_pid(sid) {
            let mut fresh: Option<sentinel::Arm> = None;
            let mut stale: Option<sentinel::Arm> = None;
            for a in sentinel::list(&pending) {
                if a.pid != Some(pid) {
                    continue;
                }
                if sentinel::is_fresh(&a, now) {
                    fresh = Some(a);
                } else if stale.is_none() {
                    stale = Some(a);
                }
            }
            if let Some(a) = fresh {
                return deliver(&env, &log_path, "1b", &a);
            }
            if let Some(a) = stale {
                return deliver_stale(&env, &log_path, "1b-stale", &a);
            }
        }
    }

    // Tier 2: job lineage (.job sidecar, or legacy key-prefix, names this job).
    if let Some(jobid) = env.jobid.as_deref() {
        let mut fresh: Option<sentinel::Arm> = None;
        let mut fresh_n = 0u32;
        let mut stale: Option<sentinel::Arm> = None;
        let mut stale_n = 0u32;
        let mut stale_group: Vec<sentinel::Arm> = Vec::new();
        for a in sentinel::list(&pending) {
            if !belongs_to_job(&a, jobid) {
                continue;
            }
            if sentinel::is_fresh(&a, now) {
                fresh_n += 1;
                fresh = Some(a);
            } else {
                stale_n += 1;
                stale_group.push(a.clone());
                stale = stale_group.last().cloned();
            }
        }
        if fresh_n == 1 {
            return deliver(&env, &log_path, "2", &fresh.unwrap());
        }
        if fresh_n == 0 && stale_n == 1 {
            return deliver_stale(&env, &log_path, "2-stale", &stale.unwrap());
        }
        // Two or more stale arms in one job lineage: GC, never a guess.
        if stale_n > 1 {
            for a in stale_group {
                sentinel::remove(&a);
            }
        }
    }

    // Tier 3: project-scoped cwd fallback, plus the orphan GC that rides
    // along with it in the original hook.
    if env.cwd.is_empty() {
        return Ok(());
    }
    let mut fresh_cands: Vec<sentinel::Arm> = Vec::new();
    let mut stale_cands: Vec<sentinel::Arm> = Vec::new();
    for a in sentinel::list(&pending) {
        match &a.cwd {
            None => {
                // Sidecar-less orphan: only ever cleanable, never a candidate.
                if !sentinel::is_fresh(&a, now) {
                    sentinel::remove(&a);
                }
            }
            Some(scwd) => {
                let under_root = scwd == &env.cwd || scwd.starts_with(&format!("{}/", env.cwd));
                if !under_root {
                    if !sentinel::is_fresh(&a, now) {
                        sentinel::remove(&a);
                    }
                    continue;
                }
                if sentinel::is_fresh(&a, now) {
                    fresh_cands.push(a);
                } else {
                    stale_cands.push(a);
                }
            }
        }
    }
    fresh_cands.sort_by(|a, b| a.path.cmp(&b.path));

    if fresh_cands.len() == 1 {
        let cand = &fresh_cands[0];
        if let Some(jobid) = env.jobid.as_deref() {
            if cand.job.as_deref() == Some(jobid) {
                return deliver(&env, &log_path, "2-jobmatch", cand);
            }
        }
        return report_cwd_only(&env, &log_path, cand, "fresh");
    }

    if fresh_cands.len() > 1 {
        if let Some(jobid) = env.jobid.as_deref() {
            let mine: Vec<&sentinel::Arm> = fresh_cands
                .iter()
                .filter(|a| a.job.as_deref() == Some(jobid))
                .collect();
            if mine.len() == 1 {
                let cand = mine[0].clone();
                return deliver(&env, &log_path, "3-jobtiebreak", &cand);
            }
        }
    }

    if fresh_cands.is_empty() && stale_cands.len() == 1 {
        let cand = &stale_cands[0];
        if let Some(jobid) = env.jobid.as_deref() {
            if cand.job.as_deref() == Some(jobid) {
                return deliver_stale(&env, &log_path, "2-jobmatch-stale", cand);
            }
        }
        return report_cwd_only(&env, &log_path, cand, "stale");
    }

    if fresh_cands.is_empty() && stale_cands.len() > 1 {
        for a in &stale_cands {
            sentinel::remove(a);
        }
        return Ok(());
    }

    if fresh_cands.len() > 1 {
        report_ambiguous(&env, &log_path, &fresh_cands);
    }

    Ok(())
}

fn belongs_to_job(a: &sentinel::Arm, jobid: &str) -> bool {
    match a.job.as_deref() {
        Some(j) if !j.is_empty() => j == jobid,
        _ => a
            .path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|name| name.starts_with(jobid)),
    }
}

fn bundle_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"/[^ ]*/\.claude/reseed/[0-9a-f][0-9a-f-]{7,}").unwrap())
}

/// The bundle directory named inside a sentinel's reload text.
fn bundle_of(reload: &str) -> Option<PathBuf> {
    bundle_re().find(reload).map(|m| PathBuf::from(m.as_str()))
}

/// Generation depth of a bundle: how many hops of `/clear` + "go" chain
/// links it took to get here, capped at `MAX_GEN_HOPS`.
fn gen_of(start: &Path) -> u32 {
    let mut d = start.to_path_buf();
    let mut n = 0u32;
    for _ in 0..MAX_GEN_HOPS {
        if !d.is_dir() {
            break;
        }
        let Ok(text) = std::fs::read_to_string(d.join("narrative.md")) else {
            break;
        };
        let head: String = text.lines().take(40).collect::<Vec<_>>().join("\n");
        if !head.contains(CLEAR_MARKER) {
            break;
        }
        n += 1;
        let ctx_text = std::fs::read_to_string(d.join("context-files.md")).unwrap_or_default();
        let Some(nxt) = bundle_of(&ctx_text) else {
            break;
        };
        if nxt == d {
            break;
        }
        d = nxt;
    }
    n
}

/// Provenance banner text, or `None` when the bundle owns its own subject
/// (generation 0). Returns the generation either way.
fn prov_banner(reload: &str) -> (u32, Option<String>) {
    let Some(bundle) = bundle_of(reload) else {
        return (0, None);
    };
    let gen = gen_of(&bundle);
    if gen == 0 {
        return (0, None);
    }
    let template = msg::text("provenance", PROVENANCE_DEFAULT);
    let text = msg::fill(
        &template,
        &[("gen", &gen.to_string()), ("op", &crate::msg::operator())],
    );
    (gen, Some(format!("{text}\n\n")))
}

fn log_row(env: &Env, log_path: &Path, tier: &str, arm_path: Option<&Path>, gen: &str) {
    let arm_name = arm_path
        .and_then(|p| p.file_name())
        .and_then(|n| n.to_str())
        .unwrap_or("none");
    let sid = env.sid.as_deref().unwrap_or("");
    let job = env.jobid.as_deref().unwrap_or("none");
    let _ = emit::log(log_path, tier, sid, job, &env.cwd, arm_name, gen);
}

/// The candidate block, rendered to fit `budget` bytes. This block is the
/// only unbounded part of the emission (eight lines at full width is 24 KB,
/// three times the inline limit), so it is the part that gets cut.
fn park_block(reload: &str, budget: usize) -> Option<String> {
    let bundle = bundle_of(reload)?;
    let ledger_path = paths::ledger().ok()?;
    let ledger_text = std::fs::read_to_string(&ledger_path).ok()?;
    if ledger_text.is_empty() {
        return None;
    }
    let narrative = std::fs::read_to_string(bundle.join("narrative.md")).ok()?;
    let lines: Vec<String> = ledger_text.lines().map(String::from).collect();
    let matches = park::matches(&narrative, &lines);
    let head = format!("\n{}\n", msg::text("park-head", PARK_HEAD_DEFAULT));
    let fitted = park::render_within(
        &matches,
        budget.saturating_sub(head.len() + ELISION_RESERVE),
    );
    let body = if fitted.text.is_empty() {
        "none"
    } else {
        &fitted.text
    };
    let mut block = format!("{head}{body}\n");
    if fitted.shortened || !fitted.dropped.is_empty() {
        block.push_str(&elision_note(&fitted, &ledger_path));
    }
    Some(block)
}

/// Bytes held back from the candidate budget for `elision_note`.
const ELISION_RESERVE: usize = 400;

/// Say what was cut, so a line whose subject match fell past the cut is not
/// read as absent. The read verb is a grep: a positional read of the ledger
/// is what the scope guard denies.
fn elision_note(fitted: &park::Fitted, ledger_path: &Path) -> String {
    let dropped = if fitted.dropped.is_empty() {
        String::new()
    } else {
        let nums: Vec<String> = fitted.dropped.iter().map(|n| n.to_string()).collect();
        format!(
            " Lines {} matched but were left out entirely.",
            nums.join(", ")
        )
    };
    format!(
        "(Shortened to keep this reload inline: a line may be cut before the words that name \
         your subject.{dropped} Read any of them in full with `grep -n -iE '<your subject \
         keywords>' {}`.)\n",
        ledger_path.display(),
    )
}

fn status_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r#""status":\s*"(pending|in_progress)""#).unwrap())
}

/// Copy `~/.claude/tasks/<old>/*.json` into `<new>/`, never clobbering a
/// list the new session already started. Identity tiers only.
fn carry_tasks(old_key: &str, env: &Env) -> Option<String> {
    let new = env.sid.as_deref()?;
    if old_key == new {
        return None;
    }
    let tasks_dir = paths::tasks().ok()?;
    let old_dir = tasks_dir.join(old_key);
    if !old_dir.is_dir() {
        return None;
    }
    let files: Vec<PathBuf> = std::fs::read_dir(&old_dir)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("json"))
        .collect();
    if files.is_empty() {
        return None;
    }
    let new_dir = tasks_dir.join(new);
    std::fs::create_dir_all(&new_dir).ok()?;
    let mut open = 0usize;
    for f in &files {
        if let Ok(body) = std::fs::read_to_string(f) {
            if status_re().is_match(&body) {
                open += 1;
            }
        }
        if let Some(name) = f.file_name() {
            let dest = new_dir.join(name);
            if !dest.exists() {
                let _ = std::fs::copy(f, &dest);
            }
        }
    }
    let old8: String = old_key.chars().take(8).collect();
    Some(format!(
        "\nTask list carried over from session {old8}: {} tasks, {open} open. TaskList already \
         shows them - do not recreate, just continue from the open ones.\n",
        files.len(),
    ))
}

fn key_of(a: &sentinel::Arm) -> String {
    a.path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default()
        .to_string()
}

/// Assemble one emission inside `STDOUT_CAP`. `fixed` is everything that has
/// to survive verbatim; the candidate block takes whatever budget is left,
/// after reserving the task carry-over. Emitting once, rather than a print
/// per section, is what makes the cap assertable.
fn emit_capped(fixed: String, reload_text: &str, key: &str, env: &Env) -> String {
    let tasks = carry_tasks(key, env);
    let reserve = tasks.as_deref().map_or(0, str::len);
    let mut out = if fixed.len() <= STDOUT_CAP {
        fixed
    } else {
        oversize_pointer(reload_text, fixed.len())
    };
    let budget = STDOUT_CAP.saturating_sub(out.len() + reserve);
    if let Some(block) = park_block(reload_text, budget) {
        out.push_str(&truncate_bytes(&block, budget));
    }
    if let Some(t) = tasks {
        // The note gives way, not the brief: it is advisory, the files were
        // already copied, and a pointer would cost every section instead.
        out.push_str(&truncate_bytes(&t, STDOUT_CAP.saturating_sub(out.len())));
    }
    debug_assert!(out.len() <= STDOUT_CAP, "emission over cap: {}", out.len());
    out
}

/// Fallback for a reload text that alone passes the limit: name the bundle
/// rather than inline it. Over the limit the harness hands the model a file
/// path anyway, and takes the candidate block and task list down with it.
fn oversize_pointer(reload_text: &str, bytes: usize) -> String {
    match bundle_of(reload_text) {
        Some(b) => format!(
            "A session reload was armed for the session you just cleared. Its text is {bytes} \
             bytes, past the {STDOUT_CAP} byte inline limit, so it is named here instead of \
             inlined: read {0}/narrative.md in full, then {0}/context-files.md, and continue \
             there.\n",
            b.display(),
        ),
        None => truncate_bytes(reload_text, STDOUT_CAP),
    }
}

/// Cut to a byte budget on a character boundary. `String::truncate` panics
/// mid-character and `chars().take(n)` counts characters, not bytes.
fn truncate_bytes(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut out = String::with_capacity(max);
    for c in s.chars() {
        if out.len() + c.len_utf8() > max {
            break;
        }
        out.push(c);
    }
    out
}

/// Deliver a fresh identity match: banner, log row, reload text, park
/// block, task carry-over, cleanup, then the `-done` row (P8).
fn deliver(env: &Env, log_path: &Path, tier: &str, a: &sentinel::Arm) -> Result<()> {
    let (gen, banner) = prov_banner(&a.reload);
    let mut fixed = String::new();
    if let Some(b) = &banner {
        fixed.push_str(b);
    }
    log_row(env, log_path, tier, Some(&a.path), &gen.to_string());
    fixed.push_str(&a.reload);
    print!("{}", emit_capped(fixed, &a.reload, &key_of(a), env));
    sentinel::remove(a);
    log_row(
        env,
        log_path,
        &format!("{tier}-done"),
        Some(&a.path),
        &gen.to_string(),
    );
    Ok(())
}

/// Deliver a stale identity match behind the opt-out wrapper: re-arm in the
/// background, then the same trailer as `deliver`, then the `-done` row.
fn deliver_stale(env: &Env, log_path: &Path, tier: &str, a: &sentinel::Arm) -> Result<()> {
    let (gen, banner) = prov_banner(&a.reload);
    let mut fixed = String::new();
    if let Some(b) = &banner {
        fixed.push_str(b);
    }
    let age_mins = SystemTime::now()
        .duration_since(a.mtime)
        .map(|d| d.as_secs() / 60)
        .unwrap_or(0);
    let key = key_of(a);
    spawn::rearm(&key);
    let head = msg::text("stale-head", STALE_HEAD_DEFAULT);
    fixed.push_str(&msg::fill(&head, &[("age", &age_mins.to_string())]));
    fixed.push('\n');
    let resume = msg::text("stale-resume", STALE_RESUME_DEFAULT);
    fixed.push_str(&msg::fill(&resume, &[("reload", &a.reload)]));
    fixed.push('\n');
    fixed.push_str(&msg::text("stale-unrelated", STALE_UNRELATED_DEFAULT));
    fixed.push('\n');
    print!("{}", emit_capped(fixed, &a.reload, &key, env));
    log_row(env, log_path, tier, Some(&a.path), &gen.to_string());
    sentinel::remove(a);
    log_row(
        env,
        log_path,
        &format!("{tier}-done"),
        Some(&a.path),
        &gen.to_string(),
    );
    Ok(())
}

/// A cwd-only match: named on stdout, left on disk, never loaded.
fn report_cwd_only(env: &Env, log_path: &Path, a: &sentinel::Arm, state: &str) -> Result<()> {
    let sj = a.job.clone().unwrap_or_else(|| "unknown".to_string());
    let bundle = bundle_of(&a.reload);
    let gen = bundle.as_deref().map(gen_of).unwrap_or(0);
    log_row(
        env,
        log_path,
        &format!("3-declined-cwdonly-{state}"),
        Some(&a.path),
        &gen.to_string(),
    );
    let job_display = env
        .jobid
        .as_deref()
        .unwrap_or("none (foreground TUI)")
        .to_string();
    println!(
        "{}",
        msg::fill(
            &msg::text("cwd-only-head", CWD_ONLY_HEAD_DEFAULT),
            &[("state", state), ("job", &sj), ("thisjob", &job_display)],
        )
    );
    if gen > 0 {
        println!(
            "{}",
            msg::fill(
                &msg::text("cwd-only-gen", CWD_ONLY_GEN_DEFAULT),
                &[("gen", &gen.to_string())],
            )
        );
    }
    println!("{}", msg::text("cwd-only-decide", CWD_ONLY_DECIDE_DEFAULT));
    if let Some(b) = &bundle {
        println!("  sed -n \"1,20p\" {}/narrative.md", b.display());
    }
    println!(
        "{}",
        msg::text("cwd-only-unrelated", CWD_ONLY_UNRELATED_DEFAULT)
    );
    Ok(())
}

/// Two or more same-root fresh candidates: named, never auto-loaded.
fn report_ambiguous(env: &Env, log_path: &Path, cands: &[sentinel::Arm]) {
    println!(
        "{}",
        msg::fill(
            &msg::text("ambiguous-head", AMBIGUOUS_HEAD_DEFAULT),
            &[("n", &cands.len().to_string())],
        )
    );
    println!("{}", msg::text("ambiguous-pick", AMBIGUOUS_PICK_DEFAULT));
    for a in cands {
        let sj = a.job.clone().unwrap_or_else(|| "unknown".to_string());
        println!("  {} (job {sj}, armed {})", a.path.display(), armed_at(a));
    }
    let job_display = env
        .jobid
        .as_deref()
        .unwrap_or("none (foreground TUI)")
        .to_string();
    println!(
        "{}",
        msg::fill(
            &msg::text("ambiguous-job", AMBIGUOUS_JOB_DEFAULT),
            &[("thisjob", &job_display)],
        )
    );
    log_row(
        env,
        log_path,
        &format!("3-declined-{}cand", cands.len()),
        None,
        "-",
    );
    println!(
        "{}",
        msg::text("ambiguous-unrelated", AMBIGUOUS_UNRELATED_DEFAULT)
    );
}

/// HH:MM:SS the arm was written, in the operator's own timezone as the shell's
/// `stat -f %Sm` printed it. The value is read to tell two candidate arms
/// apart, so a UTC clock beside a local wall clock is a wrong answer.
fn armed_at(a: &sentinel::Arm) -> String {
    let secs = a
        .mtime
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0) as libc::time_t;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    // SAFETY: the reentrant form writes only into `tm` and reads only `secs`,
    // both owned here, so no static buffer is shared with another thread.
    if unsafe { libc::localtime_r(&secs, &mut tm) }.is_null() {
        let rem = (secs as u64) % 86_400;
        return format!("{:02}:{:02}:{:02}", rem / 3600, (rem % 3600) / 60, rem % 60);
    }
    format!("{:02}:{:02}:{:02}", tm.tm_hour, tm.tm_min, tm.tm_sec)
}
