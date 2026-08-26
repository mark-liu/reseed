#!/usr/bin/env bash
# Drive the mapped reseed features against a REAL transcript, capture evidence,
# clean up the bundle it created. Evidence survives; the bundle does not.
set -uo pipefail
REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../../.." && pwd)"
BIN="$REPO/target/release/reseed"
FEAT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../features" && pwd)"
STAMP="$(date +%Y%m%d-%H%M%S)"
EV="$HOME/scratch/verify-reseed-$STAMP"
SCRATCH="$(mktemp -d "${TMPDIR:-/tmp}/verify-reseed.XXXXXX")"
mkdir -p "$EV"
LOG="$EV/results.txt"
pass=0; failed=0

say()  { echo "$*" | tee -a "$LOG" >/dev/null; echo "$*"; }
ok()   { pass=$((pass+1));   say "PASS  $*"; }
bad()  { failed=$((failed+1)); say "FAIL  $*"; }
skip() { say "SKIP  $*"; }

cleanup() { rm -rf "$SCRATCH"; }
trap cleanup EXIT

[ -x "$BIN" ] || { echo "no $BIN - run doctor.sh"; exit 1; }

# Pick the transcript: the named session, else the largest real one.
if [ "${1:-}" != "" ]; then
  T="$(find "$HOME/.claude/projects" -name "*$1*.jsonl" 2>/dev/null | head -1)"
else
  T="$(find "$HOME/.claude/projects" -name '*.jsonl' -size +50k 2>/dev/null \
       | xargs -I{} stat -f '%z %N' {} 2>/dev/null | sort -rn | head -1 | cut -d' ' -f2-)"
fi
[ -n "$T" ] && [ -f "$T" ] || { echo "no transcript found"; exit 1; }

say "# verify-reseed $STAMP"
say "binary:     $("$BIN" --version)"
say "transcript: $T ($(stat -f %z "$T") bytes)"
say "features:   $FEAT"
say ""

B="$SCRATCH/bundle"

# --- distill ---------------------------------------------------------------
if "$BIN" distill "$T" --out "$B" >"$SCRATCH/distill.out" 2>&1; then
  ok "distill exits 0"
else
  bad "distill exited non-zero: $(tail -1 "$SCRATCH/distill.out")"
fi

miss=""
for f in narrative.md context-files.md index.json savings.md; do
  [ -f "$B/$f" ] || miss="$miss $f"
done
[ -d "$B/calls" ] || miss="$miss calls/"
[ -z "$miss" ] && ok "bundle layout complete" || bad "bundle missing:$miss"

ptr=$(grep -oE '\[tool#[0-9]{3} [A-Za-z]' "$B/narrative.md" 2>/dev/null | wc -l | tr -d ' ')
if [ "${ptr:-0}" -gt 0 ]; then
  ok "narrative carries $ptr tool pointers"
else
  bad "narrative has ZERO [tool#NNN Name] pointers - parser no longer matches the transcript schema"
fi

python3 - "$B" "$T" >>"$LOG" 2>&1 <<'PY'
import hashlib, json, pathlib, re, sys
b = pathlib.Path(sys.argv[1]); out = []
idx = json.loads((b / "index.json").read_text())
nar = (b / "narrative.md").read_text()
ptrs = {int(n) for n in re.findall(r"\[tool#(\d{3}) ", nar)}
files = {int(p.stem) for p in (b / "calls").glob("*.json")}
orphan = sorted(ptrs - files)
out.append(("orphan pointers with no calls/NNN.json", not orphan, f"{orphan[:5]}"))
bad_sha = [e["n"] for e in idx
           if hashlib.sha256(json.loads((b / "calls" / f"{e['n']:03d}.json").read_text())["result"]
                             .encode()).hexdigest()[:16] != e["sha256"]]
out.append(("index.json sha256 agrees with archived result", not bad_sha, f"{bad_sha[:5]}"))
bad_len = [e["n"] for e in idx
           if len(json.loads((b / "calls" / f"{e['n']:03d}.json").read_text())["result"].encode()) != e["bytes"]]
