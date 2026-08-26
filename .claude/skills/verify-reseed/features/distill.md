# Distill

## Sub-features
- Writes the five-artefact bundle layout.
- Replaces tool payloads with `[tool#NNN name]` pointers and keeps the narrative verbatim.
- Prunes stale `calls/NNN.json` on redistill into an existing directory.
- Reports a real token reduction in `savings.md`.

## How to get to it (user POV)
`reseed distill <session-id-prefix>`, or a full path to a `.jsonl`. In daily use
it arrives through `! reseed-here` before a `/clear`.

## Driving it with the shell harness
    target/release/reseed distill "$TRANSCRIPT" --out "$SCRATCH/bundle"

Assert against the directory, not the exit code:

- `narrative.md`, `context-files.md`, `index.json`, `savings.md` and `calls/` all exist.
- `narrative.md` is non-empty and matches `\[tool#[0-9]{3} [A-Za-z]` at least once.
  Zero pointers on a real transcript over 50 KB means the parser stopped
  recognising the schema, which is the whole reason this skill exists.
- Every pointer number in `narrative.md` has a matching `calls/NNN.json`.
- Each `index.json` entry's `sha256` equals the first 16 hex chars of the
  sha256 of that call's `result` string, and `bytes` equals that string's
  **UTF-8 byte length**. Rust `String::len()` is bytes; a checker written in
  Python must encode first, or every non-ASCII result reads as a mismatch.
- `narrative.md` is at least 80% of the transcript's own user and assistant
  text bytes. This is the sharpest schema-drift check there is: a pointer
  count can stay non-zero while the parser silently drops most of the
  dialogue, and this catches that where the pointer check does not.
- `savings.md` shows distilled below full. A very high saving is normal and
  is not a defect signal: one measured session was 13.5 MB of tool payloads
  against 23 KB of dialogue, giving 99.8% against the README's typical 65%.

Redistill into the same `--out` after truncating the input, then assert the
higher-numbered `calls/NNN.json` are gone.

## Gotchas
- Bundles default to `~/.claude/reseed/<session-id>/`, which holds **live**
  reload bundles. A verification run always passes `--out`.
- A transcript under about 50 KB can distill to a near-empty narrative and still
  exit 0. Pick a large one.
- Session id prefix and a `.jsonl` path are both accepted by the same positional
  argument, so a typo'd prefix fails as "no match", not as a bad path.
