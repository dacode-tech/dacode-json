# The port vs yyjson and simdjson

Vela's `AUTHORS` file credits **yyjson** for tier 3 ("flat 16-byte value
nodes in a pre-allocated pool") and **simdjson** for tier 2's structural
indexing. Everything measured until now was against *Rust* libraries. This
is the comparison against the originals.

Both are vendored under `vendor/` and built from source at `-O3` by
`build.rs`, so the project has no dependency on the Vela tree.

* yyjson **0.12.0** (MIT), `vendor/yyjson/`
* simdjson **4.6.1** (Apache-2.0), `vendor/simdjson/`, `arm64` kernel
* Apple M2 Max, `rustc 1.95.0`, `lto = "fat"`, `codegen-units = 1`

Reproduce with `tools/cbench.sh c_parse_10mb`.

---

## 1. The answer

**Vela's own measurement**, `docs/stage2/JSON_IMPROVEMENT_PLAN.md:52-73`,
10 MB payload:

| | throughput | vs Vela tier 3 |
|---|---|---|
| simdjson | 860 MB/s | 5.2× |
| yyjson | 711 MB/s | **4.3×** |
| Vela tier 3 (`json_pool_parse_ws`) | 166 MB/s | 1.0 |

**The Rust port**, same algorithm, 10 MB `records`:

| | throughput | vs the port |
|---|---|---|
| `vela_faithful` (the port) | **754 MiB/s** | 1.0 |
| yyjson | 1022 MiB/s | 1.36× |
| yyjson + pooled allocator | 1.28 GiB/s | 1.74× |
| simdjson DOM | 1.10 GiB/s | 1.49× |
| simdjson On Demand | 1.91 GiB/s | 2.59× |

**Porting closed most of the gap: yyjson went from 4.3× ahead of Vela's
tier 3 to 1.36× ahead of the same algorithm in Rust.** The remaining
difference is real but modest, and it is not uniform — see §3.

---

## 2. Full results, 10 MB corpora

Throughput, higher is better. **Bold** is the best in each row.

| corpus | vela<br>faithful | vela<br>strict | yyjson | yyjson<br>insitu | yyjson<br>pool | simdjson<br>DOM | simdjson<br>OnDemand |
|---|---|---|---|---|---|---|---|
| records | 754 MiB/s | 423 | 1022 | 1.08 GiB/s | 1.28 GiB/s | 1.10 GiB/s | **1.91 GiB/s** |
| int_array | 561 | 408 | 874 | 939 | 1.19 GiB/s | 613 | **1.62 GiB/s** |
| strings | 1.84 GiB/s | 435 | 675 | 722 | 733 | 959 | **5.45 GiB/s** |
| geo_int | 444 | 295 | 598 | 630 | 788 | 468 | **1.96 GiB/s** |
| geo_float | 643 | 317 | 994 | 1018 | 1.18 GiB/s | 744 | **3.62 GiB/s** |

1 MB numbers are within 3% of these except `yyjson/int_array`
(819 vs 874 MiB/s).

### Reading the table honestly

The contenders do not do the same work.

| | validates | numbers | strings | buffer reuse | mutates input |
|---|---|---|---|---|---|
| `vela_faithful` | **no** | **`i64` only, lossy** | raw slices, never decoded | yes | no |
| `vela_strict` | yes | `i64` + `f64` | raw slices, decoded on demand | yes | no |
| yyjson | yes | full | unescaped eagerly, owned | pool variant | insitu variant |
| simdjson OnDemand | yes | full | decoded on request | yes | needs 64 B padding |

`vela_faithful` returns `314` for `3.14` and accepts `{"a":nope]` without
complaint. **`vela_strict` is the row that is comparable to yyjson**, and it
is 2.4× slower than yyjson on `records`.

simdjson On Demand is lazy — it does not build a DOM at all, it walks the
document producing values as asked. It is the fastest column and the least
comparable one.

---

## 3. Where the port wins, and why

**`strings`: 1.84 GiB/s vs yyjson's 675 MiB/s — 2.8× faster.**

This is the one row the port dominates, and it is the whole argument for the
tier-3 representation. A string node is `(offset, len)` into the input.
Bytes inside a string literal are touched exactly once, by Stage 1, and
never again unless a caller asks for that field. yyjson unescapes every
string into the document at parse time, so it pays for data nobody may read.

The cost is deferred, not removed — `vela_strict` is 435 MiB/s on the same
corpus once escape and UTF-8 validation are turned on. But the deferral is
the right default for the common case of reading a few fields out of a
document.

**Everywhere else the port loses by 1.35–1.56×** (2.6–2.8× against yyjson's
pooled allocator). The gap is largest on `int_array` and `geo_int` —
number-dense data — which points at the scalar parser and the per-element
builder work rather than at Stage 1.

---

## 4. Realistic workloads

Parse and sum one integer field across every record, 4 MB:

| | time |
|---|---|
| simdjson On Demand | **2.59 ms** |
| yyjson | 3.72 ms |
| `vela_faithful` | 7.07 ms |
| `vela_strict` | 11.4 ms |
| `serde_json` | 33.2 ms |

