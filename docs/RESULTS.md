# Results

> **All numbers below were re-measured with one process per benchmark**
> (`tools/isolate.sh`) after a harness problem was found in
> `benches/cbaseline.rs`. See §12 for the validation: `benches/parse.rs` and
> `benches/query.rs` turned out **not** to be affected (≤1.8% difference),
> so the earlier conclusions stand — but the absolute figures for this port
> were stale by 18–30%, because the Stage 1 work in §9 landed after they
> were first recorded.

Machine: Apple M2 Max (12 core), 32 GB, macOS 15.6.1
Toolchain: `rustc 1.95.0 (59807616e 2026-04-14)`, `opt-level=3`, `lto="fat"`, `codegen-units=1`
Harness: Criterion 0.5, 30–100 samples, throughput in MiB/s of **input** bytes
Corpora: generated deterministically by `src/corpus.rs` (no data files)

> Rust 1.98.0 is the current release. Per the instruction to use the installed
> toolchain, everything below is 1.95.0. Nothing here depends on a feature
> newer than 1.61 (stable NEON intrinsics).

---

## 0. Contents

* §1–5 — the original DOM-parsing comparison
* §6 — bugs found in the Vela sources
* §7 — recommendations
* §8 — **table-driven classification**: the optimisation Vela specified and
  never built, and why it would not have helped
* §9 — **Stage 1 rebuilt**: the real bottleneck, 2.4–3.8× Vela
* §10 — **struct serialize / deserialize** vs serde_json, simd-json, sonic-rs
* §11 — float precision: where serde_json is wrong

Companion documents:

| | |
|---|---|
| `docs/TIERS.md` | all four Vela tiers head-to-head; which should be the default |
| `docs/PROFILING.md` | where every parser spends its time, and how to reproduce |
| `docs/ZEROCOPY.md` | the YaFF question: a zero-copy wire format for JSON |
| `docs/UNWRAP_FREE.md` | panic-free design study |

---

## 1. The headline

Porting Vela's tier-3 algorithm to Rust, unchanged, makes it **2.3–3.7× faster**
on the same work. The algorithm was never the problem.

| Stage | Vela (`JSON_IMPROVEMENT_PLAN.md:52-73`) | This port | Speedup |
|---|---|---|---|
| Stage 1 structural scan | 914 MB/s | 1.13–2.34 GiB/s | **1.3–2.6×** |
| Full DOM build | 166 MB/s | 383–606 MiB/s | **2.3–3.7×** |
| 68-byte document | 5 590 ns | 73 ns | **76×** |

Vela's own analysis concluded:

> *"Vela's structural scan (914 MB/s) is competitive with simdjson (860 MB/s)
> — the gap is entirely in DOM building (166 vs 860 MB/s)."*

That diagnosis is confirmed, and the cause is narrowed: the DOM builder was
slow because every pool write, every context push and every byte read was an
opaque cross-module call (`__json_pool_push`, `__json_ctx_inc_count`,
`__tui_str_byte`). In Rust the same operations inline into the walk loop.

The 76× on small documents is a different effect entirely: it is the three
`mmap` calls per parse that `json_parse_inline` (J2) was written to avoid. A
reused `Workspace` removes them without needing a 96 KB static buffer.

---

## 2. Stage 1 — structural indexing (1 MiB corpora)

| corpus | scalar | S6 branchless | S6b 2× unrolled |
|---|---|---|---|
| records | 545 MiB/s | 958 MiB/s | **1.135 GiB/s** |
| int_array | 546 MiB/s | 1.734 GiB/s | **2.160 GiB/s** |
| strings | 670 MiB/s | 1.922 GiB/s | **2.343 GiB/s** |
| geo_int | 493 MiB/s | 1010 MiB/s | **1.138 GiB/s** |
| geo_float | 551 MiB/s | 1.642 GiB/s | **1.911 GiB/s** |

The 2× unroll is worth a consistent 15–25% over plain S6, which is a larger
gain than it has any right to be for pure loop-overhead amortisation — the
combined 32-bit mask also halves the number of entries into the extraction
loop.

Throughput tracks **structural density**, not corpus size:

