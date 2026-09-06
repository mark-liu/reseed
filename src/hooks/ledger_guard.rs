//! PreToolUse hook: blocks positional reads of the park ledger. Ported from
//! `park-ledger-scope-guard.py`; P4 hand scanner for the heredoc mask, P12
//! texts (no internal citations).

use super::Payload;
use regex::Regex;
use std::sync::OnceLock;

fn ledger_ref() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?:~|\$HOME|/Users/[^/\s]+)/scratch/parked/(?P<tail>\S*)").unwrap()
    })
}

fn positional_reader() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"(?:^|[|;&(]|\s)(?:/bin/|/usr/bin/)?(?:tail|head|cat|less|more|bat|sed|awk|open)\b",
        )
        .unwrap()
    })
}

fn content_select() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?:^|[|;&]|\s)(?:/usr/bin/)?(?:grep|rg|ag|ugrep)\b").unwrap())
}

fn survey_marker() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"#\s*ledger-survey\b").unwrap())
}

fn segment_split() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"\|\||&&|[|;&\n]").unwrap())
}

fn quoted_span() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new("'[^']*'|\"[^\"]*\"").unwrap())
}

fn narrow_tail() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?:^|[|;&(]|\s)(?:/usr/bin/)?tail\s+(?:-1|-n\s*1)\s").unwrap())
}

fn append_ref() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(&format!(
            r">>\s*{}",
            r"(?:~|\$HOME|/Users/[^/\s]+)/scratch/parked/(?P<tail>\S*)"
        ))
        .unwrap()
    })
}

fn text_authoring() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"^\s*(?:git\s+(?:commit|log|show|tag|diff)|echo|printf|cat\s*<<|python3?\b)")
            .unwrap()
    })
}

const RECIPE: &str = "[park-ledger-scope-guard] BLOCKED: positional read of the park ledger.

The park ledger is a host-wide append-only queue. Every session on this box
appends to it, including sessions still running right now. Its tail is the
most recent parking across ALL threads, so recency is not relevance, and the
newest line is more likely to belong to a different live session than yours.

If this call also contained an append, that append did not run: this guard
is PreToolUse, so it kills the whole call before execution, not just the
offending command. A one-line tail of the ledger you appended to earlier in
the SAME call is allowed as the check of your own line (never a byte-range
tail, never first); anything wider is the survey this guard exists to stop.
Re-issue the append as its own call, then verify it with a one-line tail or
a grep for a distinctive phrase you just wrote.

What to do instead: you already hold a subject, so look it up by name.

    grep -n -iE '<subject keywords>' ~/scratch/parked/<Host>.md | cut -c1-3000

The subject comes from the reseed bundle's narrative.md (its last user turn
and any trailing question), or from what the operator just asked. NOT from
this file. A ledger line that does not match your subject is another
thread's work: its resume pointer is an address, not an assignment, even
when it is the newest line and even when the step looks ungated.

If you genuinely need the whole-host survey, it is still available. Re-run
with the marker, which makes the survey a deliberate act rather than a
reflex:

    tail -8 ~/scratch/parked/<Host>.md | cut -c1-400  # ledger-survey";

/// True when the command reads the park ledger by position, not by content.
pub fn is_positional_ledger_read(command: &str) -> bool {
    if survey_marker().is_match(command) {
        return false;
    }
    let stripped = quoted_span()
        .replace_all(&super::mask_heredocs(command), " ")
        .into_owned();
    if content_select().is_match(&stripped) {
        return false;
    }

    let mut appended: std::collections::HashSet<String> = std::collections::HashSet::new();
    for segment in segment_split().split(&stripped) {
        let reader = !text_authoring().is_match(segment) && positional_reader().is_match(segment);
        if reader {
            let own_check = narrow_tail().is_match(segment);
            for m in ledger_ref().captures_iter(segment) {
                let tail = m.name("tail").map(|t| t.as_str()).unwrap_or("");
                let whole = m.get(0).unwrap();
                let before = &segment[..whole.start()];
                if before.trim_end().ends_with('>') {
                    continue; // a redirect INTO the ledger is a park, not a read
                }
                if own_check && appended.contains(tail) {
                    continue;
                }
                if tail.is_empty() || tail == "/" || tail.ends_with(".md") {
                    return true;
                }
            }
        }
        for m in append_ref().captures_iter(segment) {
            if let Some(t) = m.name("tail") {
                appended.insert(t.as_str().to_string());
            }
        }
    }
    false
}

fn ssh_scp() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^\s*(?:ssh|scp)\b").unwrap())
}

pub fn run(p: Payload) -> i32 {
    let Some(command) = p.tool_input.command.filter(|c| !c.is_empty()) else {
        return 0;
    };
    if ssh_scp().is_match(&command) {
        return 0;
    }
    if is_positional_ledger_read(&command) {
        println!(
            "{}",
            serde_json::json!({
                "hookSpecificOutput": {
                    "hookEventName": "PreToolUse",
                    "permissionDecision": "deny",
                    "permissionDecisionReason": RECIPE,
                }
            })
        );
        return 0;
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cases() -> Vec<(bool, &'static str)> {
        vec![
            (true, "tail -8 ~/scratch/parked/bender.md | cut -c1-400"),
            (true, "sed -n '90,95p' ~/scratch/parked/bender.md"),
            (true, "cat $HOME/scratch/parked/bender.md"),
            (true, "head -20 ~/scratch/parked/Mark-Partly.md"),
            (
                true,
                "tail -8 ~/scratch/parked/bender.md | cut -c1-400 && echo done",
            ),
            (true, "less ~/scratch/parked/TS-Mac-Mli.md"),
            (
                false,
                "grep -n -iE 'minio|3020' ~/scratch/parked/bender.md | cut -c1-3000",
            ),
            (
                false,
                "tail -8 ~/scratch/parked/bender.md | cut -c1-400  # ledger-survey",
            ),
            (
                false,
                r#"echo "2026-08-27 | thing | resume: x" >> ~/scratch/parked/bender.md"#,
            ),
            (
                false,
                r#"git commit -m "note about ~/scratch/parked/bender.md tail""#,
            ),
            (
                false,
                "ssh partly 'tail -8 ~/scratch/parked/Mark-Partly.md'",
            ),
            (false, "wc -l ~/scratch/parked/bender.md"),
            (false, "ls -la ~/scratch/parked/"),
            (false, "tail -5 ~/scratch/notes.md"),
        ]
    }

    #[test]
    fn ledger_guard_case_table() {
        for (want, cmd) in cases() {
            assert_eq!(is_positional_ledger_read(cmd), want, "case: {cmd}");
        }
    }
}
