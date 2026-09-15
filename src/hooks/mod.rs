//! Shared hook payload parsing. Every hook reads the same stdin JSON shape
//! and fails open on any parse error (P9).

pub mod guard;
pub mod halt;
pub mod ledger_guard;
pub mod nudge;
pub mod override_;
pub mod probe_guard;

use crate::{msg, paths};
use regex::Regex;
use serde::Deserialize;
use std::io::Read;
use std::sync::OnceLock;
use std::time::{Duration, SystemTime};

const STDIN_CAP: usize = 4_000_000;

/// The watcher ticks every few seconds, so a quieter log means no pass runs.
const WATCH_FRESH: Duration = Duration::from_secs(60);

/// Public-safe fallbacks (P12) shared by the injector branch of every hook.
const INJECTOR_STEP_DEFAULT: &str =
    "This is a background job and the reseed injector is live: once this turn ends it \
     runs /clear and types 'go' itself. Do NOT advise {op} to /clear or type 'go'.";

/// Operator-facing, so it carries the fallback: a spawned rearm is not a
/// written sentinel, and an attached viewer holds the injector off.
const INJECTOR_NOTE_DEFAULT: &str =
    "the injector auto-clears this job once the turn ends and nobody is attached or \
     typing; if it is still here 2 min later, /clear then 'go'";

/// True when `reseed watch --inject` will clear this session unaided. Only a
/// background job has a daemon pty to type into, so a terminal session never
/// is. The kill file is checked on its own because a stopped watcher still
/// logs an `off` row every tick. Callers must also know a bundle is armed or
/// distilling: the injector skips an unarmed session.
pub fn injector_live() -> bool {
    match std::env::var_os("CLAUDE_JOB_DIR") {
        Some(v) if !v.is_empty() => {}
        _ => return false,
    }
    let (Ok(kill), Ok(log)) = (paths::kill_file(), paths::watch_log()) else {
        return false;
    };
    if kill.exists() {
        return false;
    }
    // A future mtime (clock step) counts as fresh, as in the Python predicate.
    std::fs::metadata(log)
        .and_then(|m| m.modified())
        .map(|t| SystemTime::now().duration_since(t).unwrap_or_default() < WATCH_FRESH)
        .unwrap_or(false)
}

/// Model-facing: the injector does the reset, so do not ask for one.
pub fn injector_step() -> String {
    msg::fill(
        &msg::text("injector-step", INJECTOR_STEP_DEFAULT),
        &[("op", &msg::operator())],
    )
}

/// Operator-facing banner tail for the injector branch.
pub fn injector_note() -> String {
    msg::text("injector-note", INJECTOR_NOTE_DEFAULT)
}

#[derive(Debug, Default, Deserialize)]
pub struct Payload {
    pub session_id: Option<String>,
    pub cwd: Option<String>,
    pub transcript_path: Option<String>,
    pub agent_id: Option<String>,
    #[serde(default)]
    pub stop_hook_active: bool,
    pub prompt: Option<String>,
    #[serde(default)]
    pub tool_input: ToolInput,
}

#[derive(Debug, Default, Deserialize)]
pub struct ToolInput {
    pub command: Option<String>,
}

/// Read and parse the hook payload from stdin, capped at 4MB. `None` on any
/// read or parse error, or when the body is not a JSON object: the caller
/// exits 0 silently (fail-open, P9).
pub fn read_payload() -> Option<Payload> {
    let mut buf = Vec::new();
    std::io::stdin()
        .take(STDIN_CAP as u64)
        .read_to_end(&mut buf)
        .ok()?;
    let value: serde_json::Value = serde_json::from_slice(&buf).ok()?;
    if !value.is_object() {
        return None;
    }
    serde_json::from_value(value).ok()
}

/// Sanitise a session id into a state-file key, 128 chars max.
pub fn session_key(sid: &str) -> String {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| Regex::new(r"[^A-Za-z0-9._-]").unwrap());
    let cleaned = re.replace_all(sid, "_");
    cleaned.chars().take(128).collect()
}

fn heredoc_start() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    // No backreference (P4): the closing quote is not re-checked against the
    // opening one, a heuristic mismatch cost worth avoiding the `regex` gap.
    RE.get_or_init(|| Regex::new(r#"<<-?\s*['"]?(\w+)"#).unwrap())
}

/// Blank out heredoc bodies, `<<WORD` up to the matching terminator line,
/// keeping line count and line lengths so later offsets still line up.
/// Shared by every guard that scans command text: a file whose CONTENT quotes
/// a blocked command is prose, not the command (spec 3.4).
pub fn mask_heredocs(command: &str) -> String {
    let lines: Vec<&str> = command.split('\n').collect();
    let start_re = heredoc_start();
    let mut out: Vec<String> = Vec::with_capacity(lines.len());
    let mut i = 0;
    while i < lines.len() {
        if let Some(cap) = start_re.captures(lines[i]) {
            let word = cap.get(1).unwrap().as_str().to_string();
            // Keep the opener's prefix: `cat >> <ledger> <<'PARK'` carries the
            // real redirect, and masking it hid the append from the ledger
            // guard's own-line self-check.
            let at = cap.get(0).unwrap().start();
            let mut masked = lines[i][..at].to_string();
            masked.push_str(&" ".repeat(lines[i].len() - at));
            out.push(masked);
            i += 1;
            while i < lines.len() {
                let terminator = lines[i].trim();
                out.push(" ".repeat(lines[i].len()));
                let matched = terminator == word;
                i += 1;
                if matched {
                    break;
                }
            }
            continue;
        }
        out.push(lines[i].to_string());
        i += 1;
    }
    out.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_key_defaults_and_truncates() {
        assert_eq!(session_key("abc"), "abc");
        assert_eq!(session_key("a/b"), "a_b");
        let long = "a".repeat(200);
        assert_eq!(session_key(&long).len(), 128);
    }
}
