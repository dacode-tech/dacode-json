#!/usr/bin/env bash
# Sampling profile of one workload, with symbols.
#
# `samply --save-only` writes a profile without symbolication (it normally
# happens in the Firefox Profiler UI via a symbol server), so
# tools/symbolicate.py maps the raw addresses back through `nm` on the
# binary. The release profile sets `debug = 1` for this.
#
# Usage: tools/profile.sh <workload> [iterations]
#        tools/profile.sh list
set -euo pipefail

cd "$(dirname "$0")/.."
W="${1:-list}"
N="${2:-200}"
BIN=./target/release/profile

cargo build --release --features "profiling cbench" --bin profile >/dev/null 2>&1

if [ "$W" = "list" ]; then exec "$BIN" list; fi

command -v samply >/dev/null || { echo "install samply: cargo install samply" >&2; exit 1; }
command -v rustfilt >/dev/null || echo "note: cargo install rustfilt for demangled names" >&2

OUT="/tmp/prof_${W}.json.gz"
samply record --save-only -o "$OUT" -- "$BIN" "$W" "$N" >/dev/null 2>&1
python3 tools/symbolicate.py "$W" "$BIN"
echo
echo "  open in a UI with: samply load $OUT"
