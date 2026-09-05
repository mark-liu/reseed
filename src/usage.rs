//! Session-size introspection, ported from `ctxstate.py`. Shared by the
//! nudge, guard and halt hooks so all three agree on the tier.

use anyhow::Result;
use serde::Deserialize;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

const TAIL_TIERS: [u64; 3] = [131_072, 1_048_576, 8_388_608];
const DEFAULT_WINDOW: u64 = 200_000;
const DEFAULT_RESET_LINE: u64 = 300_000;
const TIER_MULTS: [f64; 3] = [1.0, 1.25, 1.417];
const TIER_WINDOW_CAPS: [f64; 3] = [0.75, 0.875, 0.95];
const EARLY_FRACTION: f64 = 0.8;

#[derive(Debug, Clone)]
pub struct Snapshot {
    pub sid: String,
    pub ctx: Option<u64>,
    pub window: u64,
    pub model: Option<String>,
    pub cwd: Option<String>,
    pub session_kind: Option<String>,
    pub stop_reason: Option<String>,
    pub mtime: Option<std::time::SystemTime>,
}

#[derive(Deserialize)]
struct Entry {
    #[serde(rename = "type")]
    kind: Option<String>,
    #[serde(rename = "isSidechain")]
    is_sidechain: Option<bool>,
    message: Option<Message>,
}

#[derive(Deserialize)]
struct Message {
    model: Option<String>,
    usage: Option<Usage>,
}

#[derive(Deserialize)]
struct Usage {
    input_tokens: Option<u64>,
    cache_read_input_tokens: Option<u64>,
    cache_creation_input_tokens: Option<u64>,
}

/// The reset line in tokens, overridable via `STATUSLINE_RESET_LINE`.
pub fn reset_line() -> u64 {
    std::env::var("STATUSLINE_RESET_LINE")
        .ok()
        .and_then(|v| v.parse::<i64>().ok())
        .filter(|v| *v > 0)
        .map(|v| v as u64)
        .unwrap_or(DEFAULT_RESET_LINE)
}

/// Effective context window for a transcript's model id: `[1m]` is 1M,
/// `haiku` is the one family never observed above 200k, else 1M.
pub fn window_for_model(model: Option<&str>) -> u64 {
    let m = model.unwrap_or("").to_lowercase();
    if m.contains("[1m]") {
        1_000_000
    } else if m.contains("haiku") {
        DEFAULT_WINDOW
    } else {
        1_000_000
    }
}

/// The three absolute nudge lines for this window, low to high.
pub fn tier_lines(window: u64) -> [u64; 3] {
    let line = reset_line() as f64;
    let mut out = [0u64; 3];
    for i in 0..3 {
        let capped = (line * TIER_MULTS[i]).min(TIER_WINDOW_CAPS[i] * window as f64);
        out[i] = capped as u64;
    }
    out
}

/// Where the early band starts: `EARLY_FRACTION` of the tier-1 line.
pub fn early_line(window: u64) -> u64 {
    (tier_lines(window)[0] as f64 * EARLY_FRACTION) as u64
}

/// Count of lines `ctx` is at or past, 0..3. `None` ctx is tier 0, never
/// conflated with "known and under the line".
pub fn tier_of(ctx: Option<u64>, lines: [u64; 3]) -> u8 {
    match ctx {
        None => 0,
        Some(c) => lines.iter().filter(|&&line| c >= line).count() as u8,
    }
}

/// (ctx tokens, window) from the newest main-thread assistant usage line,
/// escalating the tail read through three tiers when nothing is found.
pub fn last_context_tokens(transcript_path: &Path) -> (Option<u64>, u64) {
    let size = match std::fs::metadata(transcript_path) {
        Ok(m) => m.len(),
        Err(_) => return (None, DEFAULT_WINDOW),
    };
    for &tail_bytes in TAIL_TIERS.iter() {
        let tail = match read_tail(transcript_path, size, tail_bytes) {
            Ok(t) => t,
            Err(_) => return (None, DEFAULT_WINDOW),
        };
        for line in tail.lines().rev() {
            if !line.contains("\"usage\"") {
                continue;
            }
            let entry: Entry = match serde_json::from_str(line) {
                Ok(e) => e,
                Err(_) => continue,
            };
            if entry.kind.as_deref() != Some("assistant") || entry.is_sidechain.unwrap_or(false) {
                continue;
            }
            let msg = match entry.message {
                Some(m) => m,
                None => continue,
            };
            let usage = match msg.usage {
                Some(u) => u,
                None => continue,
            };
            let input = match usage.input_tokens {
                Some(v) => v,
                None => continue,
            };
            let window = window_for_model(msg.model.as_deref());
            let ctx = input
                + usage.cache_read_input_tokens.unwrap_or(0)
                + usage.cache_creation_input_tokens.unwrap_or(0);
            return (Some(ctx), window);
        }
        if tail_bytes >= size {
            break;
        }
    }
    (None, DEFAULT_WINDOW)
}

