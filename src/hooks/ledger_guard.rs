//! PreToolUse hook: blocks positional reads of the park ledger. Ported from
//! `park-ledger-scope-guard.py`; P4 hand scanner for the heredoc mask, P12
//! texts (no internal citations).

use super::Payload;
use crate::msg;
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
offending command. Re-issue the append as its own call through the append
helper, which verifies the line landed itself. No tail check exists: a
positional read of the ledger is always the survey this guard stops. If you
must confirm a line, grep for a distinctive phrase you just wrote.

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

const WRITE_RECIPE: &str = "[park-ledger-scope-guard] BLOCKED: direct write to the park ledger.

The park ledger has exactly one write path, the sanctioned append helper. It
takes the per-host lock that the expiry rotation also holds. A raw redirect,
tee, cp, sed -i, an interpreter opening the file for write, or an edit tool
call does not take that lock, so it can race the rotation and silently drop
your line.

This call did not run. Re-issue it as its own call through the append helper
(line on stdin). If the ledger itself needs a manual repair, ask the operator
to run the command with a leading `!`.";

fn deny(reason: String) {
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
}

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

    for segment in segment_split().split(&stripped) {
        let reader = !text_authoring().is_match(segment) && positional_reader().is_match(segment);
        if reader {
            for m in ledger_ref().captures_iter(segment) {
                let tail = m.name("tail").map(|t| t.as_str()).unwrap_or("");
                let whole = m.get(0).unwrap();
                let before = &segment[..whole.start()];
                if before.trim_end().ends_with('>') {
                    continue; // a redirect INTO the ledger is a park, not a read
                }
                if tail.is_empty() || tail == "/" || tail.ends_with(".md") {
                    return true;
                }
            }
        }
    }
    false
}

fn ssh_scp() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^\s*(?:ssh|scp)\b").unwrap())
}

#[derive(Debug, PartialEq)]
enum Verdict {
    Pass,
    RawWrite,
    PositionalRead,
}

/// What `run` decides for one Bash command. A write is denied first and on any
/// host (ssh does not make a raw write safe); a remote read is the other box's.
fn verdict(command: &str, cwd: &str) -> Verdict {
    if super::ledger_write::is_raw_ledger_write(command, cwd) {
        Verdict::RawWrite
    } else if !ssh_scp().is_match(command) && is_positional_ledger_read(command) {
        Verdict::PositionalRead
    } else {
        Verdict::Pass
    }
}

pub fn run(p: Payload) -> i32 {
    let Some(command) = p.tool_input.command.filter(|c| !c.is_empty()) else {
        return 0;
    };
    match verdict(&command, p.cwd.as_deref().unwrap_or("")) {
        Verdict::RawWrite => deny(msg::text("ledger-raw-write", WRITE_RECIPE)),
        Verdict::PositionalRead => deny(msg::text("ledger-positional-read", RECIPE)),
        Verdict::Pass => {}
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    // Mirrors CASES in test_park_ledger_scope_guard.py: (blocked?, command),
    // asserted through `verdict`, the decision `run` acts on.
    fn cases() -> Vec<(bool, &'static str)> {
        vec![
            (true, "tail -8 ~/scratch/parked/host-a.md | cut -c1-400"),
            (true, "sed -n '90,95p' ~/scratch/parked/host-a.md"),
            (true, "cat $HOME/scratch/parked/host-a.md"),
            (true, "head -20 ~/scratch/parked/host-b.md"),
            (true, "tail -8 ~/scratch/parked/host-a.md | cut -c1-400 && echo done"),
            (true, "less ~/scratch/parked/host-c.md"),
            // The own-line `tail -1` exception is gone: the append helper verifies itself.
            (true, "tail -1 ~/scratch/parked/host-a.md"),
            (
                true,
                "cat >> ~/scratch/parked/host-a.md <<'PARK'\nhello\nPARK\ntail -1 ~/scratch/parked/host-a.md",
            ),
            (
                false,
                "grep -n -iE 'minio|3020' ~/scratch/parked/host-a.md | cut -c1-3000",
            ),
            (
                false,
                "tail -8 ~/scratch/parked/host-a.md | cut -c1-400  # ledger-survey",
            ),
            (true, r#"echo "2026-08-27 | thing | resume: x" >> ~/scratch/parked/host-a.md"#),
            (true, "cat >> ~/scratch/parked/host-a.md <<'EOF'\nline\nEOF"),
            (true, "ssh host-b 'echo x >> ~/scratch/parked/host-b.md'"),
            (
                false,
                "~/repos/claude-memory/scripts/park-append.sh <<'EOF'\n2026-10-03 | x\nEOF",
            ),
            (false, "echo 'x >> ~/scratch/parked/host-a.md'"),
            (
                false,
                r#"git commit -m "note about ~/scratch/parked/host-a.md tail""#,
            ),
            (false, "ssh host-b 'tail -8 ~/scratch/parked/host-b.md'"),
            (false, "wc -l ~/scratch/parked/host-a.md"),
            (false, "ls -la ~/scratch/parked/"),
            (false, "tail -5 ~/scratch/notes.md"),
            (
                false,
                r#"OUT=~/scratch/parked-audit-x; mkdir -p "$OUT"; python3 ~/scripts/parked-rotate.py --list > "$OUT/dry.txt""#,
            ),
            (
                false,
                "TMP=$(mktemp); python3 ~/scripts/parked-rotate.py > $TMP; wc -l $TMP; rm -f $TMP",
            ),
            (
                false,
                "H=$(scutil --get ComputerName); grep -n foo ~/scratch/parked/$H.md | tee ~/scratch/out-$H.txt",
            ),
            (
                false,
                r#"export X=1; grep -rn parked project-memory > "$HOME/scratch/hits-$X.txt""#,
            ),
            (
                true,
                r#"L=~/scratch/parked/$(scutil --get ComputerName).md; echo x >> "$L""#,
            ),
            (true, "D=~/scratch/parked; echo x >> $D/host-a.md"),
        ]
    }

    #[test]
    fn ledger_guard_case_table() {
        let _held = crate::testlock::env_lock();
        let tmp = tempfile::tempdir().unwrap();
        let prev = std::env::var_os("HOME");
        std::env::set_var("HOME", tmp.path());
        for (want, cmd) in cases() {
            assert_eq!(verdict(cmd, "") != Verdict::Pass, want, "case: {cmd}");
        }
        match prev {
            Some(h) => std::env::set_var("HOME", h),
            None => std::env::remove_var("HOME"),
        }
    }

    #[test]
    fn deny_kind_follows_the_shape() {
        let _held = crate::testlock::env_lock();
        assert_eq!(
            verdict("echo x >> ~/scratch/parked/host-a.md", ""),
            Verdict::RawWrite
        );
        assert_eq!(
            verdict("tail -1 ~/scratch/parked/host-a.md", ""),
            Verdict::PositionalRead
        );
    }
}