| | structurals/byte | throughput |
|---|---|---|
| `int_array` | 0.2045 | 2.20 GiB/s |
| `strings` | 0.0525 | 2.34 GiB/s |

Both are near the memory-bandwidth-bound ceiling for this loop; the classify
step costs the same per byte either way, and only the `cttz` extraction loop
scales with density.

---

## 3. Full parse — 10 MiB corpora

Throughput in MiB/s. **Bold** is the best in each row.

| corpus | vela<br>faithful | vela<br>strict | serde_json<br>Value | simd-json<br>tape | simd-json<br>borrowed | sonic-rs<br>Value |
|---|---|---|---|---|---|---|
| records | **785** | 419 | 132 | 678 | 241 | 591 |
| int_array | **559** | 409 | 281 | 502 | 270 | 542 |
| strings | **1 893** | 431 | 250 | 541 | 505 | 717 |
| geo_int | **450** | 294 | 101 | 380 | 115 | 324 |
| geo_float | **654** | 319 | 178 | 613 | 207 | 507 |

The port now leads every corpus, which it did not when these were first
measured — the Stage 1 work in §9 moved it 8–30%. Competitor figures moved
by at most 8% and mostly under 3%.

Note this is `vela_faithful`, which skips validation and mangles floats.
`vela_strict` is the comparable row and it loses to simd-json's tape and
sonic-rs everywhere. And against the C libraries these were modelled on,
both lose — see `docs/CBASELINE.md`.

### Do not read that table as a ranking

The contenders do not compute the same thing, and every difference favours
`vela_faithful`:

| | validates | numbers | strings | reuses buffers | mutates input |
|---|---|---|---|---|---|
| `vela_faithful` | **no** | **`i64` only, lossy** | raw slices | yes | no |
| `vela_strict` | yes | `i64` + `f64` | raw slices | yes | no |
| `serde_json::Value` | yes | full | owned `String` | no | no |
| `simd_json::to_tape` | yes | full | in-place unescape | yes | **yes** |
| `simd_json::to_borrowed_value` | yes | full | borrowed | yes | **yes** |
| `sonic_rs::Value` | yes | full | owned | no | no |

`vela_faithful` returns the wrong answer for `3.14` (it produces `314`) and
accepts `{"a":nope]` without complaint. **`vela_strict` is the only column
that is comparable to the others.** Read that row.

Also note simd-json needs a fresh mutable, padded copy of the input per
iteration because it unescapes in place. That copy is excluded from the
timing via `iter_batched_ref`, which flatters it relative to an application
that has to make it.

### The honest comparison

Against the three reference libraries, `vela_strict` — same validation
guarantees, same number semantics:

* beats `serde_json::Value` on every corpus, by **1.3–3.0×**
* beats `simd_json::to_borrowed_value` on 4 of 5, by up to **2.3×**
* loses to `simd_json::to_tape` and `sonic_rs::Value` on every corpus,
  by **1.3–1.8×**

So: the ported design lands solidly in the middle of the Rust field. It is a
genuinely good design that beats the ubiquitous default by a wide margin and
does not reach the state of the art.

### Where the design actually wins: `strings`

`vela_faithful` hits **1.96 GiB/s** on string-heavy data, 3.5× faster than
simd-json's tape. This is not a trick, it is the whole point of the
representation: string nodes are `(offset, len)` pairs into the original
input, so bytes inside a string literal are touched exactly once, by Stage 1,
and never again unless someone asks for that field.

simd-json unescapes every string into a side buffer during parsing.
`serde_json` allocates a `String` per string. Both pay for data the caller
may never read.

The cost is deferred, not eliminated — and `vela_strict` shows the bill:
439 MiB/s once escape and UTF-8 validation are turned on, versus 1.78 GiB/s
with `validate_strings: false`. That corpus is deliberately adversarial
(an escape roughly every eight characters); on `records`, whose strings are
short clean ASCII words, validation costs only 16% (366 vs 434 MiB/s).

---

## 4. Small documents

Time per parse, 68-byte document:

