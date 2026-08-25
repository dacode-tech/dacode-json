# Profiling

Two techniques are used here, and the cheap one produced the better result.

## Method

**Phase decomposition** — build cumulative variants of a pipeline and take
the deltas. No tooling, exact attribution, works on inlined code. This is
what found the Stage 1 bottleneck (§2), which a sampling profiler would have
shown as one undifferentiated blob because the whole scanner inlines into a
single function.

**Sampling** — `samply` + a symbolication step:

```bash
tools/profile.sh list              # available workloads
tools/profile.sh vela_de 200       # self-time table
samply load /tmp/prof_vela_de.json.gz   # flame graph in the browser
```

`samply --save-only` writes an unsymbolicated profile (symbolication
normally happens in the Firefox Profiler UI via a symbol server), so
`tools/symbolicate.py` maps raw addresses through `nm` on the binary and
demangles with `rustfilt`. The release profile sets `debug = 1` for this —
line tables only, no effect on codegen.

Workloads live in `src/bin/profile.rs`, one per parser and phase, all on the
same 1 MiB `records` corpus.

---

## 1. The classifier is not the bottleneck

`docs/stage2/P1_2_JSON_TIERS.md:91` specifies "branchless character
classification (lookup tables)" for Vela tier 3. It was never built. Having
built it (`src/scan/table.rs`), the specification turns out to have been
aimed at the wrong thing.

**Classifier in isolation**, 1 MiB, XOR-accumulated so there is no
dependency chain:

| | throughput |
|---|---|
| scalar `match` | 0.89 GiB/s |
| 256-entry LUT | 2.07 GiB/s |
| Vela's 8× `icmp` + 5× `or` | 4.90 GiB/s |
| nibble-shuffle table (`vqtbl1q`) | 5.82 GiB/s |
| **hybrid** (table for structural, compares for quote/backslash) | **6.47 GiB/s** |

The shuffle table is 19% faster than Vela's compares. So it should win
end-to-end. It does not:

| corpus | compare | shuffle table | delta |
|---|---|---|---|
| records | 1.159 | 1.111 GiB/s | **−4.1%** |
| int_array | 2.149 | 1.975 GiB/s | **−8.1%** |
| strings | 2.291 | 2.220 GiB/s | −3.1% |
| geo_int | 1.147 | 1.119 GiB/s | −2.4% |
| geo_float | 1.907 | 1.806 GiB/s | −5.3% |

**A 19% faster classifier made the scanner 2–8% slower.** The reason is the
critical path:

```text
quote mask ─→ real_quotes ─→ prefix_xor ─→ in_string ─→ output_mask
                    ↑
              escaped (from backslash mask)
```

`vceqq_u8` produces all-ones lanes that feed the movemask directly. A table
lookup produces a *bit pattern*, so it needs a `cmtst` first — one extra
instruction on the critical path, per chunk. The structural mask, which is
where the table saves eleven instructions, is consumed only at the very end
and is off the path entirely.

Hence the hybrid: compares for quote and backslash (latency), table for
structural (throughput). Same instruction count as the full table, 32%
faster in isolation, and **+0.1% to +3.4%** end-to-end.

That is a 3% win for a genuinely clever optimisation, which is the real
lesson.

## 2. …position extraction is

Cumulative phases, 1 MiB corpora, times per iteration:

| corpus | classify | + carry chain | + extract (full) |
|---|---|---|---|
| records | 131 µs | 184 µs | 1034 µs |
| int_array | 158 µs | 223 µs | 680 µs |
| strings | 251 µs | 353 µs | 991 µs |
| geo_int | 131 µs | 184 µs | 976 µs |
| geo_float | 131 µs | 184 µs | 583 µs |

As a share of Stage 1:

| | classify | carry chain | **extraction** |
|---|---|---|---|
| records | 12.7% | 5.1% | **82.2%** |
| int_array | 23.3% | 9.5% | **67.2%** |
| strings | 25.3% | 10.3% | **64.4%** |
| geo_int | 13.4% | 5.4% | **81.2%** |
| geo_float | 22.5% | 9.1% | **68.4%** |

