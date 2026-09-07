//! PostToolUse nudge: tell the driver to reseed/clear past the yellow line.
//! Ported from `context-reseed-nudge.py`; P9 fail-open, P12 texts.

use super::Payload;
use crate::{msg, paths, sentinel, spawn, usage};
use serde::{Deserialize, Serialize};
use std::path::Path;

/// Public-safe fallbacks (P12); a site overrides any of them through
/// `$RESEED_MESSAGES`.
const NUDGE_EARLY_BANNER_DEFAULT: &str =
    "reseed-nudge: context ~{ctx}k, {pct}% of {line}k - bundle arming, /clear then 'go' \
     before the halt";

const NUDGE_EARLY_DEFAULT: &str =
    "Context is at ~{ctx}k tokens, {pct}% of the {line}k yellow line, and every tool \
     call is DENIED at {line}k. Reset now, while it is cheap. FIRST invoke the \
     `rename-thread` skill (after /clear no title can be derived). Then advise {op}: \
     {step} Do not tell them to run `! reseed-here` first. Finish only a near-done \
     step; do not start new multi-step work at this size.";

const NUDGE_TIER_BANNER_DEFAULT: &str =
    "reseed-nudge: context ~{ctx}k > {line}k - reseed/clear recommended";

const NUDGE_TIER_DEFAULT: &str =
    "Context is at ~{ctx}k tokens (yellow line {line}k). This session is past its reset \
     point - every further turn pays a latency and recall tax. FIRST invoke the \
     `rename-thread` skill - after /clear the conversation is gone, so this is the LAST \
     moment a title can be derived from what this session actually did, and the /resume \
     picker entry is all a future session has to find it by. Then advise {op}: {step} \
     For a new task just /clear. Finish only a near-done step first; do not start new \
     multi-step work at this context size.";
use std::time::{SystemTime, UNIX_EPOCH};

const REARM_INTERVAL_SECS: f64 = 480.0;

#[derive(Debug, Default, Serialize, Deserialize)]
struct State {
    #[serde(default)]
    tier: u8,
    #[serde(default)]
    armed_at: f64,
    #[serde(default)]
    early: bool,
}

fn now_secs() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
}

fn state_path(session: &str) -> Option<std::path::PathBuf> {
    Some(paths::nudge_state().ok()?.join(format!("{session}.json")))
}

fn read_state(path: &Path) -> State {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn write_state(path: &Path, state: &State) {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(body) = serde_json::to_vec(state) {
        let _ = crate::atomic::write(path, &body);
    }
}

/// Returns the exit code. Prints the nudge JSON to stdout when firing.
pub fn run(p: Payload) -> i32 {
    if p.agent_id.is_some() {
        return 0;
    }
    let transcript = match p.transcript_path {
        Some(t) if !t.is_empty() => t,
        _ => return 0,
    };
    let session = super::session_key(p.session_id.as_deref().unwrap_or("d"));
    let (ctx, window) = usage::last_context_tokens(Path::new(&transcript));
    let Some(ctx) = ctx else { return 0 };

    let lines = usage::tier_lines(window);
    let tier = usage::tier_of(Some(ctx), lines);
    let Some(state_path) = state_path(&session) else {
        return 0;
    };
    let mut state = read_state(&state_path);

    if tier < state.tier {
        write_state(
            &state_path,
            &State {
                tier,
                armed_at: 0.0,
                early: false,
            },
        );
        return 0;
    }

    let now = now_secs();
    let early = usage::early_line(window);
    if ctx >= early && (now - state.armed_at) >= REARM_INTERVAL_SECS && spawn::rearm(&session) {
        state.armed_at = now;
    }

    if tier == 0 && ctx < early && state.early {
        state.early = false;
    }

    if tier == 0 && ctx >= early && !state.early {
        let ctx_k = ctx / 1000;
        let line_k = lines[0] / 1000;
        let pct = (100.0 * early as f64 / lines[0] as f64).round() as u64;
        let reset_step = if sentinel_armed(&session) {
            "The reload bundle is ALREADY distilled and armed, so the reset is just \
             /clear then type 'go'."
                .to_string()
        } else {
            "A distill was just kicked off in the background, so by the end of this \
             turn the reset is /clear then 'go'. Only if 'go' injects nothing: \
             `! reseed-here` then /clear then 'go'."
                .to_string()
        };
        let early_msg = msg::fill(
            &msg::text("nudge-early", NUDGE_EARLY_DEFAULT),
            &[
                ("ctx", &ctx_k.to_string()),
                ("pct", &pct.to_string()),
                ("line", &line_k.to_string()),
                ("op", &msg::operator()),
                ("step", &reset_step),
            ],
        );
        println!(
            "{}",
            serde_json::json!({
                "systemMessage": msg::fill(
                    &msg::text("nudge-early-banner", NUDGE_EARLY_BANNER_DEFAULT),
                    &[
                        ("ctx", &ctx_k.to_string()),
                        ("pct", &pct.to_string()),
                        ("line", &line_k.to_string()),
                    ],
                ),
                "hookSpecificOutput": {"hookEventName": "PostToolUse", "additionalContext": early_msg},
            })
        );
        state.tier = tier;
        state.early = true;
        write_state(&state_path, &state);
        return 0;
    }

    if tier == 0 || tier == state.tier {
        write_state(&state_path, &state);
        return 0;
    }

    let ctx_k = ctx / 1000;
    let line_k = lines[0] / 1000;
    let reset_step = if sentinel_armed(&session) {
        "The reload bundle is ALREADY distilled and armed, so the reset is just \
         /clear then type 'go' - do NOT run `! reseed-here` first."
            .to_string()
    } else {
        "A distill was just kicked off in the background; by the time you finish \
         this turn /clear then 'go' should reload. If 'go' injects nothing, fall \
         back to `! reseed-here` then /clear then 'go'."
            .to_string()
    };
    let ctx_msg = msg::fill(
        &msg::text("nudge-tier", NUDGE_TIER_DEFAULT),
        &[
            ("ctx", &ctx_k.to_string()),
            ("line", &line_k.to_string()),
            ("op", &msg::operator()),
            ("step", &reset_step),
        ],
    );
    println!(
        "{}",
        serde_json::json!({
            "systemMessage": msg::fill(
                &msg::text("nudge-tier-banner", NUDGE_TIER_BANNER_DEFAULT),
                &[("ctx", &ctx_k.to_string()), ("line", &line_k.to_string())],
            ),
            "hookSpecificOutput": {"hookEventName": "PostToolUse", "additionalContext": ctx_msg},
        })
    );
    state.tier = tier;
    write_state(&state_path, &state);
    0
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