| | time | vs Vela |
|---|---|---|
| `vela_faithful`, reused workspace | **75 ns** | 75× faster |
| `simd_json::to_tape` | 111 ns | |
| `vela_strict`, reused workspace | 132 ns | |
| `sonic_rs::Value` | 159 ns | |
| `vela_faithful`, fresh allocation | 190 ns | 29× faster |
| `serde_json::Value` | 361 ns | |
| *Vela `t860` measurement* | *5 590 ns* | *baseline* |
| *yyjson (`t860`)* | *104 ns* | |
| *simdjson (`t860`)* | *72 ns* | |

Vela's 5 590 ns was three `mmap` calls, not parsing. Reusing a `Workspace`
puts the port at 75 ns — level with the C libraries Vela was measured
against, and it makes `json_parse_inline`'s 96 KB static BSS buffer (task J2)
unnecessary. Even the *fresh allocation* path is 29× faster than Vela,
because `Vec` uses the allocator rather than going to the kernel.

The two allocation-dominated rows — `vela_faithful_fresh` (135 → 190 ns) and
`serde_json` (322 → 361 ns) — got *slower* under isolation, because a fresh
process has cold allocator free lists where a shared one is warm from
previous benchmarks. Neither figure is wrong; they answer different
questions ("first call in a process" versus "steady state"). The isolated
number is the conservative one.

---

## 5. Query workloads

Raw DOM throughput flatters lazy representations, so these measure parse plus
a realistic access pattern (4 MiB of records).

**Sum one integer field from every record** — MiB/s:

| | 256 KiB | 4 MiB |
|---|---|---|
| sonic-rs `Value` | **614** | 529 |
| vela faithful | 582 | **593** |
| vela strict | 365 | 369 |
| sonic-rs lazy iter | 261 | 264 |
| serde_json `Value` | 148 | 127 |

**Read every field, decoding strings** — MiB/s:

| | 256 KiB | 4 MiB |
|---|---|---|
| vela faithful | **487** | **495** |
| vela strict | 324 | 328 |
| serde_json `Value` | 150 | 135 |

The pool holds its advantage when everything is read, because decoding on
demand into a `Cow<str>` still borrows for the (common) escape-free case.

**One field from the first record** — the case that exposes the limit of the
whole approach:

| | time |
|---|---|
| sonic-rs lazy pointer | **54 ns** |
| vela faithful | 342 µs |
| serde_json `Value` | 1 669 µs |

6 400× — because `sonic_rs::get(bytes, pointer![0, "name"])` never builds a
DOM at all. It skips to the first element and stops. Both Vela tiers, and
every eager DOM parser, index all 256 KiB first.

This is the honest limit of the tier-3 architecture: **a lazy/streaming API
beats any DOM builder for point queries, by orders of magnitude.** If Vela
wants a fast `json_object_get` on large documents, the answer is not a faster
tape — it is not building a tape.

---

## 6. Bugs and divergences found in the Vela sources

Documenting the port surfaced four issues in the original.

### 6.1 S6/S6b disagree with the scalar scanner on invalid input — real

`tests/scanner_equivalence.rs::documented_divergence_backslash_outside_string`

The scalar reference only treats `\` as an escape when already inside a
string (`tier2/structural.vl:83-89`). The branchless scanner inherits
simdjson's ordering: `__simd_json_find_escaped` runs over the raw backslash
mask before the string mask exists (`structural_simd.vl:233-240`), so a
backslash outside a string still escapes the next byte.

```text
input:      \"aaaaaaaaaaaa:1"    (17 bytes, so the SIMD path runs)
scalar:     [1, 16]
branchless: [14, 16]
```

Valid JSON cannot observe this. Malformed JSON can, and since tier 3 does no
validation, **the parse result for malformed input depends on which scanner
is compiled in**. `t859_json_branchless.vl` asserts S6 ≡ scalar but all three
of its escape cases put the backslash inside a string, so it does not catch
this. The shipped fast path calls S5, which does gate on `in_string` and
therefore agrees with the scalar reference — only S6/S6b diverge.

Found by randomised differential testing; the minimal case was found by
delta-debugging from a 200-byte random input.

**Suggested fix:** mask the backslash input to `find_escaped`, or accept the
difference and note in `t859` that S6 is only equivalent on valid input.

### 6.2 `read_null` is dead code — cosmetic but misleading

`runtime/json_pool_write.ll:732-735`

```llvm
read_null:
  ; Word compare for documentation; both paths push null
  br label %push_null
