#!/usr/bin/env bash
# Read-only preflight: is this checkout worth driving?
set -uo pipefail
REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../../.." && pwd)"
BIN="$REPO/target/release/reseed"
fail=0; warn=0

echo "repo: $REPO"

if [ -x "$BIN" ]; then
  built="$("$BIN" --version 2>/dev/null | awk '{print $NF}')"
  declared="$(awk -F'"' '/^version *=/{print $2; exit}' "$REPO/Cargo.toml")"
  if [ "$built" = "$declared" ]; then
    echo "OK    binary $built matches Cargo.toml"
  else
    echo "FAIL  binary $built != Cargo.toml $declared - run: cargo build --release"; fail=1
  fi
else
  echo "FAIL  no $BIN - run: cargo build --release"; fail=1
fi

big="$(find "$HOME/.claude/projects" -name '*.jsonl' -size +50k 2>/dev/null | head -1)"
if [ -n "$big" ]; then
  echo "OK    real transcripts >50k present ($(find "$HOME/.claude/projects" -name '*.jsonl' -size +50k 2>/dev/null | wc -l | tr -d ' ') found)"
else
  echo "FAIL  no transcript >50k under ~/.claude/projects - a small one proves nothing"; fail=1
fi

# CC's ! bang shell sources only ~/.zshenv, so ~/.cargo/bin is not on its PATH.
if zsh -fc 'source ~/.zshenv 2>/dev/null; command -v reseed' >/dev/null 2>&1; then
  echo "OK    reseed reachable from the bang shell"
else
  echo "WARN  reseed NOT on the bang-shell PATH - '! reseed-here' will fail."
  echo "      fix: ln -sf ~/.cargo/bin/reseed ~/.local/bin/reseed"; warn=1
fi

if [ "$fail" -eq 0 ]; then
  [ "$warn" -eq 0 ] && echo "doctor: pass" || echo "doctor: pass (with warnings)"
else
  echo "doctor: FAIL"
fi
exit "$fail"
