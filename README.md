# reseed

Distill a [Claude Code](https://claude.com/claude-code) session into a slim,
reloadable bundle — strip the tool-call noise into an addressable archive,
keep the narrative, and reseed a fresh context window without losing the
thread.

## Why

In a long Claude Code session, tool input/output dominates the transcript.
Measured across real sessions, `tool_use` + `tool_result` blocks are
**60–65% of the bytes**, while the actual user↔assistant narrative — the
decisions and reasoning you care about — is only **~12%**.

Claude Code's built-in options are `/compact` (a lossy LLM summary of the
whole history) and `/clear` (a full wipe). Neither lets you keep the
decisions verbatim while shedding the tool noise. `reseed` does exactly
that, and keeps the stripped tool calls in an archive you can fetch back
for audit or when the narrative misses a detail.

## What it produces

```
<out>/<session-id>/
  narrative.md      # the dialogue; tool calls become [tool#NNN name] pointers
  calls/NNN.json    # one archived tool call+result per file, addressable
  context-files.md  # files the session Read/Edited/Wrote
  index.json        # manifest: pointer → tool, bytes, sha256
  savings.md        # token estimate, full vs distilled
```

On a real 1,500-line session this distilled **165k → 57k tokens (65%)**.

## Usage

```sh
# Distill a session (id prefix, or a full path to a .jsonl)
reseed distill 5f84ae5c

# Fetch one archived tool call (defanged by default — see Safety)
reseed fetch 5f84ae5c 42

# Distill, then launch a fresh `claude` seeded to read the bundle
reseed launch 5f84ae5c
```

Sessions are auto-discovered under `~/.claude/projects/*/` by id prefix.
Bundles default to `~/.claude/reseed/<session-id>/`; override with `--out`.

### Reseeding

`reseed` writes the bundle; the reseed itself is one read in a fresh
session:

```
claude
> Read ~/.claude/reseed/<id>/narrative.md and context-files.md, then continue.
```

`reseed launch` does this for you.

## Safety — defang on read

A transcript records every tool result verbatim, including web pages and
MCP responses that may carry prompt-injection payloads. Reading those bytes
back into a fresh context window is a deferred-injection channel.

`reseed fetch` therefore **defangs by default**: alphabetic runs of 4+
characters are interleaved with U+00B7 MIDDLE DOT, so `ignore` becomes
`i·g·n·o·r·e` — defeating literal and regex pattern matching while staying
readable. Pass `--raw` to bypass (with a stderr warning) when you trust the
content and need exact bytes.

The narrative itself is kept raw: it is user/assistant text, the low-risk
slice. The high-risk raw tool output lives only in the archive, behind the
defanging fetch.

## Token estimate

`reseed` uses a `chars / 4` heuristic — no API key, no network. Absolute
numbers are approximate; the full-vs-distilled **percentage** is the stable
signal. For exact counts, Anthropic's `POST /v1/messages/count_tokens` is
the only accurate path (`tiktoken` is wrong for Claude).

## Install

```sh
cargo install --path .
```

## License

MIT
