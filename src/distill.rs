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
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::fmt::Write as _;

/// Tools whose `file_path` / `notebook_path` argument names a file the
/// session read or wrote — used to build the "files touched" list.
const FILE_TOOLS: &[&str] = &["Read", "Edit", "Write", "MultiEdit", "NotebookEdit"];

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

    for item in items {
        match item {
            Item::Text { role, text } => {
                if role != &last_role {
                    writeln!(narrative, "\n**{role}:**\n").ok();
                    last_role = role.clone();
                }
                narrative.push_str(text.trim_end());
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
    let savings_md = render_savings(full_tokens, distilled_tokens);

    Bundle {
        narrative,
        calls,
        context_files,
        index_json,
        savings_md,
        full_tokens,
        distilled_tokens,
    }
}

/// Render the "files touched" markdown from the collected paths.
pub fn render_context_files(files: &[String]) -> String {
    let mut s = String::from(
        "# Files this session touched\n\n\
         Derived from Read / Edit / Write / NotebookEdit tool calls in the \
         transcript.\nPreload or re-open these as needed.\n\n",
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

fn render_savings(full: usize, distilled: usize) -> String {
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
    writeln!(s, "| Saved | {saved} ({pct:.1}%) |").ok();
    s.push_str("\n_Estimate is ~4 chars/token; the percentage is the stable signal._\n");
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
}
