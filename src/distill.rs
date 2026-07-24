//! Turn an ordered transcript item stream into a reseed bundle:
//! a narrative with tool-call pointers, an addressable tool archive, the
//! list of files the session touched, a manifest, and a token-savings
//! report.
//!
//! This module is pure: it consumes parsed [`Item`]s and returns a
//! [`Bundle`] of in-memory strings/structs. All filesystem writes live in
//! the CLI layer, so the distillation logic is testable without touching
//! disk.

use crate::parse::{content_to_string, Item};
use crate::tokens;
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::fmt::Write as _;
use std::sync::LazyLock;

/// Tools whose `file_path` / `notebook_path` argument names a file the
/// session read or wrote — used to build the "files touched" list.
const FILE_TOOLS: &[&str] = &["Read", "Edit", "Write", "MultiEdit", "NotebookEdit"];

/// A `<local-command-stdout>` block larger than this is slash-command output
/// (e.g. a `/context` dump) — reproducible noise, dropped. Smaller blocks are
/// short command echoes and kept inline.
const STDOUT_DROP_BYTES: usize = 400;

/// Markers that, *when a text block opens with one*, mean the entire block is
/// harness-injected boilerplate the verbatim narrative supersedes:
/// - the `/compact` continuation-summary (a recompacted session embeds one per
///   compaction — we measured up to 32 copies in a single transcript)
/// - a `/context` telemetry paste (the giant MCP-tool usage table)
///
/// We require the marker to *open* the block (after leading whitespace) rather
/// than match anywhere: these injections always constitute the whole block, so
/// block-start anchoring keeps every real-saving case while making it
/// structurally impossible to truncate genuine dialogue that merely quotes the
/// marker mid-text (e.g. a session discussing transcript internals — like this
/// one).
const BLOCK_OPEN_MARKERS: &[&str] = &[
    "This session is being continued from a previous conversation",
    "# Context Usage",
    "## Context Usage",
    "### Context Usage",
];

/// Harness-injected spans removed wherever they appear in a block. These are
/// *wrapped* (paired open/close tag) and never occur in genuine prose, so
/// inline removal is safe — unlike bare `<command-*>` invocation tags, which a
/// session about Claude Code transcripts can legitimately quote, so those are
/// deliberately left intact. `(?s)` lets `.` span newlines; `.*?` keeps each
/// match minimal so adjacent blocks don't merge.
static INJECTED_SPANS: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    [
        r"(?s)<system-reminder>.*?</system-reminder>",
        r"(?s)<local-command-caveat>.*?</local-command-caveat>",
        r"(?s)Caveat: The messages below were generated.*?(?:\n\n|$)",
    ]
    .iter()
    .map(|p| Regex::new(p).unwrap())
    .collect()
});

/// `<local-command-stdout>` block, removed when large (see [`STDOUT_DROP_BYTES`]).
static COMMAND_STDOUT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?s)<local-command-stdout>.*?</local-command-stdout>").unwrap());

/// Strip harness-injected, non-dialogue content from one narrative text block:
/// `/compact` continuation-summaries, `/context` telemetry pastes, and
/// system-reminder / caveat injections.
///
/// These are dropped rather than archived (unlike tool results): the original
/// transcript is never mutated, so they remain recoverable from source, and a
/// recompacted session carries many redundant copies the verbatim narrative
/// already supersedes.
pub fn strip_harness(text: &str) -> String {
    // 1. Whole-block drop: if the block *opens* with a harness marker it is
    //    entirely boilerplate. Anchoring to block-start (not "anywhere") is
    //    what keeps quoted markers in real dialogue safe.
    let lead = text.trim_start();
    if BLOCK_OPEN_MARKERS.iter().any(|m| lead.starts_with(m)) {
        return String::new();
    }

    // 2. Remove wrapped injected spans anywhere in the block.
    let mut out = std::borrow::Cow::Borrowed(text);
    for re in INJECTED_SPANS.iter() {
        if let std::borrow::Cow::Owned(s) = re.replace_all(&out, "") {
            out = std::borrow::Cow::Owned(s);
        }
    }

    // 3. Drop large slash-command stdout (reproducible output, not dialogue);
    //    keep short echoes inline.
    COMMAND_STDOUT
        .replace_all(&out, |caps: &regex::Captures| {
            if caps[0].len() > STDOUT_DROP_BYTES {
                String::new()
            } else {
                caps[0].to_string()
            }
        })
        .into_owned()
}

/// One archived tool call paired with its result. Serialized to
/// `calls/NNN.json`.
#[derive(Debug, Serialize, Deserialize)]
pub struct Call {
    pub n: usize,
    pub tool_use_id: String,
    pub tool_name: String,
    pub input: Value,
    pub result: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timestamp: Option<String>,
    pub bytes: usize,
    pub sha256: String,
}