Vela's `__simd_json_write_positions`
(`runtime/simd_json_branchless.ll:148-192`) loops once per set bit with a
loop-carried dependency on the mask and a branch each iteration. Replacing
it with simdjson's `bit_indexer::write` — write eight positions
unconditionally, then advance the length by `popcount` — gives:

| corpus | serial | unrolled | delta |
|---|---|---|---|
| records | 1.159 | 2.049 GiB/s | **+77%** |
| int_array | 2.113 | 3.246 GiB/s | **+54%** |
| strings | 2.318 | 2.140 GiB/s | −8% |
| geo_int | 1.164 | 2.005 GiB/s | **+72%** |
| geo_float | 1.935 | 2.509 GiB/s | **+30%** |

`strings` regresses because its structural density is 0.05/byte — writing
eight slots to record one or two positions wastes store bandwidth. Blocks of
four were tried and are worse everywhere (−11% records, −25% geo_float): the
extra branch costs more than the saved stores.

Net effect of §1 + §2: Stage 1 is now **2.4–3.8× Vela's reported 914 MB/s**.

---

## 3. Where each parser spends its time

Self time, 1 MiB `records`, 200 iterations.

### `vela_de` — deserialize into `Vec<Record>` (194 MiB/s)

| share | function |
|---|---|
| 25.2% | `strict::StrictParser::parse` |
| 11.5% | `profile::main` (the inlined serde walk) |
| 10.0% | `unescape::unescape` |
| 9.0% | `strict::State::gap` |
| 8.4% | `scan::StructuralIndex::write_bits` |
| **7.6%** | **`query::Doc::skip_subtree`** |
| 5.2% | `strict::State::scalar` |
| 3.6% | `scan::scan_into` |

Roughly 45% parse, 55% walk. Two things stand out.

`skip_subtree` at 7.6% is pure structural overhead: the pool stores
variable-width subtrees, so finding the next object key means walking the
previous value's entire subtree. A fixed-width child layout removes it
outright — this is one of the reasons `jsonflat` (`docs/ZEROCOPY.md`) is
built the way it is.

`unescape` at 10% is after adding an all-ASCII fast path that fuses the
backslash scan and the UTF-8 check into one pass. Before that it was two
passes per string field.

### `serde_json_de` — same output (293 MiB/s)

| share | function |
|---|---|
| **22.2%** | **`core::str::converts::from_utf8`** |
| 9.3% | `serde_json::de::from_trait` |
| 7.8% | `SliceRead::skip_to_escape` |
| 7.2% | `MapAccess::next_key_seed` |
| 6.0% | `SliceRead::parse_str` |
| 5.2% | `SeqAccess::next_element_seed` |
| 4.3% | `parse_integer` |

`serde_json` spends **over a fifth of its time on UTF-8 validation**. It is
a single-pass streaming deserializer with no intermediate representation,
which is the structural advantage that keeps it ahead of a
parse-then-walk design despite that.

### `vela_strict` — parse only, no serde (366 MiB/s)

| share | function |
|---|---|
| 51.3% | `StrictParser::parse` |
| 19.7% | `State::gap` |
| 15.5% | `StructuralIndex::write_bits` |
| 8.5% | `State::scalar` |
| 4.6% | `scan_into` |

`State::gap` at ~20% is the validating state machine's per-structural work.
This is the cost of conformance over the faithful parser.

### `vela_ser` — serialize `Vec<Record>` (717 MiB/s)

| share | function |
|---|---|
| 35.6% | `ser::write_escaped` |
| 26.2% | `Serializer::serialize_str` |
| 10.7% | `profile::main` |
| 2.3% | `Compound::finish` |

