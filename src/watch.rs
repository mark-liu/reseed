//! `reseed watch`: the reload detector. Lists live sessions past the
//! context line and audits whether recent reload emissions actually
//! reached a context. Report only by default; `--inject` hands the
//! background ones to `inject`, which types `/clear` and `go` into them.

use crate::{emit, inject, paths, sentinel, usage};
use anyhow::{Context, Result};
use serde::Serialize;
use std::fs::OpenOptions;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

pub struct WatchOpts {
    /// Accepted for the future polling mode; this phase always runs once.
    pub once: bool,
    pub since_days: Option<u64>,
    pub json: bool,
    pub log_path: Option<PathBuf>,
    /// `Some` only when `--inject` was passed: the detector never types.
    pub inject: Option<inject::InjectOpts>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SessionReport {
    pub sid8: String,
    pub kind: String,
    pub ctx_k: Option<u64>,
    pub tier: u8,
    pub armed: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Verdict {
    Delivered,
    Cancelled,
    Persisted,
    Missing,
    /// The receiving session's transcript is gone, so delivery is unknowable.
    /// Kept apart from `Missing`: counting it as a failure would inflate the
    /// loss rate this detector exists to measure.
    Unknown,
}

impl Verdict {
    fn as_str(self) -> &'static str {
        match self {
            Verdict::Delivered => "delivered",
            Verdict::Cancelled => "cancelled",
            Verdict::Persisted => "persisted",
            Verdict::Missing => "missing",
            Verdict::Unknown => "unknown",
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct AuditRow {
    pub ts: String,
    pub tier: String,
    pub sid8: String,
    pub verdict: Verdict,
}

#[derive(Debug, Default, Serialize)]
pub struct AuditCounts {
    pub delivered: usize,
    pub cancelled: usize,
    pub persisted: usize,
    pub missing: usize,
    pub unknown: usize,
}

impl AuditCounts {
    fn add(&mut self, v: Verdict) {
        match v {
            Verdict::Delivered => self.delivered += 1,
            Verdict::Cancelled => self.cancelled += 1,
            Verdict::Persisted => self.persisted += 1,
            Verdict::Missing => self.missing += 1,
            Verdict::Unknown => self.unknown += 1,
        }
    }

    fn total(&self) -> usize {
        self.delivered + self.cancelled + self.persisted + self.missing + self.unknown
    }
}

#[derive(Debug, Serialize)]
pub struct Report {
    pub sessions: Vec<SessionReport>,
    pub audit: Vec<AuditRow>,
    pub counts: AuditCounts,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub actions: Vec<inject::Action>,
}

pub fn run(opts: WatchOpts) -> Result<()> {
    let mut report = build_report(opts.since_days)?;
    if let Some(inject_opts) = &opts.inject {
        report.actions = inject::run(inject_opts)?;
    }
    let log_path = match opts.log_path {
        Some(p) => p,
        None => paths::watch_log()?,
    };
    append_log(&log_path, &report)?;

    if opts.json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print_text(&report);
    }
    if !opts.once {
        eprintln!("watch: no polling loop in this build, ran a single pass anyway");
    }
    Ok(())
}

fn print_text(report: &Report) {
    println!("Live sessions past the context line:");
    if report.sessions.is_empty() {
        println!("  (none)");
    }
    for s in &report.sessions {
        println!(
            "  {} {:<8} ctx={:>4}k tier={} armed={}",
            s.sid8,
            s.kind,
            s.ctx_k.map(|k| k.to_string()).unwrap_or_else(|| "?".into()),
            s.tier,
            s.armed,
        );
    }
    println!();
    println!("Reload delivery audit:");
    for a in &report.audit {
        println!(
            "  {} sid={} tier={} -> {}",
            a.ts,
            a.sid8,
            a.tier,
            a.verdict.as_str()
        );
    }
    if !report.actions.is_empty() {
        println!();
        println!("Injection:");
        for a in &report.actions {
            println!("  {} job={} {} ({})", a.sid8, a.job, a.did, a.why);
        }
    }
    println!(
        "counts: delivered={} cancelled={} persisted={} missing={} unknown={} total={}",
        report.counts.delivered,
        report.counts.cancelled,
        report.counts.persisted,
        report.counts.missing,
        report.counts.unknown,
        report.counts.total(),
    );
}

fn append_log(log_path: &Path, report: &Report) -> Result<()> {
    if let Some(parent) = log_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut f = OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path)
        .with_context(|| format!("opening {}", log_path.display()))?;
    let ts = now_ts();
    for s in &report.sessions {
        let ctx_k = s.ctx_k.map(|k| k.to_string()).unwrap_or_default();
        writeln!(
            f,
            "{ts}\t{}\t{ctx_k}\t{}\tnone\treport\ttier{}\t{}",
            s.sid8,
            s.kind,
            s.tier,
            if s.armed { "armed" } else { "unarmed" },
        )?;
    }
    for a in &report.audit {
        writeln!(
            f,
            "{ts}\t{}\t\taudit\tnone\treport\t{}\ttier={}",
            a.sid8,
            a.verdict.as_str(),
            a.tier,
        )?;
    }
    for a in &report.actions {
        writeln!(
            f,
            "{ts}\t{}\t\tinject\tjob={}\t{}\t{}",
            a.sid8, a.job, a.did, a.why,
        )?;
    }
    Ok(())
}

/// `%FT%TZ` now, matching `emit.log`'s own timestamp shape.
fn now_ts() -> String {
    // Reuses emit's row writer purely for its timestamp: log a throwaway
    // row to a scratch buffer is overkill, so format directly instead.
    let secs = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    emit_ts_from_secs(secs)
}

fn emit_ts_from_secs(secs: u64) -> String {
    let days = secs / 86_400;
    let rem = secs % 86_400;
    let (h, m, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    let (y, mo, d) = civil_from_days(days as i64);
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{m:02}:{s:02}Z")
}

/// Howard Hinnant's `civil_from_days`, duplicated from `emit` (private
/// there): days since epoch to (year, month, day).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

fn build_report(since_days: Option<u64>) -> Result<Report> {
    let sessions = live_sessions_past_line()?;
    let audit = audit_emit_log(since_days)?;
    let mut counts = AuditCounts::default();
    for a in &audit {
        counts.add(a.verdict);
    }
    Ok(Report {
        sessions,
        audit,
        counts,
        actions: Vec::new(),
    })
}

/// Registry fields this phase reads: enough to name a session and tell
/// bg from terminal. Parsed loosely; a malformed `<pid>.json` is skipped.
#[derive(serde::Deserialize)]
struct RegistryEntry {
    #[serde(rename = "sessionId")]
    session_id: Option<String>,
    #[serde(rename = "jobId")]
    job_id: Option<String>,
}

fn live_sessions_past_line() -> Result<Vec<SessionReport>> {
    let sessions_dir = paths::sessions_dir()?;
    let projects_dir = paths::projects()?;
    let pending = paths::pending()?;
    let mut out = Vec::new();

    let Ok(entries) = std::fs::read_dir(&sessions_dir) else {
        return Ok(out);
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let Ok(body) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(reg) = serde_json::from_str::<RegistryEntry>(&body) else {
            continue;
        };
        let Some(sid) = reg.session_id.filter(|s| !s.is_empty()) else {
            continue;
        };
        let kind = if reg.job_id.filter(|j| !j.is_empty()).is_some() {
            "bg"
        } else {
            "terminal"
        };
        let transcript = find_transcript(&projects_dir, &sid);
        let (tier, ctx, _line, _early) = match &transcript {
            Some(p) => usage::context_state(p),
            None => (0, None, 0, 0),
        };
        if tier == 0 {
            continue; // not past the line: out of scope for this report
        }
        let armed = sentinel::read(&pending, &sentinel::key(&sid)).is_some();
        out.push(SessionReport {
            sid8: sid.chars().take(8).collect(),
            kind: kind.to_string(),
            ctx_k: ctx.map(|c| c / 1000),
            tier,
            armed,
        });
    }
    out.sort_by(|a, b| a.sid8.cmp(&b.sid8));
    Ok(out)
}

/// Live background sessions as `(session id, job short id)`. The job id is
/// the handle the daemon control socket addresses, so a terminal session
/// (no job id) is not injectable and is left out.
pub fn live_bg_sessions() -> Result<Vec<(String, String)>> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(paths::sessions_dir()?) else {
        return Ok(out);
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let Ok(body) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(reg) = serde_json::from_str::<RegistryEntry>(&body) else {
            continue;
        };
        let (Some(sid), Some(job)) = (
            reg.session_id.filter(|s| !s.is_empty()),
            reg.job_id.filter(|j| !j.is_empty()),
        ) else {
            continue;
        };
        out.push((sid, job));
    }
    out.sort();
    Ok(out)
}

pub fn transcript_for(projects_dir: &Path, sid: &str) -> Option<PathBuf> {
    find_transcript(projects_dir, sid)
}

/// Did the reload this emit row describes actually reach a context? The
/// injector gates `go` on this, the audit reports it.
pub fn delivered(projects_dir: &Path, sid8: &str, row_secs: u64) -> bool {
    classify(projects_dir, sid8, row_secs) == Verdict::Delivered
}

/// A transcript whose filename stem is exactly `sid` (session ids are full
/// UUIDs; the registry always knows the whole id, never a prefix).
fn find_transcript(projects_dir: &Path, sid: &str) -> Option<PathBuf> {
    for project in read_dir_paths(projects_dir) {
        for p in read_dir_paths(&project) {
            if p.extension().and_then(|e| e.to_str()) == Some("jsonl")
                && p.file_stem().and_then(|s| s.to_str()) == Some(sid)
            {
                return Some(p);
            }
        }
    }
    None
}

/// A transcript matched by an 8-char `sid8` prefix, as `emit.log` truncates
/// it. Ambiguous prefixes are vanishingly unlikely at 8 hex chars; the
/// first match is used, same tradeoff `main.rs` makes for `fetch`.
fn find_transcript_by_prefix(projects_dir: &Path, sid8: &str) -> Option<PathBuf> {
    for project in read_dir_paths(projects_dir) {
        for p in read_dir_paths(&project) {
            let is_jsonl = p.extension().and_then(|e| e.to_str()) == Some("jsonl");
            let matches = p
                .file_stem()
                .and_then(|s| s.to_str())
                .is_some_and(|stem| stem.starts_with(sid8));
            if is_jsonl && matches {
                return Some(p);
            }
        }
    }
    None
}

fn read_dir_paths(dir: &Path) -> Vec<PathBuf> {
    std::fs::read_dir(dir)
        .map(|entries| entries.flatten().map(|e| e.path()).collect())
        .unwrap_or_default()
}

fn audit_emit_log(since_days: Option<u64>) -> Result<Vec<AuditRow>> {
    let log_path = paths::emit_log()?;
    let projects_dir = paths::projects()?;
    let rows = emit::read_rows(&log_path)?;
    let cutoff = since_days.map(|d| {
        SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs()
            .saturating_sub(d * 86_400)
    });

    let mut out = Vec::new();
    for row in rows {
        if !is_delivery_attempt(&row.tier) {
            continue;
        }
        let row_secs = emit::parse_ts_secs(&row.ts).unwrap_or(0);
        if let Some(cutoff) = cutoff {
            if row_secs < cutoff {
                continue;
            }
        }
        let verdict = classify(&projects_dir, &row.sid, row_secs);
        out.push(AuditRow {
            ts: row.ts,
            tier: row.tier,
            sid8: row.sid,
            verdict,
        });
    }
    Ok(out)
}

/// A `SessionStart` hook attachment, as it appears in a transcript line.
#[derive(serde::Deserialize)]
struct AttachmentLine {
    attachment: Option<Attachment>,
    timestamp: Option<String>,
}

#[derive(serde::Deserialize)]
struct Attachment {
    #[serde(rename = "type")]
    kind: Option<String>,
    #[serde(rename = "hookName")]
    hook_name: Option<String>,
    content: Option<String>,
    stdout: Option<String>,
}

/// Rows that are not a delivery attempt, and would skew the audit:
/// a `3-declined-*` row records the hook naming a candidate and refusing to
/// load it, and a `<tier>-done` row is the second row of one emission, not a
/// second emission.
fn is_delivery_attempt(tier: &str) -> bool {
    !tier.contains("declined") && !tier.ends_with("-done")
}

/// The matcher that emits a reload. A transcript usually also carries a
/// `SessionStart:startup` attachment (538 of them against 209 clear ones in
/// a 400-file sample), so matching the event alone reads the wrong hook.
const RELOAD_HOOK: &str = "SessionStart:clear";

/// Verdict order (spec 11c, phase C brief section 2): cancelled, then
/// persisted, then missing (transcript found, no clear attachment in it),
/// else delivered. No transcript at all is `Unknown`, never a failure.
///
/// Correlated on time, not on `sid8` alone: one session can carry several
/// clear attachments, and only one at or after `row_secs` can be the outcome
/// of this emission. An attachment the transcript failed to stamp is kept,
/// since dropping it would report a delivery as `Missing`.
fn classify(projects_dir: &Path, sid8: &str, row_secs: u64) -> Verdict {
    let Some(path) = find_transcript_by_prefix(projects_dir, sid8) else {
        return Verdict::Unknown;
    };
    let Ok(f) = std::fs::File::open(&path) else {
        return Verdict::Unknown;
    };
    for line in BufReader::new(f).lines().map_while(std::result::Result::ok) {
        if !line.contains("\"attachment\"") || !line.contains(RELOAD_HOOK) {
            continue;
        }
        let Ok(entry) = serde_json::from_str::<AttachmentLine>(&line) else {
            continue;
        };
        let Some(att) = entry.attachment else {
            continue;
        };
        if att.hook_name.as_deref() != Some(RELOAD_HOOK) {
            continue;
        }
        let att_secs = entry.timestamp.as_deref().and_then(emit::parse_ts_secs);
        if att_secs.is_some_and(|secs| secs < row_secs) {
            continue;
        }
        if att.kind.as_deref() == Some("hook_cancelled") {
            return Verdict::Cancelled;
        }
        let body = format!(
            "{}{}",
            att.content.as_deref().unwrap_or(""),
            att.stdout.as_deref().unwrap_or(""),
        );
        // An attachment that carried no text is not evidence of a delivery;
        // the emission's real outcome, if any, is a later attachment.
        if body.is_empty() {
            continue;
        }
        if body.contains("<persisted-output>") {
            return Verdict::Persisted;
        }
        return Verdict::Delivered;
    }
    Verdict::Missing
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tempfile::tempdir;

    fn with_home<T>(home: &Path, f: impl FnOnce() -> T) -> T {
        let _held = crate::testlock::env_lock();
        std::env::set_var("HOME", home);
        let out = f();
        std::env::remove_var("HOME");
        out
    }

    /// Attachments are stamped ahead of the emit rows the helpers plant, the
    /// only order the harness can produce: the hook logs, then its output is
    /// attached. The margin keeps a slow fixture from inverting it.
    fn transcript_line(sid: &str, attachment_json: &str) -> String {
        transcript_line_at(sid, 5, 0, attachment_json)
    }

    fn transcript_line_at(
        sid: &str,
        secs_from_now: i64,
        millis: u32,
        attachment_json: &str,
    ) -> String {
        let ts = attachment_ts(secs_from_now, millis);
        format!(
            r#"{{"parentUuid":null,"isSidechain":false,"attachment":{attachment_json},"type":"attachment","uuid":"u1","timestamp":"{ts}","sessionKind":"bg","sessionId":"{sid}"}}"#
        )
    }

    /// A transcript timestamp: `emit.log`'s shape plus the milliseconds the
    /// harness writes, so the fixture exercises the fractional-second path.
    fn attachment_ts(secs_from_now: i64, millis: u32) -> String {
        let secs = now_secs() + secs_from_now;
        let base = emit_ts_from_secs(secs.max(0) as u64);
        format!("{}.{millis:03}Z", base.trim_end_matches('Z'))
    }

    fn now_secs() -> i64 {
        SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64
    }

    fn plant_transcript(projects: &Path, project: &str, sid: &str, attachment_json: &str) {
        let dir = projects.join(project);
        std::fs::create_dir_all(&dir).unwrap();
        let line = transcript_line(sid, attachment_json);
        std::fs::write(dir.join(format!("{sid}.jsonl")), format!("{line}\n")).unwrap();
    }

    fn plant_emit_row(home: &Path, tier: &str, sid8: &str, ts_offset_days: i64) {
        let log = home.join(".claude/reseed/emit.log");
        std::fs::create_dir_all(log.parent().unwrap()).unwrap();
        let secs = now_secs() - ts_offset_days * 86_400;
        let ts = emit_ts_from_secs(secs.max(0) as u64);
        let row =
            format!("{ts}\ttier={tier}\tsid={sid8}\tjob=none\tcwd=/x\tarm=full-{sid8}\tgen=0\n");
        let mut f = OpenOptions::new()
            .create(true)
            .append(true)
            .open(log)
            .unwrap();
        f.write_all(row.as_bytes()).unwrap();
    }

    fn projects_dir(home: &Path) -> PathBuf {
        home.join(".claude/projects")
    }

    #[test]
    fn delivered_emission_classifies_as_delivered() {
        let tmp = tempdir().unwrap();
        let home = tmp.path();
        plant_transcript(
            &projects_dir(home),
            "-proj",
            "aaaaaaaa-0000-0000-0000-000000000000",
            r#"{"type":"hook_success","hookName":"SessionStart:clear","content":"Read the bundle"}"#,
        );
        plant_emit_row(home, "2", "aaaaaaaa", 0);
        let audit = audit_over(home);
        assert_eq!(audit[0].verdict, Verdict::Delivered);
    }

    #[test]
    fn cancelled_hook_classifies_as_cancelled() {
        let tmp = tempdir().unwrap();
        let home = tmp.path();
        plant_transcript(
            &projects_dir(home),
            "-proj",
            "bbbbbbbb-0000-0000-0000-000000000000",
            r#"{"type":"hook_cancelled","hookName":"SessionStart:clear"}"#,
        );
        plant_emit_row(home, "2", "bbbbbbbb", 0);
        let audit = audit_over(home);
        assert_eq!(audit[0].verdict, Verdict::Cancelled);
    }

    #[test]
    fn persisted_pointer_classifies_as_persisted() {
        let tmp = tempdir().unwrap();
        let home = tmp.path();
        let content = r#"<persisted-output>\nOutput too large (12.0KB). Full output saved to: /x/tool-results/y.txt\n"#;
        plant_transcript(
            &projects_dir(home),
            "-proj",
            "cccccccc-0000-0000-0000-000000000000",
            &format!(
                r#"{{"type":"hook_success","hookName":"SessionStart:clear","content":"{content}"}}"#
            ),
        );
        plant_emit_row(home, "2", "cccccccc", 0);
        let audit = audit_over(home);
        assert_eq!(audit[0].verdict, Verdict::Persisted);
    }

    #[test]
    fn no_matching_attachment_classifies_as_missing() {
        let tmp = tempdir().unwrap();
        let home = tmp.path();
        plant_transcript(
            &projects_dir(home),
            "-proj",
            "dddddddd-0000-0000-0000-000000000000",
            r#"{"type":"hook_success","hookName":"PostToolUse:Bash","content":"unrelated"}"#,
        );
        plant_emit_row(home, "2", "dddddddd", 0);
        let audit = audit_over(home);
        assert_eq!(audit[0].verdict, Verdict::Missing);
    }

    /// A deleted transcript is unknowable, not a failed delivery: folding it
    /// into `Missing` would inflate the loss rate the detector reports.
    #[test]
    fn absent_transcript_classifies_as_unknown() {
        let tmp = tempdir().unwrap();
        let home = tmp.path();
        plant_emit_row(home, "2", "eeeeeeee", 0);
        let audit = audit_over(home);
        assert_eq!(audit[0].verdict, Verdict::Unknown);
    }

    /// The regression that motivated `RELOAD_HOOK`: a startup attachment
    /// sits ahead of the clear one in the same transcript, and matching the
    /// event rather than the matcher reports the wrong hook's outcome.
    #[test]
    fn a_startup_attachment_does_not_stand_in_for_the_clear_one() {
        let tmp = tempdir().unwrap();
        let home = tmp.path();
        let dir = projects_dir(home).join("-proj");
        std::fs::create_dir_all(&dir).unwrap();
        let sid = "ffffffff-0000-0000-0000-000000000000";
        let startup = r#"{"type":"hook_success","hookName":"SessionStart:startup","content":"memory audit ok"}"#;
        let clear = r#"{"type":"hook_success","hookName":"SessionStart:clear","content":"<persisted-output>Output too large (14.9KB)."}"#;
        let body = format!(
            "{}\n{}\n",
            transcript_line(sid, startup),
            transcript_line(sid, clear)
        );
        std::fs::write(dir.join(format!("{sid}.jsonl")), body).unwrap();
        plant_emit_row(home, "2", "ffffffff", 0);
        let audit = audit_over(home);
        assert_eq!(audit[0].verdict, Verdict::Persisted);
    }

    /// One session can carry several clear attachments (transcript
    /// `e9b4ad27` carries two, 0.9s apart). Matching on `sid8` alone returns
    /// the first one in the file, so an outcome that predates the emission
    /// is reported as its own.
    #[test]
    fn an_earlier_attachment_is_not_this_emissions_outcome() {
        let tmp = tempdir().unwrap();
        let home = tmp.path();
        let dir = projects_dir(home).join("-proj");
        std::fs::create_dir_all(&dir).unwrap();
        let sid = "12121212-0000-0000-0000-000000000000";
        let earlier = r#"{"type":"hook_cancelled","hookName":"SessionStart:clear"}"#;
        let mine = r#"{"type":"hook_success","hookName":"SessionStart:clear","content":"Read the bundle"}"#;
        let body = format!(
            "{}\n{}\n",
            transcript_line_at(sid, -30, 0, earlier),
            transcript_line_at(sid, 5, 651, mine)
        );
        std::fs::write(dir.join(format!("{sid}.jsonl")), body).unwrap();
        plant_emit_row(home, "2", "12121212", 0);
        let audit = audit_over(home);
        assert_eq!(audit[0].verdict, Verdict::Delivered);
    }

    /// The other direction: a second `/clear` in a session whose only clear
    /// attachment belongs to the first one never landed, and counting it as
    /// delivered hides exactly the loss this detector exists to measure.
    #[test]
    fn an_emission_with_only_older_attachments_is_missing() {
        let tmp = tempdir().unwrap();
        let home = tmp.path();
        let dir = projects_dir(home).join("-proj");
        std::fs::create_dir_all(&dir).unwrap();
        let sid = "13131313-0000-0000-0000-000000000000";
        let earlier = r#"{"type":"hook_success","hookName":"SessionStart:clear","content":"the first brief"}"#;
        std::fs::write(
            dir.join(format!("{sid}.jsonl")),
            format!("{}\n", transcript_line_at(sid, -30, 0, earlier)),
        )
        .unwrap();
        plant_emit_row(home, "2", "13131313", 0);
        let audit = audit_over(home);
        assert_eq!(audit[0].verdict, Verdict::Missing);
    }

    /// `hook_non_blocking_error` attachments carry no body (1 of 366 on this
    /// host). Reading one as a delivery credits the emission with text that
    /// never reached a context, the exact loss this detector measures.
    #[test]
    fn an_attachment_with_no_body_is_not_a_delivery() {
        let tmp = tempdir().unwrap();
        let home = tmp.path();
        let dir = projects_dir(home).join("-proj");
        std::fs::create_dir_all(&dir).unwrap();
        let sid = "14141414-0000-0000-0000-000000000000";
        let empty = r#"{"type":"hook_non_blocking_error","hookName":"SessionStart:clear"}"#;
        std::fs::write(
            dir.join(format!("{sid}.jsonl")),
            format!("{}\n", transcript_line_at(sid, 5, 755, empty)),
        )
        .unwrap();
        plant_emit_row(home, "2", "14141414", 0);
        let audit = audit_over(home);
        assert_eq!(audit[0].verdict, Verdict::Missing);
    }

    /// The same empty attachment ahead of a real one must not swallow it.
    #[test]
    fn an_empty_attachment_does_not_hide_the_delivery_behind_it() {
        let tmp = tempdir().unwrap();
        let home = tmp.path();
        let dir = projects_dir(home).join("-proj");
        std::fs::create_dir_all(&dir).unwrap();
        let sid = "15151515-0000-0000-0000-000000000000";
        let empty = r#"{"type":"hook_non_blocking_error","hookName":"SessionStart:clear"}"#;
        let real = r#"{"type":"hook_success","hookName":"SessionStart:clear","content":"Read the bundle"}"#;
        let body = format!(
            "{}\n{}\n",
            transcript_line_at(sid, 5, 755, empty),
            transcript_line_at(sid, 6, 651, real)
        );
        std::fs::write(dir.join(format!("{sid}.jsonl")), body).unwrap();
        plant_emit_row(home, "2", "15151515", 0);
        let audit = audit_over(home);
        assert_eq!(audit[0].verdict, Verdict::Delivered);
    }

    #[test]
    fn declined_and_done_rows_are_not_delivery_attempts() {
        assert!(is_delivery_attempt("2"));
        assert!(is_delivery_attempt("2-stale"));
        assert!(!is_delivery_attempt("3-declined-cwdonly-fresh"));
        assert!(!is_delivery_attempt("3-declined-cwdonly-stale"));
        assert!(!is_delivery_attempt("2-done"));
        assert!(!is_delivery_attempt("1b-stale-done"));
    }

    /// A `-done` row would otherwise double-count its own emission once the
    /// Rust `reload` starts writing one.
    #[test]
    fn a_done_row_does_not_add_a_second_audit_entry() {
        let tmp = tempdir().unwrap();
        let home = tmp.path();
        plant_transcript(
            &projects_dir(home),
            "-proj",
            "77777777-0000-0000-0000-000000000000",
            r#"{"type":"hook_success","hookName":"SessionStart:clear","content":"ok"}"#,
        );
        plant_emit_row(home, "2", "77777777", 0);
        plant_emit_row(home, "2-done", "77777777", 0);
        let audit = audit_over(home);
        assert_eq!(audit.len(), 1);
        assert_eq!(audit[0].verdict, Verdict::Delivered);
    }

    /// A transcript carrying only a startup hook has no reload evidence.
    #[test]
    fn a_startup_only_transcript_classifies_as_missing() {
        let tmp = tempdir().unwrap();
        let home = tmp.path();
        plant_transcript(
            &projects_dir(home),
            "-proj",
            "99999999-0000-0000-0000-000000000000",
            r#"{"type":"hook_success","hookName":"SessionStart:startup","content":"ok"}"#,
        );
        plant_emit_row(home, "2", "99999999", 0);
        let audit = audit_over(home);
        assert_eq!(audit[0].verdict, Verdict::Missing);
    }

    #[test]
    fn counts_sum_to_row_total() {
        let tmp = tempdir().unwrap();
        let home = tmp.path();
        plant_transcript(
            &projects_dir(home),
            "-proj",
            "aaaaaaaa-0000-0000-0000-000000000000",
            r#"{"type":"hook_success","hookName":"SessionStart:clear","content":"ok"}"#,
        );
        plant_emit_row(home, "2", "aaaaaaaa", 0);
        plant_emit_row(home, "2", "eeeeeeee", 0);
        let audit = with_home(home, || audit_emit_log(None).unwrap());
        let mut counts = AuditCounts::default();
        for a in &audit {
            counts.add(a.verdict);
        }
        assert_eq!(counts.total(), 2);
    }

    #[test]
    fn since_window_excludes_older_rows() {
        let tmp = tempdir().unwrap();
        let home = tmp.path();
        plant_emit_row(home, "2", "ffffffff", 20);
        plant_emit_row(home, "2", "aaaaaaaa", 0);
        let audit = with_home(home, || audit_emit_log(Some(14)).unwrap());
        assert_eq!(audit.len(), 1);
        assert_eq!(audit[0].sid8, "aaaaaaaa");
    }

    fn audit_over(home: &Path) -> Vec<AuditRow> {
        with_home(home, || audit_emit_log(None).unwrap())
    }

    /// The subcommand may only ever create or append `watch.log`.
    #[test]
    fn run_writes_nothing_outside_watch_log() {
        let tmp = tempdir().unwrap();
        let home = tmp.path();
        std::fs::create_dir_all(home.join(".claude/sessions")).unwrap();
        std::fs::create_dir_all(home.join(".claude/projects")).unwrap();
        std::fs::create_dir_all(home.join(".claude/reseed/pending")).unwrap();
        plant_emit_row(home, "2", "aaaaaaaa", 0);

        let before = snapshot(home);
        with_home(home, || {
            run(WatchOpts {
                once: true,
                since_days: None,
                json: true,
                log_path: None,
                inject: None,
            })
            .unwrap()
        });
        let watch_log = home.join(".claude/reseed/watch.log");

        let after = snapshot(home);
        for (path, mtime) in &before {
            if *path == watch_log {
                continue;
            }
            assert_eq!(
                after.get(path),
                Some(mtime),
                "{} was modified by watch",
                path.display()
            );
        }
        assert!(watch_log.exists());
    }

    fn snapshot(dir: &Path) -> std::collections::BTreeMap<PathBuf, Duration> {
        let mut out = std::collections::BTreeMap::new();
        walk(dir, &mut out);
        out
    }

    fn walk(dir: &Path, out: &mut std::collections::BTreeMap<PathBuf, Duration>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk(&path, out);
            } else if let Ok(meta) = std::fs::metadata(&path) {
                let mtime = meta
                    .modified()
                    .ok()
                    .and_then(|m| m.duration_since(SystemTime::UNIX_EPOCH).ok())
                    .unwrap_or_default();
                out.insert(path, mtime);
            }
        }
    }
}