/// One row of the manifest written to `index.json`.
#[derive(Debug, Serialize)]
pub struct IndexEntry {
    pub n: usize,
    pub tool_use_id: String,
    pub tool_name: String,
    pub bytes: usize,
    pub sha256: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timestamp: Option<String>,
}

/// The complete in-memory bundle. The CLI serializes each field to a file.
pub struct Bundle {
    pub narrative: String,
    pub calls: Vec<Call>,
    pub context_files: Vec<String>,
    pub index_json: String,
    pub savings_md: String,
    pub full_tokens: usize,
    pub distilled_tokens: usize,
    /// Tokens removed from narrative text by [`strip_harness`] (compact
    /// summaries, /context pastes, command wrappers, harness injections).
    /// Reported separately so the second-order saving is visible.
    pub harness_stripped_tokens: usize,
}

/// Distill an ordered item stream into a bundle.
pub fn distill(items: &[Item], session: &str) -> Bundle {
    // Pass 1: collect tool results keyed by the tool_use_id they answer.
    let mut results: HashMap<String, String> = HashMap::new();
    for item in items {
        if let Item::ToolResult {
            tool_use_id,
            content,
        } = item
        {
            results.insert(tool_use_id.clone(), content_to_string(content));
        }
    }

    // Pass 2: walk in document order — number tool calls, build the
    // narrative with inline pointers, and pair each call with its result.
    // Seed capacity from a rough narrative-fraction of total content to
    // avoid repeated reallocation on long sessions.
    let mut narrative = String::with_capacity(8 * 1024);
    writeln!(narrative, "# Reseed narrative — session {session}\n").ok();
    narrative.push_str(
        "> Distilled Claude Code session. Tool calls are elided as \
         `[tool#NNN name]` pointers.\n> Fetch full input/output with \
         `reseed fetch <session> <NNN>` (defanged by default).\n\n---\n\n",
    );

    let mut calls = Vec::new();
    let mut context_files = Vec::new();
    let mut seen_files = std::collections::HashSet::new();
    let mut counter = 0usize;
    let mut last_role = String::new();
    let mut harness_stripped_tokens = 0usize;

    for item in items {
        match item {
            Item::Text { role, text } => {
                let cleaned = strip_harness(text);
                harness_stripped_tokens +=
                    tokens::estimate(text).saturating_sub(tokens::estimate(&cleaned));
                // Trim only the trailing edge: leading whitespace can be
                // significant (indented code, YAML, patches), so it is
                // preserved — matching the pre-strip behaviour. Emptiness is
                // judged on a fully-trimmed view so a block that was pure
                // harness boilerplate is skipped without orphaning a role
                // marker above empty content.
                let cleaned = cleaned.trim_end();
                if cleaned.trim_start().is_empty() {
                    continue;
                }
                if role != &last_role {
                    writeln!(narrative, "\n**{role}:**\n").ok();
                    last_role = role.clone();
                }
                narrative.push_str(cleaned);
                narrative.push_str("\n\n");
            }
            Item::ToolUse {
                id,
                name,
                input,
                timestamp,
            } => {
                counter += 1;
                writeln!(narrative, "→ [tool#{counter:03} {name}]\n").ok();

                if FILE_TOOLS.contains(&name.as_str()) {
                    if let Some(path) = file_path_of(input) {
                        if seen_files.insert(path.clone()) {
                            context_files.push(path);
                        }
                    }
                }

                // Move the result out of the map — each id is consumed
                // once, so removing frees the (potentially large) string.
                let result = results.remove(id).unwrap_or_default();
                let bytes = result.len();
                let sha256 = sha256_prefix(&result);
                calls.push(Call {
                    n: counter,
                    tool_use_id: id.clone(),
                    tool_name: name.clone(),
                    input: input.clone(),
                    result,
                    timestamp: timestamp.clone(),
                    bytes,
                    sha256,
                });
            }
            Item::ToolResult { .. } => {}
        }
    }

    let index: Vec<IndexEntry> = calls
        .iter()
        .map(|c| IndexEntry {
            n: c.n,
            tool_use_id: c.tool_use_id.clone(),
            tool_name: c.tool_name.clone(),
            bytes: c.bytes,
            sha256: c.sha256.clone(),
            timestamp: c.timestamp.clone(),
        })
        .collect();
    let index_json = serde_json::to_string_pretty(&index).unwrap_or_else(|_| "[]".into());

    // Token accounting (char/4): full = narrative text + all tool I/O;
    // distilled = what actually gets reloaded (narrative + file list).
    let full_tokens = full_token_estimate(items);
    let context_md = render_context_files(&context_files);
    let distilled_tokens = tokens::estimate(&narrative) + tokens::estimate(&context_md);
    let savings_md = render_savings(full_tokens, distilled_tokens, harness_stripped_tokens);

    Bundle {
        narrative,
        calls,
        context_files,
        index_json,
        savings_md,
        full_tokens,
        distilled_tokens,
        harness_stripped_tokens,
    }
}