```

The comment says a word compare happens. It does not. `null` is never
validated, so `nope`, `n`, and `@!?` all parse as `null`. That is
intentional (null is the universal fallback), but the block should be deleted
rather than left looking like a check.

### 6.3 `pool_container_to_json` reads array and object children differently

`tier3/parse.vl:809` vs `:834`

Objects take the first child from `pool_payload(idx)`; arrays take it from
`idx + 1`. Both are the same value given the layout invariant, so it works —
but only by accident. If the payload ever stops being redundant, arrays
silently break.

### 6.4 `json_pool_object_get_float` returns `i64`

`tier3/parse.vl:609`

The declared return type is `i64` and it returns the raw NUMBER payload.
Given that the number parser destroys the fractional part first, the function
cannot do anything useful; it is `json_pool_object_get_int` under a different
name.

---

## 7. What Vela should take from this

Ordered by value per unit of effort.

1. **The DOM builder gap is a codegen problem, not an algorithm problem.**
   Same algorithm, 2.3–3.7× faster under rustc/LLVM. Before redesigning
   Stage 2, make `__json_pool_push`, `__json_ctx_*` and `__tui_str_byte`
   inline. `stage2_lib_json_tier3_opt` (`BUILD.bazel:1429`) already merges
   the runtime `.ll` for LTO — the measurement to run is whether tier 3's
   166 MB/s was taken with that target or without it.

2. **Reusing a workspace removes the 76× small-document penalty**, and does
   it better than `json_parse_inline`. J2's 96 KB static BSS buffer is a
   workaround for `__vela_page_alloc` going straight to `mmap`. A real
   allocator with free-list reuse fixes this everywhere at once, not just for
   JSON under 4 KB, and without `json_parse_inline`'s thread-safety caveat.

3. **The lazy-string design is the best thing in tier 3.** It is why the port
   beats simd-json by 3.5× on string-heavy data. Keep it. The missing piece
   is a correct on-demand unescaper — `json_extract_string` is lossy
   (`\b`→space, `\uXXXX`→`?`), which forces callers back onto raw slices.

4. **Integer-only numbers are the biggest correctness gap.** `3.14 → 314` is
   not an approximation, it is a wrong answer, and it is silent. The strict
   parser here shows the fix costs about 20% on number-dense data, less
   elsewhere.

5. **Validation is affordable.** Full RFC 8259 conformance — grammar,
   bracket matching, number grammar, escape syntax, surrogate pairing, UTF-8
   — costs 1.4–2.0× over no validation and still beats `serde_json` by
   1.3–3.0×. Tier 3 currently produces a well-formed but wrong pool for
   malformed input, with no way for a caller to find out.

6. **For point queries, build a lazy API instead of a faster tape.** 54 ns
   versus 421 µs is not a gap that a better DOM builder closes.

---

## 8. Table-driven classification — the optimisation Vela specified

`docs/stage2/P1_2_JSON_TIERS.md:88-92` lists, for tier 3:

> *Branchless character classification (**lookup tables**)*

and `docs/stage2/JSON_DESIGN.md:286-291` lists simdjson's "Lookup-4"
algorithm. **Neither was ever built.** Tier 3 ships eight `icmp eq` plus
five `or` (`runtime/simd_json_branchless.ll:26-40`) and an if-chain in the
scalar path (`tier2/structural.vl:42-51`). There is no lookup table anywhere
in Vela's JSON stack; the only real tables in the repository are the AES
S-box (`crypto/aes.vl:23`) and the vendored simdjson/yyjson used as C
benchmark baselines.

So it was built here, both ways (`src/scan/table.rs`):

* a 256-entry class table for the scalar scanner;
* a nibble-shuffle pair evaluated with `vqtbl1q_u8`. simdjson's ARM64
  tables distinguish two classes; three were needed (structural / quote /
  backslash), so a new pair was derived and verified exhaustively over all
  256 bytes.

**It makes things slower.** Measured in isolation the shuffle table is 19%
faster than Vela's compares; inside the scanner it is 2–8% *slower*, because
classification is off Stage 1's critical path and a table needs an extra
`cmtst` to reach the movemask. A hybrid — table for structural, compares for
quote and backslash — is 32% faster in isolation and ~1.5% faster
end-to-end.

Full analysis in `docs/PROFILING.md` §1. **Vela shipping the comparison
version was accidentally the right call.**

---

## 9. Stage 1 rebuilt

Phase decomposition showed classification is 13–25% of Stage 1 and position
extraction is **64–82%**. Replacing Vela's serial per-bit `cttz` loop with
simdjson's unconditional eight-slot write:

| corpus | Vela's algorithm, ported | + hybrid classifier + unrolled extract |
|---|---|---|
| records | 1.159 GiB/s | **2.049 GiB/s** |
| int_array | 2.113 GiB/s | **3.246 GiB/s** |
| strings | 2.318 GiB/s | 2.140 GiB/s |
| geo_int | 1.164 GiB/s | **2.005 GiB/s** |
| geo_float | 1.935 GiB/s | **2.509 GiB/s** |

Against Vela's reported 914 MB/s: **2.4× to 3.8×**. (`strings` regresses 8%
— its structural density is 0.05/byte, so writing eight slots to record one
position wastes stores.)

---

## 10. Struct serialize / deserialize

`src/de.rs` and `src/ser.rs` add `serde` support over the pool, so every
contender fills the *same* `#[derive(Deserialize)]` type. This is the
fairest comparison here: all four do identical work and produce identical
results.