out.append(("index.json bytes agrees with archived result", not bad_len, f"{bad_len[:5]}"))
# The sharpest schema-drift check: the narrative must carry at least as much
# dialogue as the transcript actually holds.
t = pathlib.Path(sys.argv[2]); text = 0
for line in t.open(errors="replace"):
    try:
        c = (json.loads(line).get("message") or {}).get("content")
    except Exception:
        continue
    if isinstance(c, str):
        text += len(c.encode())
    elif isinstance(c, list):
        for blk in c:
            if isinstance(blk, dict) and blk.get("type") == "text":
                text += len(blk.get("text", "").encode())
nar = (b / "narrative.md").stat().st_size
out.append((f"narrative keeps the dialogue ({nar:,}B vs {text:,}B of transcript text)",
            text == 0 or nar >= 0.8 * text, ""))

sav = (b / "savings.md").read_text()
nums = [int(x.replace(",", "")) for x in re.findall(r"([\d,]{3,})", sav)]
out.append(("savings.md shows a reduction", len(nums) >= 2 and min(nums) < max(nums), ""))
rc = 0
for name, good, detail in out:
    print(("PASS  " if good else "FAIL  ") + name + (f"  {detail}" if not good and detail else ""))
    rc |= 0 if good else 1
sys.exit(rc)
PY
if [ $? -eq 0 ]; then pass=$((pass+5)); else failed=$((failed+1)); fi
tail -5 "$LOG"

# redistill prune: half the transcript into the same --out
head -n "$(( $(wc -l < "$T") / 2 ))" "$T" > "$SCRATCH/half.jsonl"
before=$(ls "$B/calls" | wc -l | tr -d ' ')
"$BIN" distill "$SCRATCH/half.jsonl" --out "$B" >/dev/null 2>&1
after=$(ls "$B/calls" | wc -l | tr -d ' ')
if [ "$after" -lt "$before" ]; then
  ok "redistill pruned stale calls ($before -> $after)"
else
  bad "redistill left $after calls, expected fewer than $before"
fi
"$BIN" distill "$T" --out "$B" >/dev/null 2>&1   # restore the full bundle

# --- fetch -----------------------------------------------------------------
d_out="$("$BIN" fetch "$B" 1 2>&1)"
r_out="$("$BIN" fetch "$B" 1 --raw 2>&1)"
case "$d_out" in *·*) ok "fetch defangs by default";; *) bad "fetch default output has no U+00B7 interleave";; esac
case "$r_out" in *·*) bad "fetch --raw still defanged";; *) ok "fetch --raw bypasses the defang";; esac

# --- session discovery -----------------------------------------------------
if "$BIN" distill zzzzzzzzzz --out "$SCRATCH/none" >"$SCRATCH/nf.out" 2>&1; then
  bad "bogus prefix exited 0"
else
  ok "bogus prefix fails loudly"
fi
nmatch=$(find "$HOME/.claude/projects" -name '0*.jsonl' 2>/dev/null | wc -l | tr -d ' ')
if [ "$nmatch" -gt 1 ]; then
  if "$BIN" distill 0 --out "$SCRATCH/amb" >"$SCRATCH/amb.out" 2>&1; then
    bad "ambiguous prefix exited 0 instead of refusing"
  else
    ok "ambiguous prefix refuses ($nmatch candidates)"
  fi
else
  skip "ambiguity: only $nmatch session matches '0' on this machine"
fi

# --- launch and install path ----------------------------------------------
"$BIN" launch --help 2>&1 | grep -q -- '--out' \
  && ok "launch --help documents --out" || bad "launch --help lost --out"
command -v claude >/dev/null && ok "claude on PATH (launch can exec)" \
  || bad "claude not on PATH - launch would fail at exec"
zsh -fc 'source ~/.zshenv 2>/dev/null; command -v reseed' >/dev/null 2>&1 \
  && ok "reseed reachable from the bang shell" \
  || bad "reseed NOT on bang-shell PATH - fix: ln -sf ~/.cargo/bin/reseed ~/.local/bin/reseed"

# --- evidence --------------------------------------------------------------
cp "$B/savings.md" "$EV/savings.md" 2>/dev/null
{ echo; echo "transcript: $T"; echo "pass=$pass fail=$failed"; } >> "$LOG"
say ""
say "pass=$pass fail=$failed"
echo "evidence: $EV"
[ "$failed" -eq 0 ] || exit 1
