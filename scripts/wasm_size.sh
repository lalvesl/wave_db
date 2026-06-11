#!/usr/bin/env bash
# Track the size of the WaveDB wasm artifact across commits.
#
# Usage:
#   scripts/wasm_size.sh            # canonical: nix build .#wasm (wasm-bindgen + wasm-opt -Oz)
#   scripts/wasm_size.sh --cargo    # fast path: cargo only (pre-bindgen, no wasm-opt — upper bound)
#
# Every run appends one JSON line to
#   crates/wavedb-wasm/tests/track_size_of_wasm.jsonl
#   {"date":...,"git_rev":...,"mode":...,"raw_bytes":...,"gzip_bytes":...}
# and prints the delta against the previous entry of the same mode.
# Run it whenever a feature lands or a crate updates — the JSONL is the
# growth history.

set -euo pipefail
cd "$(dirname "$0")/.."

LOG="crates/wavedb-wasm/tests/track_size_of_wasm.jsonl"
MODE="nix"
[[ "${1:-}" == "--cargo" ]] && MODE="cargo"

if [[ "$MODE" == "nix" ]]; then
  nix build .#wasm -o result-wasm
  WASM=$(find -L result-wasm -name '*_bg.wasm' | head -n1)
  [[ -n "$WASM" ]] || { echo "no *_bg.wasm in result-wasm/" >&2; exit 1; }
else
  cargo build --target wasm32-unknown-unknown --profile wasm-release -p wavedb-wasm
  WASM="target/wasm32-unknown-unknown/wasm-release/wavedb_wasm.wasm"
fi

RAW=$(stat -c%s "$WASM")
GZ=$(gzip -9 -c "$WASM" | wc -c)
REV=$(git rev-parse --short HEAD 2>/dev/null || echo "unknown")
DATE=$(date -u +%Y-%m-%d)

# Previous raw size for this mode.  `nix run .#fmt` pretty-prints the
# JSONL through jq, so parse per-object (RS="}") instead of per-line —
# works on both compact and jq-formatted entries.
PREV=0
if [[ -f "$LOG" ]]; then
  PREV=$(awk -v m="$MODE" 'BEGIN { RS="}" }
    $0 ~ "\"mode\"[[:space:]]*:[[:space:]]*\"" m "\"" {
      if (match($0, /"raw_bytes"[[:space:]]*:[[:space:]]*[0-9]+/)) {
        v = substr($0, RSTART, RLENGTH)
        gsub(/[^0-9]/, "", v)
        raw = v
      }
    }
    END { print raw + 0 }' "$LOG")
fi

printf '{"date":"%s","git_rev":"%s","mode":"%s","raw_bytes":%s,"gzip_bytes":%s}\n' \
  "$DATE" "$REV" "$MODE" "$RAW" "$GZ" >> "$LOG"

human() { numfmt --to=iec --suffix=B "$1" 2>/dev/null || echo "${1}B"; }

echo "wasm ($MODE): raw $(human "$RAW")  gzip $(human "$GZ")  [$WASM]"
if [[ "$PREV" -gt 0 ]]; then
  DELTA=$((RAW - PREV))
  if [[ "$DELTA" -gt 0 ]]; then
    echo "grew by $(human "$DELTA") since last $MODE measurement"
  elif [[ "$DELTA" -lt 0 ]]; then
    echo "shrank by $(human "${DELTA#-}") since last $MODE measurement"
  else
    echo "unchanged since last $MODE measurement"
  fi
fi