62% is string handling, which is why the escape scanner was worth
vectorising: escaping clean text went from 1.6 to 9.2 GiB/s, 4.2× ahead of
`serde_json`. The remaining gap to `sonic-rs` (25 GiB/s) is that these
corpus strings are 5–8 bytes, so the 16-byte SIMD loop never runs and
everything falls to the scalar tail.

### `flat_encode` — JSON → `jsonflat` (6.3 ms)

| share | function |
|---|---|
| 19.6% | `StrictParser::parse` |
| 10.9% | `flat::Writer::write_into` |
| 9.8% | `unescape::unescape` |
| 9.1% | `flat::Writer::put_str` |
| 6.6% | `write_bits` |
| 5.4% | `Doc::skip_subtree` |
| 3.2% | `SipHasher::write` (key interning) |

### `flat_read` — read every `score` from a `jsonflat` buffer (111 µs)

| share | function |
|---|---|
| 67.3% | `profile::main` (the inlined sum loop) |
| 5.2% | `flat::Builder::build` (setup, outside the loop) |
| 0.4% | everything else in the library |

Nothing left to optimise: the reader has been inlined into the caller's loop
and the remaining cost is the loop itself.

### The C libraries

Both are compiled into the profiling binary by `build.rs` and statically
linked, so `nm` resolves their symbols and the same tooling works.

**yyjson** — `tools/profile.sh yyjson`

| share | function |
|---|---|
| **93.6%** | `yyjson_read_opts` |
| 5.3% | libsystem_platform (memcpy/memset) |

One function, everything inlined into it. There is no phase structure to
attribute — yyjson is a single tight recursive-descent loop writing 16-byte
values into a pool. That is the design tier 3 copied, and the profile shows
what "copied well" would look like.

**simdjson DOM** — `tools/profile.sh simdjson_dom`

| share | function |
|---|---|
| 48.5% | `arm64::dom_parser_implementation::stage2` |
| 37.1% | `arm64::dom_parser_implementation::stage1` |
| 13.4% | outlined helpers |

**simdjson On Demand** — `tools/profile.sh simdjson_ondemand`

| share | function |
|---|---|
| 52.2% | the field-summing shim (On Demand value extraction, inlined) |
| 43.3% | `arm64::dom_parser_implementation::stage1` |

### The most useful comparison in this document

simdjson spends **37–43% of its time in Stage 1**. This port spends
**13–25%** (§2). Same two-stage architecture, very different balance.

That is not a compliment to the port — it means the port's Stage 2 is
proportionally *more* expensive than simdjson's, and by a wide margin.
simdjson's Stage 1 is doing more work per byte (it validates UTF-8 and the
document structure, which this port's faithful mode does not) and still
takes a larger share, because its Stage 2 is so much cheaper.

Combined with §2's finding that position extraction dominates Stage 1, the
ordering of work is:

1. Stage 2 / DOM building — the largest absolute cost, and the largest gap
   to yyjson and simdjson.
2. Position extraction inside Stage 1 — 64–82% of Stage 1, already improved
   30–77% here.
3. Classification — 13–25% of Stage 1. The thing the design docs asked for.

---

## 3b. Stage 2 measured directly, and one failed optimisation

§3 inferred that Stage 2 must be the expensive half. `benches/stage2.rs`
measures it, with the structural index pre-built and reused so only the
builder is timed (1 MiB corpora):

| corpus | Stage 1 | Stage 2 | Stage 2 share of total |
|---|---|---|---|
| records | 2.03 GiB/s | 1.18 GiB/s | **63%** |
| int_array | 3.17 GiB/s | 678 MiB/s | **82%** |
| strings | 2.16 GiB/s | 15.8 GiB/s | 12% |
| geo_int | 1.99 GiB/s | 573 MiB/s | **78%** |
| geo_float | 2.45 GiB/s | 867 MiB/s | **74%** |

Confirmed: Stage 2 is 63–82% of parse time on everything except `strings`,
where structural density is low and few nodes are produced.