### Deserialize into `Vec<Record>` — MiB/s

| | 256 KiB | 4 MiB |
|---|---|---|
| sonic-rs | **390** | **380** |
| simd-json | 416 | 348 |
| serde_json | 289 | 293 |
| vela (strict + pool) | 181 | 194 |

**The pool loses, and the reason is architectural.** `serde_json` and
sonic-rs are single-pass streaming deserializers: they never build an
intermediate representation, because filling a struct does not need one. The
pool must be built first and then walked. Profiling puts that split at
roughly 45% parse / 55% walk, with 7.6% in `skip_subtree` alone — the cost
of variable-width children.

This is the mirror image of §3, where the same representation beat
everything on string-heavy DOM construction. A tape is the right shape for
"parse once, query repeatedly" and the wrong shape for "parse once, discard".

### Removing the pool: `src/stream.rs`

The obvious response is to stop building the intermediate. `src/stream.rs`
keeps Stage 1 (the structural index) and decodes straight into serde
visitors — the shape of simdjson's On-Demand API. Throughput, 4 MiB,
isolated:

| | pool | **stream** | serde_json | sonic-rs | change |
|---|---|---|---|---|---|
| owned `Vec<Record>` | 200 | **263** | 294 | 377 | +31% |
| borrowed `Vec<RecordRef>` | 223 | **320** | 326 | 436 | +43% |
| partial, 2 of 7 fields | 268 | **432** | 528 | 555 | +61% |

It closes 60–70% of the gap and reaches parity on the borrowed path
(320 vs 326), but **does not overtake `serde_json`**. Removing the
intermediate was necessary and not sufficient.

What remains is structural. Stage 1 costs 13.1% of streaming
deserialization (`write_bits` 10.0%, `scan_into` 3.1%) and `serde_json`
pays nothing equivalent: it finds delimiters with scalar code as it goes,
and never pays for the ones it skips. The index has to earn back 13% before
it wins anything, and on this workload it does not.

It is also more accurate than the pool. `10000000000000000999` has 20
digits, so it exceeds the pool's `i64` fast path and is stored as `f64`,
coming back as `1e19`; the streaming parser keeps it as an exact `u64`, as
`serde_json` does. Pinned by
`tests/stream.rs::keeps_integer_precision_that_the_pool_loses`.

`dacodec::from_slice` now uses this path. `Parser`/`Doc` still build a pool,
which is the right structure for querying rather than converting.

### Removing Stage 1 as well: `src/direct.rs`

If the index is what keeps the streaming path behind, removing it should
close the gap. `src/direct.rs` is the same deserializer with the same
scalar decoding — shared functions, so the two cannot drift — and one byte
cursor instead of a structural index.