Read every field of every record, 1 MB:

| | time |
|---|---|
| simdjson On Demand | **913 µs** |
| yyjson | 1.05 ms |
| `vela_faithful` | 2.13 ms |

One field from the *first* record, 1 MB — the case that separates eager from
lazy:

| | time |
|---|---|
| **`jsonflat`** (this crate, `docs/ZEROCOPY.md`) | **21.7 ns** |
| simdjson On Demand | 297 µs |
| yyjson | 918 µs |
| `vela_faithful` | 1.37 ms |

`jsonflat` is 13 700× faster than simdjson here because it is not parsing —
it is reading a pre-built buffer. That is a different trade (see
`docs/ZEROCOPY.md`), not a like-for-like win.

Validation, 1 MB `records`: simdjson 581 µs, yyjson 917 µs, `vela_strict`
2.47 ms, `serde_json` 7.22 ms.

Parse-and-re-serialize, 1 MB: yyjson **1.68 ms**, `vela_strict` + serde
6.79 ms, `serde_json` 8.75 ms.

---

## 5. A methodology finding worth recording

The first version of this benchmark reported `vela_faithful/int_array` at
**225 MiB/s**. The correct figure is **561 MiB/s**. Nothing about the code
under test differed.

What happened: all contenders ran in one criterion process, in one large
benchmark function. Two things independently perturbed the result by ~2.5×:

* **Inlining.** With `lto = "fat"` and `codegen-units = 1`, adding or
  removing an unrelated call site in the same function changed how
  `Workspace::parse` was inlined into the timed closure.
* **Cross-benchmark interference.** `iter_batched_ref` stages many copies of
  a large input per batch; the allocator and page-residency state that
  leaves behind changes what the *next* benchmark measures. Filtering out
  the `yyjson_insitu` and `yyjson_pool` benchmarks alone moved the
  `vela_faithful` number by 2.5×.

Both fixes are in place:

1. Every timed operation goes through an `#[inline(never)]` wrapper, so all
   contenders pay the same call overhead and codegen cannot drift between
   them.
2. `tools/cbench.sh` runs **one process per implementation**. This is the
   right methodology for a cross-language comparison anyway — the C
   libraries bring their own allocator behaviour, and letting it leak into a
   Rust measurement (or vice versa) makes the numbers meaningless.

The lesson generalises: a 2.5× swing with no code change is not a rare
pathology, it is what happens when several allocation-heavy benchmarks share
a process. Any benchmark comparing across an FFI boundary should isolate.

---

## 6. What this says for Vela

1. **The 4.3× deficit against yyjson was mostly codegen, not design.** Same
   algorithm in Rust is 1.36× behind instead. Before redesigning tier 3,
   make `__json_pool_push`, `__json_ctx_*` and `__tui_str_byte` inline.

2. **A pooled allocator is worth 25–47%.** yyjson's `yyjson_alc_pool` beats
   its own default allocator by that margin on every corpus. Vela already
   has the mechanism — `json_workspace_create` — but only tier 3 uses it,
   and the tier-2/tier-3 navigation APIs throw it away per call.

3. **Lazy strings are the port's one clear advantage — protect it.** 2.8×
   faster than yyjson on string-heavy data comes entirely from not decoding
   strings at parse time. The missing piece is a *correct* on-demand
   unescaper; `json_extract_string` is lossy (`\b`→space, `\uXXXX`→`?`),
   which forces callers back to raw slices.

4. **Number-dense input is where the remaining gap is.** `int_array` and
   `geo_int` show the widest margins. That is the scalar parser and the
   per-element builder, not Stage 1 — Stage 1 is already at 2.1–3.2 GiB/s
   (`docs/PROFILING.md`).

5. **Consider a lazy tier.** simdjson On Demand is 1.6–5.4 GiB/s because it
   never materialises a DOM. For "read three fields from a large document",
   no amount of DOM-building optimisation catches that.

## Current standing, after the streaming rewrite

The tables above measure the pool parser, which is no longer what
`dacodec::from_slice` uses. Re-measured with `src/direct.rs`:

### Sum one integer field across every record, 4 MiB

Parse plus read, which is the thing a caller actually does.

| | MiB/s |
|---|---|
| simdjson On-Demand (C++) | **1 561** |
| yyjson (C) | ~977 |
| **dacodec `direct`** | **659** |
| pool, non-validating | 591 |
| serde_json into structs | 535 |
| pool, validating | 365 |
| serde_json into `Value` | 122 |

simdjson is **2.4x** ahead, yyjson about **1.5x**. Both are C or C++ with
far more work behind them, and simdjson On-Demand's advantage here is
structural: it skips subtrees without materialising them at all, which is
the same trick that makes `flat` fast, applied during the parse.

Against the Rust field, `direct` leads: 23% over `serde_json` into structs
and 5.4x over `serde_json` into `Value`.

### simdjson DOM is *slower* than yyjson on most corpora

