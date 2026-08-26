# Session discovery

## Sub-features
- Resolves a session id prefix against `~/.claude/projects/*/`.
- Fails loudly on no match.
- Fails loudly on an ambiguous prefix rather than picking one.

## How to get to it (user POV)
Every subcommand's first positional argument. The user types four to eight
characters of a session id rather than a path.

## Driving it with the shell harness
    target/release/reseed distill zzzzzzzz --out "$SCRATCH/none"   # expect non-zero
    target/release/reseed distill "$SHORT_PREFIX" --out "$SCRATCH/amb"  # expect non-zero

- The no-match run exits non-zero and its stderr names the prefix.
- A one or two character prefix that matches several bundles exits non-zero and
  the message says ambiguous. Skip this assertion when the machine genuinely has
  only one matching session, and record the skip.

## Gotchas
- Discovery scans `~/.claude/projects/*/`, so it sees every project's sessions,
  not just the current directory's.
- Transcripts are local to the machine that ran the session. If you sync them
  between machines, the same prefix can resolve on one and not the other.
- An ambiguity failure is correct behaviour, not a bug. Do not "fix" it by
  taking the first match.