/// Read the last `tail_bytes` of a file, lossily decoded: a seek may land
/// mid-codepoint and must never panic.
fn read_tail(path: &Path, size: u64, tail_bytes: u64) -> Result<String> {
    let mut f = File::open(path)?;
    let start = size.saturating_sub(tail_bytes);
    f.seek(SeekFrom::Start(start))?;
    let mut buf = Vec::new();
    f.take(tail_bytes).read_to_end(&mut buf)?;
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

/// (tier, ctx, tier-1 line, early line) for a transcript path.
pub fn context_state(transcript_path: &Path) -> (u8, Option<u64>, u64, u64) {
    let (ctx, window) = last_context_tokens(transcript_path);
    let lines = tier_lines(window);
    let early = early_line(window);
    if ctx.is_none() {
        return (0, None, lines[0], early);
    }
    (tier_of(ctx, lines), ctx, lines[0], early)
}

/// (tier, ctx, tier-1 line): `context_state` without the early line.
pub fn context_tier(transcript_path: &Path) -> (u8, Option<u64>, u64) {
    let (tier, ctx, line, _early) = context_state(transcript_path);
    (tier, ctx, line)
}

/// Best-effort snapshot for `reseed ctx --json`.
pub fn snapshot(sid: &str, transcript_path: &Path) -> Snapshot {
    let (ctx, window) = last_context_tokens(transcript_path);
    let mtime = std::fs::metadata(transcript_path)
        .and_then(|m| m.modified())
        .ok();
    Snapshot {
        sid: sid.to_string(),
        ctx,
        window,
        model: None,
        cwd: None,
        session_kind: None,
        stop_reason: None,
        mtime,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    fn transcript_with(ctx_tokens: u64, model: &str) -> NamedTempFile {
        let mut f = NamedTempFile::new().unwrap();
        let line = serde_json::json!({
            "type": "assistant",
            "message": {
                "model": model,
                "usage": {
                    "input_tokens": 1000,
                    "cache_read_input_tokens": ctx_tokens - 1000,
                    "cache_creation_input_tokens": 0,
                }
            }
        });
        writeln!(f, "{line}").unwrap();
        f
    }

    #[test]
    fn window_for_model_variants() {
        assert_eq!(window_for_model(Some("claude-opus-5[1m]")), 1_000_000);
        assert_eq!(window_for_model(Some("claude-haiku-4-5")), 200_000);
        assert_eq!(window_for_model(Some("claude-opus-5")), 1_000_000);
        assert_eq!(window_for_model(None), 1_000_000);
    }

    #[test]
    fn tier_lines_default_300k_window_1m() {
        let lines = tier_lines(1_000_000);
        assert_eq!(lines, [300_000, 375_000, 425_100]);
    }

    #[test]
    fn tier_lines_capped_by_small_window() {
        let lines = tier_lines(200_000);
        assert_eq!(lines, [150_000, 175_000, 190_000]);
    }

    #[test]
    fn early_line_is_80pct_of_tier1() {
        assert_eq!(early_line(1_000_000), 240_000);
    }

    #[test]
    fn tier_of_counts_lines_crossed() {
        let lines = [300_000, 375_000, 425_000];
        assert_eq!(tier_of(None, lines), 0);
        assert_eq!(tier_of(Some(100_000), lines), 0);
        assert_eq!(tier_of(Some(310_000), lines), 1);
        assert_eq!(tier_of(Some(400_000), lines), 2);
        assert_eq!(tier_of(Some(500_000), lines), 3);
    }

    #[test]
    fn last_context_tokens_reads_newest_usage() {
        let f = transcript_with(250_000, "claude-opus-5");
        let (ctx, window) = last_context_tokens(f.path());
        assert_eq!(ctx, Some(250_000));
        assert_eq!(window, 1_000_000);
    }

    #[test]
    fn last_context_tokens_ignores_sidechain() {
        let mut f = NamedTempFile::new().unwrap();
        let side = serde_json::json!({
            "type": "assistant", "isSidechain": true,
            "message": {"model": "claude-opus-5", "usage": {"input_tokens": 900_000}}
        });
        let main = serde_json::json!({
            "type": "assistant",
            "message": {"model": "claude-opus-5", "usage": {"input_tokens": 250_000}}
        });
        writeln!(f, "{side}").unwrap();
        writeln!(f, "{main}").unwrap();
        let (ctx, _) = last_context_tokens(f.path());
        assert_eq!(ctx, Some(250_000));
    }

    #[test]
    fn missing_transcript_is_none() {
        let (ctx, window) = last_context_tokens(Path::new("/nonexistent/x.jsonl"));
        assert_eq!(ctx, None);
        assert_eq!(window, DEFAULT_WINDOW);
    }

    #[test]
    fn context_state_unknown_is_tier_zero_not_conflated() {
        let mut f = NamedTempFile::new().unwrap();
        writeln!(f, "not usable").unwrap();
        let (tier, ctx, _line, _early) = context_state(f.path());
        assert_eq!(tier, 0);
        assert_eq!(ctx, None);
    }

    #[test]
    fn reset_line_override_via_env() {
        std::env::set_var("STATUSLINE_RESET_LINE", "30000");
        assert_eq!(reset_line(), 30_000);
        std::env::remove_var("STATUSLINE_RESET_LINE");
    }

    #[test]
    fn reset_line_ignores_non_positive_override() {
        std::env::set_var("STATUSLINE_RESET_LINE", "-5");
        assert_eq!(reset_line(), DEFAULT_RESET_LINE);
        std::env::remove_var("STATUSLINE_RESET_LINE");
    }

    #[test]
    fn read_tail_handles_mid_codepoint_seek() {
        // A multi-byte UTF-8 char straddling the tail boundary must not panic.
        let mut f = NamedTempFile::new().unwrap();
        let mut bytes = vec![b'x'; 10];
        bytes.extend_from_slice("café".as_bytes());
        f.write_all(&bytes).unwrap();
        let size = bytes.len() as u64;
        let tail = read_tail(f.path(), size, 11).unwrap();
        assert!(!tail.is_empty());
    }
}