/// Render the "files touched" markdown from the collected paths.
pub fn render_context_files(files: &[String]) -> String {
    let mut s = String::from(
        "# Files this session touched\n\n\
         Derived from Read / Edit / Write / NotebookEdit tool calls in the \
         transcript.\nPreload or re-open these as needed.\n\n\
         Paths are HISTORICAL: recorded at the moment of each tool call. A file \
         moved or renamed later in the session is listed at its old path too, so \
         two entries may share a basename. Confirm a path exists before passing it \
         to a command; when a basename repeats, the later entry is the current one.\n\n",
    );
    if files.is_empty() {
        s.push_str("_(none)_\n");
    } else {
        for f in files {
            writeln!(s, "- {f}").ok();
        }
    }
    s
}

fn render_savings(full: usize, distilled: usize, harness: usize) -> String {
    let saved = full.saturating_sub(distilled);
    let pct = if full > 0 {
        saved as f64 / full as f64 * 100.0
    } else {
        0.0
    };
    let mut s = String::from("# Token savings (char/4 estimate)\n\n");
    s.push_str("| Stream | ~tokens |\n|--------|--------:|\n");
    writeln!(s, "| Full session (narrative + tool I/O) | {full} |").ok();
    writeln!(s, "| Distilled (narrative + file list) | {distilled} |").ok();
    writeln!(
        s,
        "| ↳ harness boilerplate dropped from narrative | {harness} |"
    )
    .ok();
    writeln!(s, "| Saved | {saved} ({pct:.1}%) |").ok();
    s.push_str("\n_Estimate is ~4 chars/token; the percentage is the stable signal._\n");
    s.push_str(
        "_Harness boilerplate = `/compact` summaries, `/context` pastes, command \
         wrappers, system-reminder/caveat injections — recoverable from the original \
         transcript._\n",
    );
    s
}

fn full_token_estimate(items: &[Item]) -> usize {
    let mut total = 0;
    for item in items {
        total += match item {
            Item::Text { text, .. } => tokens::estimate(text),
            Item::ToolUse { input, .. } => tokens::estimate(&input.to_string()),
            Item::ToolResult { content, .. } => tokens::estimate(&content_to_string(content)),
        };
    }
    total
}

fn file_path_of(input: &Value) -> Option<String> {
    input
        .get("file_path")
        .or_else(|| input.get("notebook_path"))
        .and_then(Value::as_str)
        .map(str::to_string)
}