Within Stage 2 (`tools/profile.sh vela_stage2`):

| share | function |
|---|---|
| 77.9% | `builder::build_from_index` — the walk loop |
| 21.7% | `scalar::parse_scalar_fast` — number and literal parsing |

### The optimisation that did not work

The walk loop looked obviously wasteful: `byte_at(i)` internally calls
`pos_at(i)`, so each iteration appeared to bounds-check `positions[i]`
twice, and every lookahead re-read what the next iteration would read
again. Restructuring to load `(pos, byte)` once and carry the lookahead
forward should have removed roughly half the loads.

It made things **slower** on every corpus:

| corpus | before | after | |
|---|---|---|---|
| records | 1.1845 GiB/s | 1.0939 GiB/s | −7.6% |
| int_array | 678.3 MiB/s | 655.5 MiB/s | −3.4% |
| strings | 15.80 GiB/s | 14.52 GiB/s | −8.1% |
| geo_int | 572.9 MiB/s | 549.4 MiB/s | −4.1% |
| geo_float | 867.4 MiB/s | 857.1 MiB/s | −1.2% |

LLVM was already eliminating the duplicate bounds checks — the two
`positions.get(i)` calls have the same index and no intervening write, so
CSE handles them. What the rewrite added was an `Option<(usize, u8)>`
carried across the loop back-edge, which cost more than the (already
absent) checks saved. Reverted.

The lesson is the same one §1 taught about the classifier: reasoning about
instruction counts predicts optimisation outcomes badly, and the only
reliable move is to measure the change.

### The proposed rewrite, measured before writing it

§3b ended by proposing a context-specialised walk loop — separate inner
loops for object bodies and array bodies, so branches become predictable —
on the theory that the generic `match ch` dispatch was mispredicting.

Two measurements killed that idea before any of it was written.

**First, the arithmetic.** `records` Stage 2 is 417 000 structurals in
836 µs: about **7 cycles per iteration** at 3.5 GHz. A single branch
mispredict costs ~15 cycles. The loop cannot be mispredict-dominated; the
branch predictor is already doing well.

**Second, the floor.** `benches/stage2.rs::bench_floor` walks the index
exactly as the builder does — load the position, load `input[pos]`,
dispatch — and then does nothing. That bounds what *any* index-fed Stage 2
can reach.

| corpus | index only | + load & dispatch (floor) | real builder | headroom |
|---|---|---|---|---|
| records | 29.9 GiB/s | 1.74 GiB/s | 1.23 GiB/s | **1.4×** |
| int_array | 57.2 GiB/s | 4.99 GiB/s | 684 MiB/s | 7.3× |
| strings | 224 GiB/s | 16.0 GiB/s | 15.8 GiB/s | **1.01×** |
| geo_int | 27.6 GiB/s | 1.76 GiB/s | 574 MiB/s | 3.1× |
| geo_float | 54.5 GiB/s | 3.54 GiB/s | 878 MiB/s | 4.0× |

On `records` the floor is only **1.4× above the real builder**, and that
floor already includes the dispatch. So a perfect rewrite of the dispatch
buys at most 41% there, and nothing at all on `strings`. The proposal was
aimed at the wrong thing.

### Where the cost actually is

The table says it plainly. Reading the index is free — 28–224 GiB/s. Adding
one load of `input[pos]` collapses it to 1.7–16 GiB/s, a **17× drop on
`records`**. That data-dependent load into the document, not the branch, is
what the walk loop spends its time on.

And where there *is* large headroom — `int_array` at 7.3×, `geo_float` at
4.0× — the gap is below the floor line, i.e. in work the floor benchmark
skips: `parse_scalar_fast` and the node writes. Those corpora are
number-dense, which matches `parse_scalar_fast` being 21.7% of Stage 2
overall and much more than that on numeric data.

So the two real targets, in order:

1. **Number parsing.** One `acc * 10 + digit` per byte, called once per
   numeric node. Explains the `int_array` / `geo_int` / `geo_float` gaps
   entirely. A SWAR or SIMD digit parser is the standard fix.
2. **Eliminate the `input[pos]` load.** Stage 1 *already knows* which
   character it matched — it came from a specific classification mask — but
   throws that away and stores only the offset, forcing Stage 2 to go back
   to the document for it. Packing a 3-bit class alongside a 29-bit
   position keeps the index at 4 bytes per entry and removes the load
   entirely for dispatch. The `index only` column suggests most of the 17×
   is recoverable.

Neither is the rewrite proposed in §3b. Both are smaller.

### Target 2 was implemented, measured, and reverted

Packing a 3-bit class into each index entry was implemented in full: Stage 1
classifies each structural byte while extracting it (an L1-hot load, since
the chunk was just streamed) and Stage 2 dispatches on the packed class,
never touching the document. All 201 tests passed, so the semantics were
right. The performance was not:

| corpus | Stage 1 before | after | Stage 2 before | after |
|---|---|---|---|---|
| records | 2.03 GiB/s | **834 MiB/s** | 1.18 GiB/s | 1.23 GiB/s |
| int_array | 3.17 GiB/s | 2.52 GiB/s | 678 MiB/s | 675 MiB/s |
| strings | 2.16 GiB/s | 1.94 GiB/s | 15.8 GiB/s | 17.7 GiB/s |
| geo_int | 1.99 GiB/s | **839 MiB/s** | 573 MiB/s | 554 MiB/s |
| geo_float | 2.45 GiB/s | 1.69 GiB/s | 867 MiB/s | 852 MiB/s |

Stage 1 lost 10–59%. Stage 2 gained **4% at best and regressed on three of
five corpora** — against a floor benchmark that predicted the dispatch would
go from 1.74 GiB/s to 30 GiB/s. Reverted.

### Why the floor benchmark was misleading

This is the more valuable result than the optimisation would have been.

`walk_dispatch` runs at 1.74 GiB/s on `records`: 417 000 scattered loads in
596 µs, about 5 cycles each — L2 latency, pipelined. `positions_only`, with
no load at all, runs at 30 GiB/s. The difference looked like 596 µs of pure
load cost sitting inside a builder that takes 845 µs.

It was not. In the real builder those loads are **hidden by
out-of-order execution** — there is enough independent work per iteration
(pool writes, stack updates, scalar parsing) that the load latency overlaps
with it and costs almost nothing. `walk_dispatch` measures the loads with
*nothing to hide them behind*, so it reports their latency rather than their
cost.

**An isolated microbenchmark measures an operation's latency; the real loop
pays its throughput cost, which out-of-order execution can hide entirely.**
The floor number was a valid upper bound on the dispatch loop in isolation
and a bad predictor of the gain from removing it.

That is now three optimisations in this project predicted by local reasoning
or isolated measurement and refuted by end-to-end measurement: the
lookup-table classifier (§1), the walk-loop restructure (§3b), and this one.
The only two that worked — the unrolled bit extractor and the hybrid
classifier — were also the only two proposed *after* a phase decomposition
of the full pipeline rather than a microbenchmark of a part.

### The two-pass hypothesis, tested and rejected

The remaining explanation for the 1.36x gap to yyjson was architectural:
yyjson reads the document once, the indexed path reads it twice (Stage 1
sequentially, Stage 2 at scattered structural offsets). For a two-stage
parser to match a single-pass one, *each* stage must run at roughly twice
its throughput — and Stage 2 manages only 0.57–1.2 GiB/s.

Vela already had the counter-design: `tier3/parse_onepass.vl`, task D1, a
single-pass byte state machine with no index. It is now ported
(`src/onepass.rs`) and produces a **byte-identical pool** — verified
against the indexed builder on every corpus, 20 000 random documents and
Vela's own `t848` shapes (`tests/onepass.rs`).

It is slower on every corpus:

