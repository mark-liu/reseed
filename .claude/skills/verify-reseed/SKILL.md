---
name: verify-reseed
description: "Prove the reseed CLI works against a real Claude Code transcript, not a synthetic one. Drives distill, fetch and session discovery, captures evidence, cleans up. Use before releasing, after a Claude Code transcript-format change, or whenever a bundle looks wrong. Triggers: /verify-reseed, \"prove reseed works\", \"did the distill actually work\"."
---

# verify-reseed

`cargo test` proves reseed against transcripts the test file writes itself
(`tests/integration.rs::transcript`). It cannot catch the failure that actually
matters: **Claude Code changes its `.jsonl` schema and reseed keeps building
green while producing an empty or broken narrative.** Nothing else in the repo
drives a real transcript.

This skill closes that gap. Everything below runs against a genuine session file
under `~/.claude/projects/`.

Surface is a short-lived CLI. There is no server, no port, and no shared state:
every run is isolated by `--out`.

## Launch

There is nothing to keep alive. Build once, then drive each run in its own
output directory.

```sh
cd ~/repos/reseed
cargo build --release          # target/release/reseed
```

Ready when `target/release/reseed --version` prints. Teardown is deleting the
scratch output directory; see Cleanup.

**Do not drive `~/.cargo/bin/reseed` for a verification run.** That is the
installed binary the live `reseed-here` workflow uses, and testing against it
means you are proving whatever was last installed rather than the working tree.

## Doctor

Read-only. Answers "is this checkout worth driving?".

```sh
scripts/doctor.sh
```

It asserts, in order:

1. `target/release/reseed` exists and its `--version` matches `Cargo.toml`.
2. At least one real transcript exists under `~/.claude/projects/*/*.jsonl`
   larger than 50 KB. A tiny transcript passes distill trivially and proves
   nothing.
3. `~/.local/bin/reseed` exists and resolves. This is not cosmetic: Claude
   Code's `! bang` prefix runs a non-interactive zsh that sources only
   `~/.zshenv`, so `~/.cargo/bin` is not on PATH and `! reseed-here` fails with
   "binary not found" even after a successful `cargo install`. A missing
   symlink here is a live workflow break, reported as a warning rather than a
   failure because it does not block the verification itself.

## Drive

```sh
scripts/verify.sh                 # picks the largest recent real transcript
scripts/verify.sh <session-id>    # or drive one you name
```

The harness runs the mapped features in [`features/`](features/) against a real
transcript, writing its bundle to a scratch directory it owns. Stable handles
throughout: subcommand names, the `[tool#NNN name]` pointer format, the
`index.json` schema, and exit codes. No line numbers, no byte offsets.

## Evidence

Written to `~/scratch/verify-reseed-<YYYYMMDD-HHMMSS>/`, and the path is printed
at the end of every run. It holds the transcript identity driven, each feature's
pass or fail with the assertion that decided it, and the `savings.md` the run
produced.

Proof standards for anything added here:

- **Drive the real path.** A synthetic transcript is what `cargo test` already
  does. If a check can pass against a hand-written fixture, it belongs in
  `tests/integration.rs`, not here.
- **Capture the action and the resulting state.** Not just "exit 0": the bundle
  layout, the narrative's pointer count, and the sha256 agreement between
  `index.json` and the `calls/NNN.json` bytes on disk.
- **Verify side effects.** Distill writes files and prunes stale ones. Check the
  directory, not the exit code.
- **`fetch --raw` is the one check that must not be trusted by name.** Assert
  the defanged form is absent from `--raw` output and present without it. A
  defang that silently no-ops still exits 0.

## Cleanup

`scripts/verify.sh` removes the scratch bundle directory it created, and only
that directory. It never touches `~/.claude/reseed/`, which holds live reload
bundles, and it never kills a process by name.

**Cleanup does not remove the evidence.** The evidence directory under
`~/scratch/` survives the run by design, and the run prints its path last.

## Helpers

Both are executable and take no required arguments.

- `scripts/doctor.sh` - the read-only preflight above.
- `scripts/verify.sh [session-id]` - the full drive, evidence capture and
  cleanup.

## Feature map

[`features/README.md`](features/README.md) is the index. A run that drives one
convenient entry point is incomplete when the map lists others.