Whitespace is skipped a byte at a time, as `serde_json` does. String
terminators are found eight bytes at a time with SWAR: `chunk ^
broadcast('"')` gives a zero byte at each quote, and
`(v - 0x01..) & !v & 0x80..` lifts those to high bits, so one
`trailing_zeros` gives the offset. No target-feature detection, no
`unsafe`.

4 MiB, isolated:

| | pool | stream | **direct** | serde_json | sonic-rs |
|---|---|---|---|---|---|
| owned | 200 | 290 | **309** | 298 | 377 |
| borrowed | 222 | 328 | **362** | 330 | 438 |
| partial, 2 of 7 | 264 | 451 | 503 | **529** | 555 |

**It beats `serde_json` on the owned and borrowing paths** — +3.6% and
+9.6% — and is 4.9% behind on partial deserialization, where `serde_json`'s
skip is still better than ours.

Memory, deserializing 10.44 MiB into 106 998 records:

| | peak over base | allocations | bytes allocated |
|---|---|---|---|
| `stream` (with index) | 36.25 MiB | 454 679 | 59.28 MiB |
| **`direct`** | **23.17 MiB** | **454 677** | **21.20 MiB** |
| `serde_json` | 23.11 MiB | 454 677 | 21.20 MiB |

`direct` matches `serde_json` to the allocation, because both allocate
nothing but the output.

**So Stage 1 was the answer to the question in §12.** It is a genuine win
for building a DOM, where the index is walked once and the result is kept —
and a genuine loss for filling a struct, where it is a second pass over the
document and 22 MiB of memory in service of delimiters that a byte cursor
finds as it goes. `dacodec::from_slice` uses `direct`.

One thing the index still buys that `direct` cannot: an exact
`SeqAccess::size_hint`, because counting elements is an index walk. That
makes `Box<[T]>` conversion free (`docs/MEMORY.md` §5) — with an exact
hint there is no slack for `into_boxed_slice` to shed. `direct` shows
`serde_json`'s +80 202 allocations on the boxed struct; `stream` does not.

### Copying sonic-rs, and why most of it did not apply

sonic-rs is 22% ahead on owned structs, so its source (0.5.8) was read to
find out how. Six techniques, verified in the code:

1. **It never leaves the bitmask domain.** Every operation is a `u64` mask
   over 64 input bytes. Skipping a container is `count_ones` and
   `trailing_zeros` on brace masks (`skip_container_loop`, parser.rs:171) —
   no byte is touched individually. **We compute the same masks and then
   materialise them into a `Vec<u32>` of positions**, which is `write_bits`,
   10% of the streaming profile, plus 4 bytes per structural character.
2. **`prefix_xor`** — a six-step shift-XOR ladder turning a quote mask into
   an in-string mask, so `& !instring` removes braces inside strings with no
   branching. They avoid PMULL on aarch64: "apparently slow".
3. **A cached whitespace bitmap**, reused across calls while the cursor
   stays inside the same 64 bytes.
4. **simdutf8** for UTF-8 validation — the 22.2% `serde_json` spends in
   `core::str::from_utf8`.
5. **236 `unsafe` blocks**, eliminating bounds checks throughout the parser.
6. **Deliberate over-reading**, guarded by a 4 KiB page-boundary check, and
   only on Linux and macOS.

Measuring the corpus before copying any of it was the useful step:

| | `records` |
|---|---|
| whitespace | **0%** |
| mean bytes between quotes | **4.8** |

So (3) is worthless here — there is no whitespace to skip — and (1) applied
to string scanning would be actively harmful: loading and masking 64 bytes
to find a terminator 4.8 bytes away is waste, and the existing 8-byte SWAR
already overshoots. Neither was implemented.

What the measurement *did* show is that every string was scanned twice:
once to find the closing quote, once in `unescape_checked` to validate
escapes, control bytes and UTF-8. At 4.8 bytes a string the second pass is
mostly call overhead. Fusing them — the SWAR loop now also reports whether
the content is plain printable ASCII, in the same pass — gives:

| 4 MiB | before | after | serde_json | sonic-rs |
|---|---|---|---|---|
| owned | 309 | 312 | 293 | 378 |
| borrowed | 362 | 352 | 337 | 444 |
| partial, 2 of 7 | 503 | **561** | 530 | 543 |

**Neutral where the string is wanted, +11.5% where it is skipped** — and
partial deserialization goes from our weakest case to the fastest measured,
ahead of both `serde_json` and sonic-rs. That is exactly the shape the
change predicts: if you need the string you must look at its bytes either
way, but validating a string you are about to discard was pure waste.

No `unsafe` was added. The simple path calls `str::from_utf8` rather than
asserting the ASCII property it just proved; std's ASCII path is a word at
a time, and the measurement above already includes that cost.

### A pool bug the cross-checks found

Adding `direct` meant the fuzzer compared three implementations instead of
one, and it found that `[-92233720368547758080]` deserialized to `-0.0`
through the pool.

The literal is 2^63 × 10, which is exactly 0 modulo 2^64, so the pool's
wrapping accumulator landed on zero and the `-0` special case claimed it.
The digit-count guard that would have caught the overflow ran immediately
afterwards. Fixed by requiring `digits == 1`; leading zeros are already
rejected, so the only single-digit literal that can reach `acc == 0` is a
real `0`. Pinned by
`tests/stream.rs::negative_overflow_that_wraps_to_zero`.

That is the second pool number bug these cross-checks have surfaced, after
`10000000000000000999` returning `1e19`.

### Two more optimisations that did not work

Both were predicted by the profile and both changed nothing, which brings
the tally to **six failed, two successful** (§12).

**Skip without converting.** Deserializing 2 of 7 fields, the ignored five
were being fully materialised — floats parsed, `String`s built — and then
discarded. Splitting number *validation* from number *conversion* so that a
skipped field only pays for the syntax check moved 4 MiB partial
deserialization from 436 to 432 MiB/s: nothing. The skipped fields on this
corpus are mostly strings, whose cost is the scan that correctness requires
anyway, not the conversion that was removed.

**Fusing the control-byte check.** RFC 8259 forbids raw bytes below 0x20 in
a string. Adding that as its own pass put `no_control_bytes` at **9.9%** of
self time in the profile — the third-largest entry. Folding the test into
the loop in `unescape` that already scans for backslashes and non-ASCII, so
it costs one more compare per byte instead of a second traversal, moved
throughput from 268 to 263 MiB/s: nothing, or slightly worse.

The lesson from §12 holds. A profiler attributes *self time*, and after
`lto="fat"` with `codegen-units=1` that attribution says little about what
removing the work would cost. A tight byte loop that vectorises is cheap
per byte however large its share of samples looks, and adding a lane to it
is not free. Only a measurement of the whole pipeline settles it.

### Partial deserialization (2 of 7 fields) — MiB/s, 4 MiB

| | |
|---|---|
| sonic-rs | **552** |
| serde_json | 526 |
| simd-json | 453 |
| vela | 235 |

Skipping via `skip_subtree` is an index walk with no byte scanning, which
sounds like it should win — but the pool still had to be *built* for all
seven fields first. Streaming parsers skip the bytes without ever
representing them.

### Serialize `Vec<Record>` — MiB/s, 4 MiB

| | |
|---|---|
| sonic-rs | **1007** |
| serde_json | 761 |
| vela | 717 |
| simd-json | 702 |

Output is byte-identical to `serde_json` (verified over 5 000 random
documents plus the full corpus).

### String escaping in isolation

Escaping is 62% of serialization self time, so it is measured separately:

| | clean | sparse escapes | dense escapes |
|---|---|---|---|
| vela | **9.24 GiB/s** | **1.85 GiB/s** | 424 MiB/s |
| serde_json | 2.19 GiB/s | 1.52 GiB/s | **686 MiB/s** |
| sonic-rs | 25.10 GiB/s | 2.23 GiB/s | 597 MiB/s |

4.2× ahead of `serde_json` on clean text after vectorising the escape
scanner (it was 1.6 GiB/s with a scalar OR-reduction). Still well behind
sonic-rs, which vectorises the copy as well as the search.

