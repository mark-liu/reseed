//! PreToolUse hook: halt the session once it is past the reset line.
//! Ported from `context-reset-halt.py`; P9 fail-open, P12 texts.

use super::Payload;
use crate::{paths, sentinel, spawn, usage};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

fn operator() -> String {
    std::env::var("RESEED_OPERATOR").unwrap_or_else(|_| "Mark".to_string())
}

fn now_secs() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
}

/// Tier the operator has explicitly overridden, or -1 (unset or expired).
fn override_tier(session: &str) -> i64 {
    let Ok(dir) = paths::guard_state() else {
        return -1;
    };
    let path = dir.join(format!("{session}.override.json"));
    let Ok(body) = std::fs::read_to_string(&path) else {
        return -1;
    };
    let Ok(data) = serde_json::from_str::<serde_json::Value>(&body) else {
        return -1;
    };
    let expires = data.get("expires").and_then(|v| v.as_f64()).unwrap_or(0.0);
    if expires < now_secs() {
        return -1;
    }
    data.get("tier").and_then(|v| v.as_i64()).unwrap_or(-1)
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
    if std::env::var("CLAUDE_NO_RESET_HALT").as_deref() == Ok("1") {
        return 0;
    }
    if p.agent_id.is_some() {
        return 0;
    }
    let Some(transcript) = p.transcript_path.filter(|t| !t.is_empty()) else {
        return 0;
    };
    let session = super::session_key(p.session_id.as_deref().unwrap_or("d"));
    let (tier, ctx, line) = usage::context_tier(Path::new(&transcript));
    let Some(ctx) = ctx else { return 0 };
    if tier < 1 {
        return 0;
    }
    if override_tier(&session) >= tier as i64 {
        return 0;
    }

    if !sentinel_armed(&session) {
        spawn::rearm(&session);
    }

    let reason = format!(
        "HALTED: context is ~{ctx}k, past the {line}k reset line. Work done from here \
         is not in the reload bundle - a whole turn was lost this way, \
         so the session pauses instead of advising. Tell {op}, in text, exactly where \
         this task got to and what the next step is, then STOP and wait: he runs \
         /clear and types 'go' to continue in a fresh session with the narrative \
         reloaded. The reload bundle is already armed or distilling in the background \
         right now, so no `! reseed-here` is needed first. \
         If the `rename-thread` skill never fired at the nudge tier, this thread still \
         carries a stale title and no tool call can fix that now - so END your handoff \
         text with a suggested title line (`suggested title: <3-6 words>`) that {op} can \
         paste after /rename. \
         Neither /clear nor `! reseed-here` is a tool call, so the way out is \
         not blocked. Do NOT retry this call, do not route around it with another tool, \
         and do not ask for the override - only {op} can lift it, by typing \
         'override reset'.",
        ctx = ctx / 1000,
        line = line / 1000,
        op = operator(),
    );
    println!(
        "{}",
        serde_json::json!({
            "hookSpecificOutput": {
                "hookEventName": "PreToolUse",
                "permissionDecision": "deny",
                "permissionDecisionReason": reason,
            }
        })
    );
    0
}
