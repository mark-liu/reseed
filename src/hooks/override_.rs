//! UserPromptSubmit hook: let the operator, and only the operator, lift the
//! reset halt. Ported from `reset-override.py`; P9 fail-open, P12 texts.

use super::Payload;
use crate::{msg, paths, usage};
use regex::Regex;
use std::path::Path;

/// Public-safe fallback (P12); a site overrides it through `$RESEED_MESSAGES`.
const OVERRIDE_LIFTED_DEFAULT: &str =
    "Reset halt LIFTED by {op} for this tier ({ctx}k, line {line}k). Tool calls work \
     again. This is not a licence to start a fresh multi-step task: finish what they \
     asked, keep the reply short, and say the reset is still owed. Crossing the next \
     tier re-halts.";
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

const TTL_SECS: f64 = 90.0 * 60.0;

fn phrase_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?i)\b(override reset|force past reset|ignore the reset|no clear)\b").unwrap()
    })
}

fn now_secs() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
}

pub fn run(p: Payload) -> i32 {
    let Some(prompt) = p.prompt.filter(|s| phrase_re().is_match(s)) else {
        return 0;
    };
    let _ = prompt;
    let transcript = p.transcript_path.unwrap_or_default();
    let session = super::session_key(p.session_id.as_deref().unwrap_or("d"));
    let (tier, ctx, line) = usage::context_tier(Path::new(&transcript));
    let Some(ctx) = ctx else { return 0 };
    if tier < 1 {
        return 0;
    }
    let Ok(dir) = paths::guard_state() else {
        return 0;
    };
    let path = dir.join(format!("{session}.override.json"));
    let body = serde_json::json!({"tier": tier, "expires": now_secs() + TTL_SECS}).to_string();
    if crate::atomic::write(&path, body.as_bytes()).is_err() {
        return 0;
    }
    println!(
        "{}",
        serde_json::json!({
            "hookSpecificOutput": {
                "hookEventName": "UserPromptSubmit",
                "additionalContext": msg::fill(
                    &msg::text("override-lifted", OVERRIDE_LIFTED_DEFAULT),
                    &[
                        ("op", &msg::operator()),
                        ("ctx", &(ctx / 1000).to_string()),
                        ("line", &(line / 1000).to_string()),
                    ],
                ),
            }
        })
    );
    0
}
