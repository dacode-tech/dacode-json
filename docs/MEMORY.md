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

## 4. Actions

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
