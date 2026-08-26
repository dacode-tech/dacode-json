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
        "rss_win": int(f[4]), "heap": int(f[5]), "chunks": int(f[6]),
        "allocs": int(f[7]), "abytes": int(f[8]),
        "note": f[9] if len(f) > 9 else "",
    })

if not rows:
    print("no results"); sys.exit(0)

base = next((r["rss"] for r in rows if r["impl"] == "baseline"), 0)
inp = rows[0]["input"]
print(f"\n=== memory: {sys.argv[2]}, input {human(inp)} ({inp} bytes) ===")
print(f"    baseline peak RSS {human(base)} (corpus generated + touched, nothing parsed)\n")

hdr = (f"{'implementation':<20} {'over base':>11} {'xJSON':>7} {'in-window':>11} "
       f"{'rust allocs':>12} {'rust bytes':>12}  note")
print(hdr)
print("-" * len(hdr))
work = [r for r in rows if r["impl"] != "baseline"]
for r in sorted(work, key=lambda r: (r["impl"].startswith("read_"), r["rss"])):
    over = r["rss"] - base
    ratio = over / inp if inp else 0
    # For read_* the absolute peak includes preparing the buffer, so show
    # a dash and let the in-window delta speak.
    over_s = "-" if r["impl"].startswith("read_") else human(over)
    ratio_s = "-" if r["impl"].startswith("read_") else f"{ratio:>6.2f}x"
    print(f"{r['impl']:<20} {over_s:>11} {ratio_s:>7} {human(r['rss_win']):>11} "
          f"{r['allocs']:>12} {human(r['abytes']):>12}  {r['note']}")
print("""
peak RSS    getrusage(RUSAGE_SELF).ru_maxrss for the whole process. Counts
            Rust, C, mmap and stacks alike -- the only metric that is
            comparable across the FFI boundary.
over base   peak RSS minus the `baseline` run, i.e. the cost of parsing.
            Shown as `-` for read_* rows: peak RSS is monotonic per process
            and those rows build their buffer before the measured window,
            so the absolute peak is not attributable to the read.
in-window   additional peak RSS caused by the measured work alone. This is
            the meaningful RSS figure for read_* rows.
rust *      GlobalAlloc counters. Exact for Rust implementations; zero for
            yyjson and simdjson by construction, since a C malloc never
            reaches Rust's allocator.

mstats()/mallinfo2() is deliberately NOT reported: `memprofile selftest`
shows it misses large allocations entirely on macOS (8 MiB Vec -> 0 B) and
does not observe frees. Run it yourself before trusting it anywhere.""")
PY
rm -f "$RAW"
