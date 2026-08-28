# Memory

Peak resident memory for parsing the same 10.44 MiB `records` document,
one process per implementation.

Apple M2 Max, `rustc 1.95.0`, release build. Reproduce with
`tools/memprofile.sh records 10485760`.

---

## 1. Choosing the instrument, and rejecting the obvious one

The natural tool is the allocator's own accounting: `mstats()` on macOS,
`mallinfo2()` on glibc. Rust's default allocator *is* `malloc` on both, so
one number would cover Rust and C alike — exactly what a cross-language
comparison needs.

**It does not work on macOS.** `memprofile selftest` calibrates it against
known sizes:

```text
                        alive        freed_on_drop      expected
8 MiB single Rust Vec   0.00 B       0.00 B             8 MiB
8 MiB × 131072 small    11.00 MiB    8.00 MiB           8 MiB  (+2 MiB outer Vec)
8 MiB C malloc          8.00 MiB     48.00 B            8 MiB
```

Two disqualifying failures:

* **Large allocations are invisible.** A single 8 MiB `Vec` reports 0 B.
  macOS routes large blocks through an mmap-backed path that
  `mstats().bytes_used` does not count.
* **Frees are not observed.** An 8 MiB C `malloc` is counted on allocation
  and reports 48 B freed on release.

Small allocations *are* counted correctly, which is what makes the bug
dangerous rather than merely wrong: `serde_json`'s 1.3 million little
allocations show up fine, and the tape parsers' handful of big ones do not.
An earlier draft of this document reported `retained` from `mstats()` and
had `simdjson_dom` at **32 bytes**.

So the primary metric is **peak RSS** from
`getrusage(RUSAGE_SELF).ru_maxrss`, which counts everything — Rust, C,
`mmap`, stacks. It is monotonic per process, so each implementation gets a
fresh process and a `baseline` run (generate and touch the corpus, parse
nothing) establishes the floor.

Rust `GlobalAlloc` counters supplement it with **allocation counts**, which
no size metric can show. They are exact for Rust and structurally zero for
yyjson and simdjson, since a C `malloc` never reaches Rust's allocator.

---

## 2. Results

Baseline peak RSS **11.89 MiB** (corpus resident, nothing parsed).

| implementation | peak RSS | over baseline | ×JSON | rust allocs | rust bytes req. |
|---|---|---|---|---|---|
| `vela_tier1` | 11.95 MiB | **64 KiB** | **0.01×** | 1 | 12 B |
| `serde_json` → structs | 35.05 MiB | 23.16 MiB | 2.22× | 454 677 | 21.20 MiB |
| **yyjson** | 49.41 MiB | 37.52 MiB | 3.59× | 2 | 22 B |
| `sonic_rs::Value` | 54.00 MiB | 42.11 MiB | 4.03× | 22 | 125.94 MiB |
| `vela_tier3_pool` | 55.25 MiB | 43.36 MiB | 4.15× | 6 | 125.26 MiB |
| `vela_strict` | 55.36 MiB | 43.47 MiB | 4.16× | 7 | 125.26 MiB |
| `vela_tier3_grow` | 55.47 MiB | 43.58 MiB | 4.17× | 25 | 73.76 MiB |
| `jsonflat` typed | 56.06 MiB | 44.17 MiB | 4.23× | 454 743 | 37.14 MiB |
| `vela_tier2_tape` | 58.83 MiB | 46.94 MiB | 4.50× | 25 | 73.75 MiB |
| **simdjson** DOM | 63.78 MiB | 51.89 MiB | 4.97× | 4 | 10.44 MiB |
| `simd-json` tape | 97.73 MiB | 85.84 MiB | 8.22× | 11 | 135.58 MiB |
| `jsonflat` dynamic | 102.81 MiB | 90.92 MiB | 8.71× | 107 059 | 168.63 MiB |
| `serde_json::Value` | 108.92 MiB | 97.03 MiB | 9.30× | 1 310 661 | 82.99 MiB |
| `simd-json` owned | 159.42 MiB | 147.53 MiB | 14.13× | 1 524 652 | 187.23 MiB |

`jsonflat` figures include parsing the JSON first, since that is how a
buffer gets built; the resulting buffers are 7.28 MiB (typed) and 16.07 MiB
(dynamic).

