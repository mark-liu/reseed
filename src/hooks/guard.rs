//! Stop hook: guarantee the reset reminder reaches the operator. Ported
//! from `context-reset-guard.py`; P9 fail-open, P12 texts.

use super::Payload;
use crate::{msg, paths, sentinel, spawn, usage};
use regex::Regex;
use serde::Deserialize;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use std::sync::OnceLock;
use std::thread::sleep;

/// Public-safe fallbacks (P12). A site's own wording - including whatever
/// incident made the rule matter there - belongs in `$RESEED_MESSAGES`.
const GUARD_ARMED_DEFAULT: &str =
    "reseed: context ~{ctx}k, halt at {line}k, {how} at the next natural break";

const GUARD_PAST_LINE_DEFAULT: &str =
    "context-reset-guard: context is ~{ctx}k, past the {line}k reset line, and your \
     reply never told {op} to reset - that is how a reload gets buried, and the turn \
     that would have carried it is lost. Re-send the SAME reply with one extra final \
     line, after done:/next:, reading: 'RESET NOW ({ctx}k): {how}'. Do not restate \
     anything else, do not start new work at this context size.";
use std::time::{Duration, SystemTime};

const SETTLE_FLOOR: Duration = Duration::from_millis(1500);
const SETTLE_TRIES: u32 = 6;
const SETTLE_WAIT: Duration = Duration::from_millis(250);
const TAIL_BYTES: u64 = 262_144;

fn delivered_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r#"(?i)(/clear\b|reseed-here|reseed/clear|reset (?:the |this )?session|type ['"`]?go['"`]?\b)"#)
            .unwrap()
    })
}

fn wait_until_settled(path: &Path) {
    sleep(SETTLE_FLOOR);
    let mut prev: i64 = -1;
    for _ in 0..SETTLE_TRIES {
        let Ok(meta) = std::fs::metadata(path) else {
            return;
        };
        let cur = meta.len() as i64;
        if cur == prev {
            return;
        }
        prev = cur;
        sleep(SETTLE_WAIT);
    }
}

#[derive(Deserialize)]
struct Entry {
    #[serde(rename = "type")]
    kind: Option<String>,
    #[serde(rename = "isSidechain")]
    is_sidechain: Option<bool>,
    message: Option<Msg>,
}
#[derive(Deserialize)]
struct Msg {
    content: Option<Vec<Block>>,
}
#[derive(Deserialize)]
struct Block {
    #[serde(rename = "type")]
    kind: Option<String>,
    text: Option<String>,
}

/// Newest main-thread assistant text, scanned from the last 256KB.
fn last_assistant_text(path: &Path) -> String {
    let Ok(mut f) = std::fs::File::open(path) else {
        return String::new();
    };
    let Ok(size) = f.metadata().map(|m| m.len()) else {
        return String::new();
    };
    let start = size.saturating_sub(TAIL_BYTES);
    if f.seek(SeekFrom::Start(start)).is_err() {
        return String::new();
    }
    let mut buf = Vec::new();
    if f.read_to_end(&mut buf).is_err() {
        return String::new();
    }
    let tail = String::from_utf8_lossy(&buf);
    for line in tail.lines().rev() {
        if !line.contains("\"assistant\"") {
            continue;
        }
        let Ok(entry) = serde_json::from_str::<Entry>(line) else {
            continue;
        };
        if entry.kind.as_deref() != Some("assistant") || entry.is_sidechain.unwrap_or(false) {
            continue;
        }
        let Some(msg) = entry.message else { continue };
        let Some(blocks) = msg.content else { continue };
        let txt = blocks
            .into_iter()
            .filter(|b| b.kind.as_deref() == Some("text"))
            .map(|b| b.text.unwrap_or_default())
            .collect::<Vec<_>>()
            .join("\n");
        if !txt.trim().is_empty() {
            return txt;
        }
    }
    String::new()
}

fn read_tier(path: &Path) -> u8 {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
        .and_then(|v| v.get("tier").and_then(|t| t.as_u64()))
        .unwrap_or(0) as u8
}

fn write_tier(path: &Path, tier: u8) {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = crate::atomic::write(
        path,
        serde_json::json!({"tier": tier}).to_string().as_bytes(),
    );
}

fn sentinel_armed(session: &str) -> bool {
    let Ok(pending) = paths::pending() else {
        return false;
    };
    match sentinel::read(&pending, session) {
        Some(arm) => sentinel::is_fresh(&arm, SystemTime::now()),
        None => false,
    }
}

pub fn run(p: Payload) -> i32 {
    if p.stop_hook_active {
        return 0;
    }
    let Some(transcript) = p.transcript_path.filter(|t| !t.is_empty()) else {
        return 0;
    };
    let session = super::session_key(p.session_id.as_deref().unwrap_or("d"));
    let Ok(state_dir) = paths::guard_state() else {
        return 0;
    };
    let state_path = state_dir.join(format!("{session}.json"));

    let transcript_path = Path::new(&transcript);
    wait_until_settled(transcript_path);
    let (tier, ctx, line, early) = usage::context_state(transcript_path);
    let Some(ctx) = ctx else { return 0 };

    if tier == 0 {
        if read_tier(&state_path) != 0 {
            write_tier(&state_path, 0);
        }
        if ctx < early {
            return 0;
        }
        let how = if sentinel_armed(&session) {
            "bundle armed: /clear then 'go'".to_string()
        } else if spawn::rearm(&session) {
            "distill running: /clear then 'go' (`! reseed-here` first only if 'go' injects nothing)"
                .to_string()
        } else {
            "no bundle armed: `! reseed-here` then /clear then 'go'".to_string()
        };
        println!(
            "{}",
            serde_json::json!({
                "systemMessage": msg::fill(
                    &msg::text("guard-armed", GUARD_ARMED_DEFAULT),
                    &[
                        ("ctx", &(ctx / 1000).to_string()),
                        ("line", &(line / 1000).to_string()),
                        ("how", &how),
                    ],
                )
            })
        );
        return 0;
    }

    let armed = sentinel_armed(&session);
    let spawned = if !armed {
        spawn::rearm(&session)
    } else {
        false
    };

    if tier <= read_tier(&state_path) {
        return 0;
    }

    let reply = last_assistant_text(transcript_path);
    write_tier(&state_path, tier);
    if delivered_re().is_match(&reply) {
        return 0;
    }

    let how = if armed {
        "/clear then type 'go' (the reload bundle is already distilled and armed)".to_string()
    } else if spawned {
        "/clear then type 'go' (a distill is running in the background right now; \
         only if 'go' injects nothing, fall back to `! reseed-here` then /clear \
         then 'go')"
            .to_string()
    } else {
        "! reseed-here then /clear then 'go' (no fresh bundle is armed, \
         so distil first)"
            .to_string()
    };
    eprintln!(
        "{}",
        msg::fill(
            &msg::text("guard-past-line", GUARD_PAST_LINE_DEFAULT),
            &[
                ("ctx", &(ctx / 1000).to_string()),
                ("line", &(line / 1000).to_string()),
                ("op", &msg::operator()),
                ("how", &how),
            ],
        )
    );
    2
}
