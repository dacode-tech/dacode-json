#!/usr/bin/env bash
# Run the C-baseline comparison with one process per implementation.
#
# Why: running all contenders inside one criterion process gave unstable
# numbers. `vela_faithful/int_array` measured 225 MiB/s in the full run and
# 560 MiB/s when the yyjson_insitu / yyjson_pool benchmarks were filtered
# out — a 2.5x swing with no change to the code under test. The cause is
# cross-benchmark interference (allocator state and page residency left
# behind by criterion's `iter_batched_ref` staging hundreds of megabytes of
# input copies), not anything about the parsers.
#
# One process per implementation removes the interference entirely. It is
# also the honest way to benchmark across a language boundary, since the C
# libraries bring their own allocator behaviour.
#
# Usage: tools/cbench.sh [group-regex]
#        tools/cbench.sh c_parse_10mb
set -uo pipefail
cd "$(dirname "$0")/.."

GROUP="${1:-c_parse_1mb}"
IMPLS=(
  vela_faithful
  vela_strict
  yyjson
  yyjson_insitu
  yyjson_pool
  simdjson_dom
  simdjson_ondemand
  serde_json
)

echo "building..." >&2
cargo bench --features cbench --bench cbaseline --no-run >/dev/null 2>&1 || {
  echo "build failed" >&2; exit 1;
}

OUT=$(mktemp)
for impl in "${IMPLS[@]}"; do
  # Anchor on the group and the exact implementation segment so
  # `yyjson` does not also match `yyjson_pool`.
  cargo bench --features cbench --bench cbaseline -- \
      "^${GROUP}/${impl}/" 2>/dev/null \
    | grep -E "^${GROUP}/|thrpt:" >> "$OUT"
done

python3 - "$OUT" "$GROUP" <<'PY'
import re, sys, collections
rows = collections.defaultdict(dict)
name = None
for line in open(sys.argv[1]):
    line = line.rstrip()
    m = re.match(r'^(\S+)/(\w+)/(\w+)\s*$', line)
    if m:
        name = (m.group(3), m.group(2))   # (corpus, impl)
        continue
    m = re.match(r'^\s+thrpt:\s+\[\S+ \S+ (\S+) (\S+) ', line)
    if m and name:
        rows[name[0]][name[1]] = f"{m.group(1)} {m.group(2)}"
        name = None

impls = ["vela_faithful","vela_strict","yyjson","yyjson_insitu",
         "yyjson_pool","simdjson_dom","simdjson_ondemand","serde_json"]
present = [i for i in impls if any(i in v for v in rows.values())]
if not rows:
    print("no results"); sys.exit(0)
w = max(len(c) for c in rows) + 2
print(f"\n=== {sys.argv[2]} (isolated processes) ===\n")
print("corpus".ljust(w) + "".join(i.ljust(20) for i in present))
for corpus, v in rows.items():
    print(corpus.ljust(w) + "".join(v.get(i, "-").ljust(20) for i in present))
PY
rm -f "$OUT"