Worth separating, because "simdjson is fastest" is not what the numbers
say. DOM construction, 1 MiB, MiB/s:

| corpus | yyjson | yyjson pool | simdjson DOM | simdjson On-Demand |
|---|---|---|---|---|
| records | 1108 | 1299 | 1125 | 1917 |
| int_array | 872 | 1210 | **614** | 1677 |
| strings | 722 | 761 | 973 | 5858 |
| geo_int | 515 | 788 | **460** | 2018 |
| geo_float | 1009 | 1205 | **747** | 3714 |

**simdjson's DOM loses to plain yyjson on three of five**, and to yyjson
with a reused pool on four. A flat node pool is simply a good way to build
a document; SIMD in Stage 1 does not make up for a more expensive Stage 2.

On-Demand is not a faster DOM builder — it is not a DOM builder. It
materialises nothing, converts only what is asked for, and skips the rest
by index arithmetic. Comparing its throughput to a DOM builder's compares
"scan and skip" against "scan and construct".

### Can we close the gap to On-Demand?

The honest answer is mostly no, and the reason is now measured rather than
guessed.

The obvious move is to use our own structural index, which is exactly what
`src/stream.rs` is. It has now been tested on four workloads, including the
skip-heavy one where an index should be at its best:

| workload, 4 MiB | `stream` (indexed) | `direct` (no index) |
|---|---|---|
| owned structs | 286 | **312** |
| borrowed structs | 325 | **352** |
| partial, 2 of 7 fields | 463 | **631** |
| sum 1 field of 5 | 467 | **666** |

**The index has now failed to pay on every workload measured.** Even when
four fifths of the document is skipped, a byte cursor beats it. That is a
strong enough result to stop proposing index-based designs for the typed
path.

Three things separate us from On-Demand, and only one is addressable:

1. **It does not validate what it skips.** Our skip checks every byte, so
   a document `serde_json` rejects is rejected here too, wherever the fault
   is. Giving that up would close much of the gap and would break the
   drop-in guarantee. Not a trade this crate makes.
2. **Its Stage 1 is faster than ours** — 2.1–3.3 GiB/s here. Since
   end-to-end On-Demand reaches 1.56 GiB/s on `c_sum_field`, our Stage 1 is
   close to being the ceiling for any index design we could build on it,
   before the walk costs anything at all.
3. **It seeks a field; serde enumerates them.** `MapAccess` must yield
   *every* key so the derived field matcher can reject the unknown ones. A
   lazy `get("score")` API could skip whole records without ever
   constructing a key. This is the one real opening, and it is the shape
   `Doc::get` and sonic-rs's `get` already have — but it is a different API
   from `Deserialize`, not a speedup of it.

### The one opening, taken: `src/pull.rs`

Point 3 above turned out to be worth building. `pull` seeks named fields
and skips the remainder of each record by counting braces, rather than
enumerating every key so serde can reject it. It allocates **nothing**.

Sum one integer field across 4 MiB of records:

| | MiB/s |
|---|---|
| simdjson On-Demand (C++) | **1 608** |
| yyjson (C) | ~1 010 |
| **`dacodec::pull`** | **778** |
| `dacodec::direct` (serde) | 656 |
| pool, non-validating | 589 |
| serde_json into structs | 538 |
| `dacodec::stream` (indexed) | 467 |
| serde_json into `Value` | 122 |

That is +19% over the serde path and closes the gap to simdjson from 2.4x
to 2.1x. Against yyjson's own `sum_field`, which is C doing the same job,
we are now within 25%.

**And the first version of it was wrong.** `Fields::finish` walked the
remaining fields one at a time, which undoes the early exit entirely —
selecting the *first* of seven fields measured the same as selecting the
*last* (731 vs 729 MiB/s). That flatness is what exposed the bug. With the
tail skipped by brace counting instead:

| field selected | position | MiB/s |
|---|---|---|
| `id` | 1st of 7 | **998** |
| `age` | 3rd | 976 |
| `city` | 5th | 929 |
| `score` | 6th | 892 |
| `tags` | 7th | 864 |

Position now matters, which is the evidence that the skip is real.

**The trade is explicit.** Content in the skipped tail is not validated:
`[{"a":1,"b":01}]` is rejected by `from_slice` and by `serde_json`, and
accepted here. That is the same trade simdjson On-Demand makes, and it is
why this is a separate function with its own contract rather than a
speed-up of `from_slice`. `pull::tests::malformed_content_in_a_skipped_tail_is_accepted`
pins it, and asserts that `from_slice` still rejects all of it.

### A benchmark bug found while re-measuring

The `serde_json` row of `c_sum_field` was parsing the document **twice** —
once through a `w_serde_value` wrapper and again with `from_slice` — so it
reported 64.5 MiB/s where the honest figure is 121.7. The wrapper existed
to keep codegen shapes comparable between contenders and quietly doubled
the work for one of them.

Reporting a competitor at half its real speed is worse than not measuring
it. Fixed, and the typed row was added alongside, since deserializing only
the field you want is what anyone summing a field would write.

