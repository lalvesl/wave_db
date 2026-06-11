#!/usr/bin/env bash
# Track the size of the WaveDB wasm artifact across commits.
#
# Usage:
#   scripts/wasm_size.sh            # canonical: nix build .#wasm (wasm-bindgen + wasm-opt -Oz)
#   scripts/wasm_size.sh --cargo    # fast path: cargo only (pre-bindgen, no wasm-opt — upper bound)
#
# Every run appends one row to wasm-size.csv:
#   date,git_rev,mode,raw_bytes,gzip_bytes
# and prints the delta against the previous row of the same mode.
# Run it whenever a feature lands or a crate updates — the CSV is the
# growth history.

set -euo pipefail
cd "$(dirname "$0")/.."

CSV="wasm-size.csv"
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

[[ -f "$CSV" ]] || echo "date,git_rev,mode,raw_bytes,gzip_bytes" > "$CSV"

PREV=$(awk -F, -v m="$MODE" '$3 == m { raw=$4 } END { print raw+0 }' "$CSV")
echo "$DATE,$REV,$MODE,$RAW,$GZ" >> "$CSV"

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
