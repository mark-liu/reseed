//! Parse a Claude Code session transcript (JSONL) into an ordered stream
//! of items: narrative text, tool calls, and tool results.
//!
//! Each transcript line is a JSON object. Conversation lines carry a
//! `message` with a `role` and `content`; `content` is either a plain
//! string (typical for user turns) or an array of typed blocks
//! (`text`, `tool_use`, `tool_result`, `thinking`). Non-conversation
//! lines (snapshots, hook records, meta) have no usable `message` and are
//! skipped. Thinking blocks are intentionally dropped — they are not
//! replayed on a fresh session and carry no decision the narrative needs.

use anyhow::{Context, Result};
use serde::Deserialize;
use serde_json::Value;
use std::io::{BufRead, BufReader, Read};

#[derive(Deserialize)]
struct Line {
    message: Option<Message>,
    timestamp: Option<String>,
}

#[derive(Deserialize)]
struct Message {
    role: Option<String>,
    content: Option<Value>,
}

/// One ordered element of the transcript, in document order.
#[derive(Debug, Clone)]
pub enum Item {
    /// A user or assistant text block.
    Text {
        role: String,
        text: String,
    },
    /// An assistant tool invocation.
    ToolUse {
        id: String,
        name: String,
        input: Value,
        timestamp: Option<String>,
    },
    /// A tool result, keyed back to its `ToolUse` by `tool_use_id`.
    ToolResult {
        tool_use_id: String,
        content: Value,
    },
}

/// Parse a reader of JSONL into an ordered item stream. Malformed lines are
/// skipped rather than aborting the whole parse — a single corrupt line in
/// a long session should not lose the rest of the transcript.
pub fn parse_reader<R: Read>(reader: R) -> Result<Vec<Item>> {
    let mut items = Vec::new();
    for line in BufReader::new(reader).lines() {
        let line = line.context("reading transcript line")?;
        if line.trim().is_empty() {
            continue;
        }
        let Ok(parsed) = serde_json::from_str::<Line>(&line) else {
            continue;
        };
        let Some(message) = parsed.message else {
            continue;
        };
        let role = message.role.unwrap_or_default();
        match message.content {
            Some(Value::String(text)) if !text.trim().is_empty() => {
                items.push(Item::Text { role, text });
            }
            Some(Value::Array(blocks)) => {
                extract_blocks(&role, &blocks, parsed.timestamp.as_deref(), &mut items);
            }
            _ => {}
        }
    }
    Ok(items)
}

fn extract_blocks(role: &str, blocks: &[Value], ts: Option<&str>, items: &mut Vec<Item>) {
    for block in blocks {
        let Some(kind) = block.get("type").and_then(Value::as_str) else {
            continue;
        };
        match kind {
            "text" => {
                if let Some(text) = block.get("text").and_then(Value::as_str) {
                    if !text.trim().is_empty() {
                        items.push(Item::Text {
                            role: role.to_string(),
                            text: text.to_string(),
                        });
                    }
                }
            }
            "tool_use" => {
                let id = block
                    .get("id")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                let name = block
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown")
                    .to_string();
                let input = block.get("input").cloned().unwrap_or(Value::Null);
                items.push(Item::ToolUse {
                    id,
                    name,
                    input,
                    timestamp: ts.map(str::to_string),
                });
            }
            "tool_result" => {
                let tool_use_id = block
                    .get("tool_use_id")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                let content = block.get("content").cloned().unwrap_or(Value::Null);
                items.push(Item::ToolResult {
                    tool_use_id,
                    content,
                });
            }
            // "thinking" and any future block types are dropped.
            _ => {}
        }
    }
}

/// Flatten a `tool_result` / tool input content `Value` to a plain string.
/// Arrays of `{type:"text", text:...}` blocks are joined; strings pass
/// through; everything else is rendered as compact JSON.
pub fn content_to_string(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Array(items) => items
            .iter()
            .map(|b| {
                b.get("text")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .unwrap_or_else(|| b.to_string())
            })
            .collect::<Vec<_>>()
            .join("\n"),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_string_and_block_content() {
        let jsonl = concat!(
            r#"{"message":{"role":"user","content":"hello world"}}"#,
            "\n",
            r#"{"message":{"role":"assistant","content":[{"type":"text","text":"hi"},{"type":"thinking","thinking":"hmm"},{"type":"tool_use","id":"t1","name":"Read","input":{"file_path":"/a"}}]},"timestamp":"2026-01-01"}"#,
            "\n",
            r#"{"message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":"file body"}]}}"#,
            "\n",
            r#"{"type":"snapshot","snapshot":{}}"#,
        );
        let items = parse_reader(jsonl.as_bytes()).unwrap();
        // user text, assistant text, tool_use, tool_result — thinking + snapshot dropped.
        assert_eq!(items.len(), 4);
        assert!(matches!(&items[0], Item::Text { role, text } if role == "user" && text == "hello world"));
        assert!(matches!(&items[1], Item::Text { role, .. } if role == "assistant"));
        assert!(matches!(&items[2], Item::ToolUse { id, name, .. } if id == "t1" && name == "Read"));
        assert!(matches!(&items[3], Item::ToolResult { tool_use_id, .. } if tool_use_id == "t1"));
    }

    #[test]
    fn skips_malformed_lines() {
        let jsonl = concat!(
            "not json at all\n",
            r#"{"message":{"role":"user","content":"survives"}}"#,
        );
        let items = parse_reader(jsonl.as_bytes()).unwrap();
        assert_eq!(items.len(), 1);
    }

    #[test]
    fn content_to_string_handles_shapes() {
        assert_eq!(content_to_string(&Value::String("x".into())), "x");
        assert_eq!(content_to_string(&Value::Null), "");
        let arr = serde_json::json!([{"type":"text","text":"a"},{"type":"text","text":"b"}]);
        assert_eq!(content_to_string(&arr), "a\nb");
    }
}