---

## 3. What the numbers say

**Tier 1 costs nothing — 64 KiB for a 10 MiB document.** One allocation, 12
bytes. It never builds a structure; navigation returns subslices of the
input. Combined with `docs/TIERS.md` showing it is also the *fastest* tier
for single-field access, this is a strong argument for Vela's default. If
memory matters, no indexed parser is competitive with not building an index.

**yyjson is the most memory-efficient DOM at 3.59×**, and it is the row to
compare tier 3 against: 3.59× versus 4.15×, so tier 3 uses **16% more
memory** for a structure that stores strictly less (no unescaped strings).
That is a fair loss, not a rout.

**Requested bytes and resident bytes are very different numbers.**
`vela_tier3_pool` *requests* 125.26 MiB — twelve times the input — because
`Workspace::with_capacity` sizes the pool at `len/2 + 64` nodes and the
index at `len + 1` slots. Peak RSS is only 43.36 MiB, because untouched
pages are never committed. Pre-sizing costs address space, not much
residency: `vela_tier3_grow`, which lets the vectors grow instead, requests
41% less (73.76 MiB) and ends up at the same 4.17×. So the over-allocation
is close to free here — but it would not be on a memory-constrained target
or with an allocator that commits eagerly.

**Allocation count is where `serde_json::Value` loses.** 1.31 million
allocations for a 10 MiB document, against 6 for the tier-3 pool. That is
the same fact the CPU profile reports from the other direction
(`docs/PROFILING.md`: 22% of `serde_json`'s time in `from_utf8`, plus
malloc). Deserialising to *structs* instead drops it to 454 677
allocations and 2.22× memory — the best DOM-free result in the table, and a
reminder that `Value` is a debugging convenience, not a parsing strategy.

**`simd-json`'s owned value is the worst option measured**, at 14.13× and
1.52 million allocations. Its borrowed tape is 8.22×. Both are well above
its C++ ancestor's 4.97×.

**The dynamic `jsonflat` encoder is memory-hungry at 8.71×**, because
building a buffer means holding the source JSON, a strict-parser pool, the
interning map and the output at once. The typed encoder is 4.23×. Neither is
a *read* cost — reading either buffer needs only the buffer — but it makes
the write side unattractive for large documents without a streaming builder.

---

## 4. The read path: what zero-copy actually costs

Sections 2 and 3 measure *building* a representation. The claim behind
`flat` is about the other side — reading one that already exists — so it
needs its own measurement.

The workloads `read_jsonflat_typed`, `read_jsonflat_dyn` and
`read_serde_json` each sum a field across every record of the `records`
corpus (10.44 MiB). The buffer is prepared **outside** the measurement
window; only the reading is measured.

### Peak RSS is the wrong instrument here

Peak RSS is monotonic per process. Preparing the buffer pushes the peak up
*before* the window opens, so the absolute peak is not attributable to the
read — the first run of this reported "44 MiB to read a document that
allocates nothing", which is preparation, not reading.

`tools/memprofile.sh` therefore reports two figures: the absolute peak
(blanked for `read_*` rows, because it is meaningless there) and the
*additional* peak caused by the measured work alone. For the read rows the
latter is 64 KiB — one page-granularity step, i.e. nothing.

### The allocation counters tell it exactly

| workload | in-window RSS | allocations | bytes allocated |
|---|---|---|---|
| `read_jsonflat_typed` | 64 KiB | **2** | **36 B** |
| `read_jsonflat_dyn` | 64 KiB | **1** | **18 B** |
| `read_serde_json` | 97 MiB | 1 310 661 | 82.99 MiB |

Reading a 10.44 MiB document costs **36 bytes and two allocations**, against
`serde_json`'s 83 MiB and 1.31 million. The two allocations are the
accumulator `Vec`s in the harness, not the reader; the reader itself
allocates nothing at all.

This is the whole argument for the format, in one table. It is not that
reading is *faster* — it is that reading does not build anything, so there
is nothing to allocate and nothing to free. `serde_json` must reconstruct
the entire document to read one field from it.

The cost is paid at encode time and in flexibility: the layout is fixed, and
a schema change is a format change. See `docs/ZEROCOPY.md`.