fn sha256_prefix(s: &str) -> String {
    let digest = Sha256::digest(s.as_bytes());
    // 16 hex chars = 64 bits — enough to correlate "same payload twice",
    // not enough to reconstruct.
    digest.iter().take(8).map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sample() -> Vec<Item> {
        vec![
            Item::Text {
                role: "user".into(),
                text: "check the file".into(),
            },
            Item::ToolUse {
                id: "t1".into(),
                name: "Read".into(),
                input: json!({"file_path": "/etc/hosts"}),
                timestamp: Some("2026-01-01".into()),
            },
            Item::ToolResult {
                tool_use_id: "t1".into(),
                content: json!("127.0.0.1 localhost"),
            },
            Item::Text {
                role: "assistant".into(),
                text: "done".into(),
            },
        ]
    }

    #[test]
    fn narrative_has_pointer_not_tool_output() {
        let b = distill(&sample(), "sess1");
        assert!(b.narrative.contains("[tool#001 Read]"));
        assert!(!b.narrative.contains("127.0.0.1"));
    }

    #[test]
    fn archive_pairs_call_with_result() {
        let b = distill(&sample(), "sess1");
        assert_eq!(b.calls.len(), 1);
        let c = &b.calls[0];
        assert_eq!(c.n, 1);
        assert_eq!(c.tool_name, "Read");
        assert_eq!(c.result, "127.0.0.1 localhost");
        assert_eq!(c.sha256.len(), 16);
    }

    #[test]
    fn context_files_collected_and_deduped() {
        let mut items = sample();
        // second Read of the same path should not duplicate.
        items.push(Item::ToolUse {
            id: "t2".into(),
            name: "Read".into(),
            input: json!({"file_path": "/etc/hosts"}),
            timestamp: None,
        });
        let b = distill(&items, "s");
        assert_eq!(b.context_files, vec!["/etc/hosts".to_string()]);
    }

    #[test]
    fn distilled_is_smaller_than_full() {
        // Make the tool result large so distillation clearly wins.
        let big = "x ".repeat(5000);
        let items = vec![
            Item::Text {
                role: "user".into(),
                text: "go".into(),
            },
            Item::ToolUse {
                id: "t1".into(),
                name: "Bash".into(),
                input: json!({"command": "ls"}),
                timestamp: None,
            },
            Item::ToolResult {
                tool_use_id: "t1".into(),
                content: json!(big),
            },
        ];
        let b = distill(&items, "s");
        assert!(b.distilled_tokens < b.full_tokens);
    }

    #[test]
    fn strip_drops_block_opening_with_compact_marker() {
        // Harness compact summaries open the block — whole block dropped.
        let text = "  This session is being continued from a previous \
                    conversation.\n\nSummary:\n1. Primary Request: secret stuff";
        assert!(strip_harness(text).trim().is_empty());
    }

    #[test]
    fn strip_drops_block_opening_with_context_usage() {
        let text = "## Context Usage\n\n| tool | tokens |\n|---|---|\n| x | 9 |";
        assert!(strip_harness(text).trim().is_empty());
    }

    #[test]
    fn strip_keeps_quoted_markers_mid_dialogue() {
        // codex regression: a session *discussing* these markers must not lose
        // the real instructions that follow them in the same block.
        let text = "When the transcript says 'This session is being continued \
                    from a previous conversation', strip it. Also drop a '## \
                    Context Usage' paste. Now: refactor the parser to handle EOF.";
        let out = strip_harness(text);
        assert!(out.contains("refactor the parser to handle EOF"));
        assert!(out.contains("This session is being continued"));
        assert!(out.contains("Context Usage"));
    }

    #[test]
    fn strip_keeps_literal_command_tags_in_prose() {
        // codex regression: bare invocation tags are not stripped, so a session
        // quoting them (or fenced code containing them) stays intact.
        let text = "The wrapper looks like <command-name>/prime</command-name> \
                    with <command-args>foo</command-args>.";
        assert_eq!(strip_harness(text), text);
    }

    #[test]
    fn strip_removes_system_reminder_and_caveat() {
        let text = "before <system-reminder>injected\nmultiline</system-reminder> after \
                    <local-command-caveat>Caveat: ...</local-command-caveat> end";
        let out = strip_harness(text);
        assert!(!out.contains("injected"));
        assert!(!out.contains("Caveat"));
        assert!(out.contains("before"));
        assert!(out.contains("after"));
        assert!(out.contains("end"));
    }

    #[test]
    fn strip_drops_large_command_stdout_keeps_small() {
        let big = "x".repeat(STDOUT_DROP_BYTES + 50);
        let text = format!(
            "a <local-command-stdout>{big}</local-command-stdout> b \
             <local-command-stdout>tiny</local-command-stdout> c"
        );
        let out = strip_harness(&text);
        assert!(!out.contains(&big));
        assert!(out.contains("tiny")); // small echo kept
        assert!(out.contains('a') && out.contains('b') && out.contains('c'));
    }

    #[test]
    fn strip_leaves_ordinary_dialogue_untouched() {
        let text = "Let's refactor the parser. Here's the plan:\n1. step one\n2. step two";
        assert_eq!(strip_harness(text), text);
    }

    #[test]
    fn distill_drops_compact_turn_without_orphan_marker() {
        let items = vec![
            Item::Text {
                role: "user".into(),
                text: "This session is being continued from a previous conversation. Summary: ..."
                    .into(),
            },
            Item::Text {
                role: "user".into(),
                text: "now the real ask".into(),
            },
        ];
        let b = distill(&items, "s");
        // The compact-only block contributes no text and no stray role marker
        // pile-up; the real ask survives.
        assert!(b.narrative.contains("now the real ask"));
        assert!(!b.narrative.contains("This session is being continued"));
        assert!(b.harness_stripped_tokens > 0);
    }

    #[test]
    fn distill_preserves_leading_indentation() {
        // codex regression: leading whitespace is significant for code/YAML and
        // must survive distillation (only the trailing edge is trimmed).
        let items = vec![Item::Text {
            role: "assistant".into(),
            text: "    indented code line\n        deeper\n".into(),
        }];
        let b = distill(&items, "s");
        assert!(b.narrative.contains("    indented code line"));
        assert!(b.narrative.contains("        deeper"));
    }
}
