#!/usr/bin/env bash
# Run a criterion benchmark group with **one process per benchmark ID**.
#
# Why this exists
# ---------------
# Running many allocation-heavy benchmarks in one criterion process gives
# unstable numbers. Two independent mechanisms, each worth ~2.5x:
#
#   * Inlining drift. With lto="fat" and codegen-units=1, a benchmark
#     function holding many closures lets the inliner spend its budget
#     unevenly; adding an unrelated call site moves an unrelated
#     measurement.
#   * Cross-benchmark interference. criterion's iter_batched_ref stages
#     many copies of a large input per batch, and the allocator and
#     page-residency state that leaves behind changes what the *next*
#     benchmark measures.
#
# The wrappers in each bench file address the first. This addresses the
# second, completely: nothing else has run in the process.
#
# Usage: tools/isolate.sh <bench> [id-regex] [--features F]
#        tools/isolate.sh parse '^parse_10mb/'
#        tools/isolate.sh cbaseline '^c_parse_10mb/' --features cbench
set -uo pipefail
cd "$(dirname "$0")/.."

BENCH="${1:?usage: tools/isolate.sh <bench> [id-regex] [--features F]}"
FILTER="${2:-.}"
# macOS ships bash 3.2, where "${arr[@]}" on an empty array trips `set -u`.
# Keep the extra cargo flags in a plain string instead of an array.
EXTRA="${3:-}${4:+ $4}"

echo "building $BENCH..." >&2
# shellcheck disable=SC2086
cargo bench --bench "$BENCH" $EXTRA --no-run >/dev/null 2>&1 || {
  echo "build failed" >&2; exit 1;
}

# Enumerate the IDs criterion knows about, then filter.
# shellcheck disable=SC2086
IDS=$(cargo bench --bench "$BENCH" $EXTRA -- --list 2>/dev/null \
      | sed -n 's/: benchmark$//p' | grep -E "$FILTER")

if [ -z "$IDS" ]; then
  echo "no benchmark IDs matched $FILTER" >&2
  exit 1
fi

N=$(echo "$IDS" | wc -l | tr -d ' ')
echo "running $N benchmark(s), one process each" >&2

RAW=$(mktemp)
i=0
while IFS= read -r id; do
  i=$((i + 1))
  printf '\r  [%d/%d] %-58s' "$i" "$N" "$id" >&2
  # Anchor the regex to the full ID so one ID cannot match another
  # (e.g. `yyjson` also matching `yyjson_pool`).
  esc=$(printf '%s' "$id" | sed 's/[][\.*^$(){}?+|/]/\\&/g')
  # shellcheck disable=SC2086
  cargo bench --bench "$BENCH" $EXTRA -- "^${esc}\$" 2>/dev/null \
    | grep -E "^${esc}|time:|thrpt:" >> "$RAW"
done <<< "$IDS"
printf '\r%-72s\r' "" >&2

python3 - "$RAW" "$BENCH" "$FILTER" <<'PY'
import re, sys

rows = []
cur = None
for line in open(sys.argv[1]):
    line = line.rstrip()
    m = re.match(r'^(\S+)\s*$', line)
    if m and '/' in m.group(1):
        cur = {"id": m.group(1)}
        continue
    m = re.match(r'^(\S+)\s+time:\s+\[\S+ \S+ (\S+ \S+)', line)
    if m and '/' in m.group(1):
        rows.append({"id": m.group(1), "time": m.group(2), "thrpt": None})
        cur = None
        continue
    m = re.match(r'^\s+time:\s+\[\S+ \S+ (\S+ \S+)', line)
    if m and cur is not None:
        cur["time"] = m.group(1)
        continue
    m = re.match(r'^\s+thrpt:\s+\[\S+ \S+ (\S+ \S+)', line)
    if m and cur is not None and "thrpt" not in cur:
        cur["thrpt"] = m.group(1)
        rows.append(cur)
        cur = None
if cur is not None and "time" in cur:
    cur.setdefault("thrpt", None)
    rows.append(cur)

if not rows:
    print("no results parsed"); sys.exit(0)

w = max(len(r["id"]) for r in rows) + 2
print(f"\n=== {sys.argv[2]}  {sys.argv[3]}  (one process per benchmark) ===\n")
for r in rows:
    t = r.get("time", "-")
    th = r.get("thrpt")
    print(f"{r['id']:<{w}} {t:>12}" + (f"   {th}" if th else ""))
print(f"\n{len(rows)} benchmark(s)")
PY
rm -f "$RAW"
