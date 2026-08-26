# Launch and install path

## Sub-features
- `reseed launch` distills, then execs `claude` seeded to read the bundle.
- The installed binary is reachable from Claude Code's `! bang` shell.

## How to get to it (user POV)
`reseed launch <session>` as a one-step reset. In daily use the reset is
`rename-thread`, then `! reseed-here`, then `/clear`, then `go`.

## Driving it with the shell harness
**The launch half is only partially drivable.** Spawning `claude` from a
verification run starts a nested interactive session, so the harness drives the
distill half and asserts the exec target instead:

- Run `reseed launch --help` and assert it documents the same `session` and
  `--out` arguments as `distill`.
- Assert `command -v claude` resolves, since launch fails at exec otherwise.

The install path is fully drivable and matters more:

    zsh -fc 'source ~/.zshenv 2>/dev/null; command -v reseed'

That reproduces exactly what `! reseed-here` sees. It must print a path.

## Gotchas
- **`cargo install` alone is not enough.** CC's bang shell sources only
  `~/.zshenv`, which adds `~/.local/bin` and not `~/.cargo/bin`. The fix is
  `ln -sf ~/.cargo/bin/reseed ~/.local/bin/reseed`, and its absence presents as
  "binary not found" long after a successful install.
- Never let a verification run actually exec `claude`. A nested session inherits
  the terminal and is awkward to kill cleanly.
