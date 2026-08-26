# Fetch

## Sub-features
- Prints one archived tool call by pointer number.
- Defangs the payload by default.
- `--raw` bypasses the defang.

## How to get to it (user POV)
`reseed fetch <session> <n>` after a distill, when the narrative's `[tool#NNN]`
pointer is not enough and the actual payload is needed.

## Driving it with the shell harness
    target/release/reseed fetch "$SCRATCH/bundle" 1
    target/release/reseed fetch "$SCRATCH/bundle" 1 --raw

Two assertions, and both are needed:

- Default output contains the U+00B7 interleave (`i·g·n·o·r·e` shape) for at
  least one word of 4 or more letters.
- `--raw` output of the same pointer does **not** contain U+00B7, and is longer
  in bytes than nothing.

A defang that silently no-ops exits 0 and prints plausible text. Only the
presence-and-absence pair catches it.

## Gotchas
- `fetch` accepts a bundle directory as well as a session prefix, which is what
  makes it drivable against a scratch bundle.
- Pointer numbers are 1-based and zero-padded in the filename (`001.json`) but
  passed unpadded (`1`).
- `--raw` is a re-injection risk by design. Never pipe its output anywhere that
  re-enters a model context.