Relevant to Vela: **`emit_v2.vl` is documented as a "DualBuffer-backed O(n)
emitter" but calls `json_escape_string` (`common.vl:52-80`), which is a
per-byte string-concat loop.** Every string written through the "O(n)"
emitter is still escaped in quadratic time.

---

## 11. serde_json's default float parser is not correctly rounded

Found while building the differential tests. For `-12715.4527e-19`:

```text
correctly rounded (Python, str::parse, us) : -0x1.6e7f7b0ed8f08p-50
serde_json, default features               : -0x1.6e7f7b0ed8f09p-50
```

Across 20 000 randomly generated high-precision literals, `serde_json`
deviates from the correctly-rounded value on **5 096 of them (25%)**; the
strict parser here matches `str::parse` on all 20 000. `serde_json` ships a
`float_roundtrip` feature that fixes this, off by default.

Pinned down by `tests/serde_de.rs::float_precision_beats_serde_json_default`
rather than papered over — the differential tests allow 1 ULP of slack and
say why.

---

## 12. Validating the older numbers

`benches/cbaseline.rs` was found to report one measurement 2.5× wrong
(`docs/CBASELINE.md` §5): two independent causes, inlining drift between
closures in one LTO'd function, and cross-benchmark interference from
criterion's `iter_batched_ref` staging large inputs. That raised an obvious
question about §3–§5, which were measured before the fix.

They were checked properly rather than assumed. `git worktree` at the
original commit, the same benchmark run both ways:

| `parse_10mb/.../records`, original commit | shared process | isolated | Δ |
|---|---|---|---|
| `vela_faithful` | 611 MiB/s | 604 MiB/s | −1.2% |
| `serde_json_value` | 134 MiB/s | 133 MiB/s | −0.7% |
| `sonic_rs_value` | 587 MiB/s | 576 MiB/s | −1.8% |

**`benches/parse.rs` was never affected.** The recorded 606 MiB/s was
correct for the code at the time. So the entire 18–30% gain in the tables
above is real improvement from the Stage 1 work in §9, not a harness
correction.

Why `cbaseline.rs` and not `parse.rs`: the C-baseline benchmark stages a
`Padded` copy per `iter_batched_ref` batch *and* allocates a
`YyPool::new(len * 24)` — over 250 MB of churn per corpus at 10 MB inputs.
`parse.rs`'s simd-json staging is an order of magnitude less. The
interference was real but needed that much allocator pressure to appear.

Everything in §3–§5, §10 and `docs/CBASELINE.md`, `docs/TIERS.md`,
`docs/MEMORY.md` is now measured with one process per benchmark
(`tools/isolate.sh`), and every timed operation in `parse.rs` and
`cbaseline.rs` goes through an `#[inline(never)]` wrapper so codegen cannot
drift between contenders.

**Lesson worth keeping:** a 2.5× swing with no code change is not exotic. If
several allocation-heavy benchmarks share a process — especially across an
FFI boundary, where the other language brings its own allocator behaviour —
isolate them and verify with a known-good baseline before believing the
output.

---

## Reproducing

```bash
cargo test                          # 173 tests
cargo bench --bench tiers           # all four Vela tiers head-to-head
cargo bench --bench scan            # Stage 1: classifiers, phases, scanners
cargo bench --bench parse           # DOM parse vs serde_json / simd-json / sonic-rs
cargo bench --bench query           # parse + access patterns
cargo bench --bench structs         # struct de/ser + escaping
cargo bench --bench zerocopy        # jsonflat vs rkyv vs re-parsing
tools/isolate.sh parse '^parse_10mb/'  # one process per benchmark
tools/memprofile.sh records 10485760   # memory matrix
tools/cbench.sh c_parse_10mb           # vs yyjson / simdjson
cargo clippy --lib                  # enforces panic-freedom
cargo run --example typestate       # the unwrap-free demo
cargo run --release --example sizes # jsonflat buffer sizes
tools/profile.sh vela_de 200        # sampling profile with symbols
```

Test coverage: 129 tests, including ~60 000 differential cases against
`serde_json`, exhaustive verification of both classifier tables over all 256
bytes, 40 000 hostile `jsonflat` buffers, and 30 000 random-byte inputs
asserting panic freedom.
