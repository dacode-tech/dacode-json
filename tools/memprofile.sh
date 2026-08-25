#!/usr/bin/env bash
# Memory profile: one process per implementation, one row per run.
#
# Peak RSS is monotonic for the life of a process, so measuring several
# implementations in one process would attribute the first one's high-water
# mark to all of them. Hence a fresh process each time.
#
# Usage: tools/memprofile.sh [corpus] [target-bytes]
#        tools/memprofile.sh records 10485760
set -uo pipefail
cd "$(dirname "$0")/.."

CORPUS="${1:-records}"
TARGET="${2:-1048576}"

cargo build --release --features "profiling cbench" --bin memprofile >/dev/null 2>&1 || {
  echo "build failed (needs a C/C++ compiler for the cbench feature)" >&2
  exit 1
}
BIN=./target/release/memprofile

RAW=$(mktemp)
for impl in $($BIN list); do
  # `|| true` so one failing implementation does not abort the matrix.
  $BIN "$impl" "$CORPUS" "$TARGET" >> "$RAW" 2>/dev/null || true
done

python3 - "$RAW" "$CORPUS" <<'PY'
import sys

def human(b):
    b = float(b); neg = b < 0; b = abs(b)
    for unit, div in (("GiB", 1<<30), ("MiB", 1<<20), ("KiB", 1<<10)):
        if b >= div:
            return f"{'-' if neg else ''}{b/div:.2f} {unit}"
    return f"{'-' if neg else ''}{b:.0f} B"

rows = []
for line in open(sys.argv[1]):
    f = line.rstrip("\n").split("\t")
    if len(f) < 8:
        continue
    rows.append({
        "impl": f[0], "input": int(f[2]), "rss": int(f[3]),
        "heap": int(f[4]), "chunks": int(f[5]), "allocs": int(f[6]),
        "abytes": int(f[7]), "note": f[8] if len(f) > 8 else "",
    })

if not rows:
    print("no results"); sys.exit(0)

base = next((r["rss"] for r in rows if r["impl"] == "baseline"), 0)
inp = rows[0]["input"]
print(f"\n=== memory: {sys.argv[2]}, input {human(inp)} ({inp} bytes) ===")
print(f"    baseline peak RSS {human(base)} (corpus generated + touched, nothing parsed)\n")

hdr = (f"{'implementation':<20} {'peak RSS':>11} {'over base':>11} {'xJSON':>7} "
       f"{'rust allocs':>12} {'rust bytes':>12}  note")
print(hdr)
print("-" * len(hdr))
work = [r for r in rows if r["impl"] != "baseline"]
for r in sorted(work, key=lambda r: r["rss"]):
    over = r["rss"] - base
    ratio = over / inp if inp else 0
    print(f"{r['impl']:<20} {human(r['rss']):>11} {human(over):>11} {ratio:>6.2f}x "
          f"{r['allocs']:>12} {human(r['abytes']):>12}  {r['note']}")
print("""
peak RSS    getrusage(RUSAGE_SELF).ru_maxrss for the whole process. Counts
            Rust, C, mmap and stacks alike -- the only metric that is
            comparable across the FFI boundary.
over base   peak RSS minus the `baseline` run, i.e. the cost of parsing.
rust *      GlobalAlloc counters. Exact for Rust implementations; zero for
            yyjson and simdjson by construction, since a C malloc never
            reaches Rust's allocator.

mstats()/mallinfo2() is deliberately NOT reported: `memprofile selftest`
shows it misses large allocations entirely on macOS (8 MiB Vec -> 0 B) and
does not observe frees. Run it yourself before trusting it anywhere.""")
PY
rm -f "$RAW"