## 5. `Box<[T]>` and `Box<str>` against `Vec<T>` and `String`

A `Vec<T>` is 24 bytes — pointer, length, capacity — and its buffer may
hold slack, because growing doubles. `Box<[T]>` is 16 bytes and holds
none. Same for `String` against `Box<str>`. So the question is whether
shrinking the target struct is worth it.

Deserializing the 10.44 MiB `records` corpus into `Vec<Record>`, where the
boxed variant differs only in using `Box<str>` and `Box<[Box<str>]>`:

| | peak over base | allocations | bytes allocated |
|---|---|---|---|
| `serde_json` → `Vec<Record>` | 23.14 MiB | 454 677 | 21.20 MiB |
| `serde_json` → `Vec<RecordBoxed>` | **16.20 MiB** | 534 879 | 15.75 MiB |
| `dacodec::stream` → `Vec<Record>` | 36.23 MiB | 454 679 | 59.28 MiB |
| `dacodec::stream` → `Vec<RecordBoxed>` | **32.14 MiB** | 454 679 | 55.06 MiB |

**Boxing is worth it: 30% less peak memory for `serde_json`, 11% for the
streaming path.** Three heap fields per record, at 8 bytes of header each
plus shed slack, over 106 998 records.

It is not free. serde has no `Box<str>` parser: it deserializes a `String`
and calls `into_boxed_str`, which reallocates and copies whenever capacity
exceeds length. That shows up as `serde_json`'s allocation count rising
from 454 677 to **534 879**, +80 202 — roughly one extra allocation per
record, for the `tags` vector whose doubling left slack to shed.

The streaming path shows **no such increase**, and that is the interesting
part. `SeqAccess::size_hint` reports the exact element count, taken from
the structural index, so `tags` is allocated at the right size the first
time. With no slack, `into_boxed_slice` has nothing to shrink and returns
the same allocation. **An exact size hint makes boxing free.**

So the guidance is:

* boxing helps whenever a struct is kept, and helps most when it has many
  small collections;
* it is a property of the target type, so it costs nothing here to
  support — it already works;
* do not box values you are about to drop: you would pay the shrink for
  memory you never hold.

### The real memory cost is ours, not the struct's

The streaming path uses more memory than `serde_json` for identical
output, and boxing does not close that gap. The cause is the structural
index, which was reserved for the worst case — one structural character
per input byte, so **4 bytes of index per input byte**:

| | reserved | actually used |
|---|---|---|
| `records`, 10.44 MiB | 43.78 MiB | 17.01 MiB |

2.6x more than needed, and the single largest memory cost of streaming
deserialization. `StructuralIndex::reserve_estimated` now reserves for 50%
structural density instead, which halves it; denser documents still work
because the scanners grow the vector, exactly as `scan()` already relied on
when starting from 4 KiB. `tests/stream.rs` covers 57%, 100% and
all-string documents to prove the growth path is correct.

That took `Vec<Record>` from 39.42 to 36.23 MiB with **no change in
throughput** (263 vs 264 MiB/s). The remaining ~22 MiB is the index at 2x
the input size, and it is why streaming trades memory for the delimiter
scan it does not have to repeat. Sizing it from measured density rather
than a fixed fraction is the next available win.

## 6. Actions

1. **`Workspace::with_capacity`'s heuristic is very loose** — `len/2 + 64`
   nodes assumes two bytes per node, the `[0,0,0,...]` worst case. Real
   documents are 5–60 bytes per node. Sizing from a first-pass structural
   count, which tier 3 already computes, would cut requested memory ~4×
   with no measurable RSS change today and a real one under a committing
   allocator.

2. **Add a streaming `jsonflat` builder.** Encoding currently peaks at
   8.71× because everything is live simultaneously. Writing records
   straight to the output while parsing would bring the write side near the
   read side.

3. **Recommend `serde` structs over `Value` explicitly.** 2.22× versus
   9.30×, and 3× fewer allocations, for the same data.

4. **Do not trust `mstats()`/`mallinfo2()` without calibrating first.**
   `memprofile selftest` takes a second and would have prevented a
   published table of nonsense. This applies to anyone measuring memory on
   macOS, not just this project.
