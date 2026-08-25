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

6. **Stage 2 is the real target.** simdjson spends 37–43% of its time in
   Stage 1 against this port's 13–25%, which means the port's Stage 2 is
   proportionally far more expensive. Optimising Stage 1 further has little
   headroom left; the DOM builder has a lot.

7. **Memory is a separate axis and tier 1 wins it outright** — 64 KiB to
   navigate a 10 MiB document, against 37.5 MiB for yyjson and 43.4 MiB for
   tier 3. See `docs/MEMORY.md`.
