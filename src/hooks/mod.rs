//! Shared hook payload parsing. Every hook reads the same stdin JSON shape
//! and fails open on any parse error (P9).

pub mod guard;
pub mod halt;
pub mod ledger_guard;
pub mod nudge;
pub mod override_;
pub mod probe_guard;

use regex::Regex;
use serde::Deserialize;
use std::io::Read;
use std::sync::OnceLock;

const STDIN_CAP: usize = 4_000_000;

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
            out.push(" ".repeat(lines[i].len()));
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
