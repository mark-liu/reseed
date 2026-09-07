//! PreToolUse hook: blocks parent-level listings of `~/.claude/reseed`.
//! Ported from `reseed-probe-guard.py`; P12 texts (no internal citations).

use super::Payload;
use crate::msg;
use regex::Regex;
use std::sync::OnceLock;

fn listing_verb() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?:^|[|;&]|\s)(?:/bin/)?(?:ls|find|du|tree)\b").unwrap())
}

fn reseed_ref() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?:~|\$HOME|/Users/[^/\s]+)/\.claude/reseed(?P<tail>\S*)").unwrap()
    })
}

fn reseed_glob_up() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?:~|\$HOME|/Users/[^/\s]+)/\.claude/\*\S*reseed").unwrap())
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
        Regex::new(r"^\s*(?:git\s+(?:commit|log|show|tag)|echo|printf|cat\s*<<)\b").unwrap()
    })
}

fn glob_tail() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^\*+/?$").unwrap())
}

const RECIPE: &str =
    "[reseed-probe-guard] BLOCKED: parent-level listing of the reseed state directory.

That directory holds 230+ session subdirs. Listing it is noise, and it is the
documented reflex on an empty-context restart. The glob forms that add a
wildcard to the directory name are matched too: the guard keys on the
directory, not the command shape, so reformulating the wildcard buys nothing.

An empty restart is NOT the exception that licenses the probe: the parent
listing cannot answer whether a bundle armed anyway. Only the newest bundle
directory's mtime plus the pending directory's mtime separate never-armed
from armed-and-consumed, and steps 1 and 3 below hand you both.

Run the recovery recipe instead, as the first action, not a fallback:

  cd ~/.claude/reseed && /bin/ls -1td */ | head -2

Then read the bundle's context-files.md FIRST (short, names every file the
session touched), and only then its narrative.md. Two cheap reads
reconstruct the thread.

In context-files.md, any path under a memory directory is not incidental:
the prior session's reflection pass wrote it minutes before the reset, so it
outranks both the narrative and any live lookup on questions of fact. Read
those rows before re-deriving anything.

If you are about to conclude nothing was armed, stat the directory before
saying so: an empty pending directory does not prove it, and an unlabelled
negative is a guess dressed as a finding. The honest line is that nothing
CURRENT was armed:

  stat -f '%Sm %SB' -t '%F %T' ~/.claude/reseed/pending

And a genuine no-arm is not a loss: a session reset never touches the
transcript:

  reseed distill <this-session-id>     # prefix ok; id = $CLAUDE_JOB_DIR basename

That rebuilds the full bundle from scratch, so never narrate lost context or
ask the operator what they were doing until that step has been tried.";

/// True when the command lists the reseed parent rather than descending into it.
///
/// Heredoc bodies are masked first (spec 3.4, deviation from the Python): the
/// segment split treats every body line as its own command, so writing a file
/// that quotes the blocked listing was blocked as if it were the listing.
pub fn is_parent_probe(command: &str) -> bool {
    let masked = super::mask_heredocs(command);
    let command = quoted_span().replace_all(&masked, " ");
    for segment in segment_split().split(&command) {
        if text_authoring().is_match(segment) {
            continue;
        }
        if !listing_verb().is_match(segment) {
            continue;
        }
        if reseed_glob_up().is_match(segment) {
            return true;
        }
        for m in reseed_ref().captures_iter(segment) {
            let tail = m.name("tail").map(|t| t.as_str()).unwrap_or("");
            if tail.is_empty() || tail == "/" {
                return true;
            }
            if glob_tail().is_match(tail) {
                return true;
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
    if is_parent_probe(&command) {
        eprintln!("{}", msg::text("probe-parent-listing", RECIPE));
        return 2;
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cases() -> Vec<(&'static str, bool)> {
        vec![
            ("/bin/ls -la ~/.claude/reseed/ 2>/dev/null | head -20", true),
            ("/bin/ls -la ~/.claude/reseed* ~/.claude/*reseed*", true),
            ("ls -la ~/.claude/reseed", true),
            ("ls $HOME/.claude/reseed/", true),
            ("find /Users/tester/.claude/reseed -maxdepth 1", true),
            ("du -sh ~/.claude/reseed", true),
            ("cd ~/.claude/reseed && /bin/ls -1td */ | head -3", false),
            ("/bin/ls -1td ~/.claude/reseed/*/ | head -2", false),
            ("ls -la ~/.claude/reseed/pending/", false),
            (
                "cat ~/.claude/reseed/e4336ad7-2487-477d-866c-21b507a01e9e/narrative.md",
                false,
            ),
            (
                "ls -la ~/.claude/reseed/e4336ad7-2487-477d-866c-21b507a01e9e/",
                false,
            ),
            ("stat -f '%Sm' -t '%F %T' ~/.claude/reseed/pending", false),
            ("~/.cargo/bin/reseed distill c4db92ee", false),
            (
                "reseed distill c4db92ee --out ~/.claude/reseed/c4db92ee",
                false,
            ),
            ("ls -la ~/scratch/", false),
            ("ssh host-b 'ls -la ~/.claude/reseed/'", false),
            (
                r#"git commit -q -m "blocks a parent listing of the reseed dir" -- scripts/x.py"#,
                false,
            ),
            (r#"echo "the recipe mentions the reseed directory""#, false),
            ("git log --oneline -1", false),
        ]
    }

    #[test]
    fn probe_guard_case_table() {
        for (cmd, expect_block) in cases() {
            assert_eq!(is_parent_probe(cmd), expect_block, "case: {cmd}");
        }
    }

    /// Spec 3.4, the one deliberate deviation from the Python: a heredoc BODY
    /// that quotes the listing is prose being written to a file, the same
    /// listing outside a heredoc is still the listing.
    #[test]
    fn heredoc_body_passes_but_the_bare_listing_still_blocks() {
        let authoring = "cat > guard-notes.md <<'EOF'\nls -la ~/.claude/reseed\nEOF";
        assert!(!is_parent_probe(authoring), "heredoc body must pass");

        let unquoted_word = "cat > guard-notes.md <<EOF\nls -la ~/.claude/reseed\nEOF";
        assert!(!is_parent_probe(unquoted_word), "unquoted <<WORD must pass");

        assert!(
            is_parent_probe("ls -la ~/.claude/reseed"),
            "the bare listing must still block"
        );
        assert!(
            is_parent_probe("cat > notes.md <<'EOF'\nprose\nEOF\nls -la ~/.claude/reseed"),
            "a listing AFTER a closed heredoc must still block"
        );
    }

    #[test]
    fn ssh_prefixed_command_bypasses_the_guard_at_run_level() {
        let p = Payload {
            tool_input: super::super::ToolInput {
                command: Some("ssh host-b 'ls -la ~/.claude/reseed/'".to_string()),
            },
            ..Default::default()
        };
        assert_eq!(run(p), 0);
    }
}