| corpus | indexed (Stage 1 + 2) | single-pass | |
|---|---|---|---|
| records | 780 MiB/s | 666 MiB/s | −15% |
| int_array | 562 MiB/s | 425 MiB/s | −24% |
| strings | **1.85 GiB/s** | 978 MiB/s | **−47%** |
| geo_int | 450 MiB/s | 379 MiB/s | −16% |
| geo_float | 655 MiB/s | 526 MiB/s | −20% |

**Reading the document twice is cheaper than reading it once badly.** Stage
1 is a vector scan at 2.0–3.2 GiB/s; it finds all the structure in one
pass over sequential memory with 16 bytes per instruction. A single-pass
parser has to find that same structure a byte at a time, interleaved with
building the DOM. The second pass costs less than the vectorisation saves.

The gap is widest on `strings` (−47%), which is exactly where a vector scan
should dominate a byte loop — long runs with nothing structural in them.

**Caveat.** This port's `skip_ws` and `skip_string` are scalar byte loops.
Vela's runtime versions (`json_pool_write.ll:460`, `:526`) are SIMD, so a
fully vectorised single-pass parser would land somewhere above 666 MiB/s.
It is unlikely to reach 780: it would still be doing Stage 1's work without
Stage 1's advantage of scanning uninterrupted by DOM construction.

So the two-stage architecture is **not** the source of the yyjson gap.
simdjson uses the same architecture and beats yyjson with it. Whatever
yyjson's advantage is, it is in the quality of its single tight loop, not
in the pass count.

### What is actually left

`parse_scalar_fast` at 21.7% is the clearest remaining target, and it
correlates with corpus shape — per-node throughput is 122M/s on `geo_float`
and 296M/s on `strings`, tracking how many nodes need a number parsed. Its
inner loop is one `acc * 10 + digit` per byte.

The walk loop at 77.9% is harder. It runs once per *structural character*,
not per node — 417 000 iterations to produce 173 000 nodes on `records` —
and dispatches on a data-dependent byte each time. Making that cheaper
means exploiting context: inside an object the sequence is
`" … " : value , " … "`, which a specialised inner loop could walk with
predictable branches instead of a general match. That is how yyjson's
recursive descent stays ahead, and it is a rewrite rather than a tweak.

---

## 4. What this suggests for Vela

1. **Fix the extraction loop first.** 64–82% of Stage 1, and simdjson's
   technique is ~20 lines. Worth 30–77%.
2. **Do not add the lookup-table classifier the design docs specify** — at
   least not on ARM64, and not for the quote/backslash masks. Measure the
   hybrid split instead; it is worth ~3%.
3. **Lay container children out at fixed width.** Removes `skip_subtree`
   (7.6% of struct deserialization) and makes array indexing O(1).
4. **Escaping is 62% of serialization.** `emit_v2.vl` is documented as an
   O(n) emitter but calls `json_escape_string` (`common.vl:52`), which is a
   per-byte string-concat loop — so string output is still quadratic. This
   is the single largest correctness-shaped performance bug found.
5. **For struct deserialization, a DOM is a structural disadvantage.**
   `serde_json` never materialises one and stays ahead of the pool despite
   spending 22% of its time on UTF-8 validation.

6. **Stage 2 is the real target, but not for the reasons first supposed.**
   simdjson spends 37–43% of its time in Stage 1 against this port's
   13–25%. Three attempts to close the gap failed (§3b) and the two-pass
   architecture was tested and exonerated. What remains untried and
   well-supported is number parsing: `parse_scalar_fast` is 21.7% of
   Stage 2, and the corpora with the most headroom against the floor —
   `int_array` 7.3x, `geo_float` 4.0x — are precisely the number-dense
   ones.

7. **Memory is a separate axis and tier 1 wins it outright** — 64 KiB to
   navigate a 10 MiB document, against 37.5 MiB for yyjson and 43.4 MiB for
   tier 3. See `docs/MEMORY.md`.
