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
    let re = Regex::new(r"[^A-Za-z0-9._-]").unwrap();
    let cleaned = re.replace_all(sid, "_");
    cleaned.chars().take(128).collect()
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
