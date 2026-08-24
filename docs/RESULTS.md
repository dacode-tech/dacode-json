# Results

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
| records | 606 | 366 | 123 | **662** | 240 | 571 |
| int_array | **516** | 387 | 307 | 483 | 261 | 515 |
| strings | **1 963** | 439 | 251 | 568 | 517 | 709 |
| geo_int | **383** | 266 | 97 | 368 | 114 | 322 |
| geo_float | 593 | 301 | 172 | **598** | 204 | 503 |

1 MiB numbers are within 5% of these except `simd_json_tape/int_array`
(582 vs 483) and `serde_json/records` (131 vs 123), where cache effects
favour the smaller input.

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
| `vela_faithful`, reused workspace | **73 ns** | 76× faster |
| `simd_json::to_tape` | 116 ns | |
| `vela_faithful`, fresh allocation | 135 ns | 41× faster |
| `vela_strict`, reused workspace | 155 ns | |
| `sonic_rs::Value` | 166 ns | |
| `serde_json::Value` | 322 ns | |
| *Vela `t860` measurement* | *5 590 ns* | *baseline* |
| *yyjson (`t860`)* | *104 ns* | |
| *simdjson (`t860`)* | *72 ns* | |

Vela's 5 590 ns was three `mmap` calls, not parsing. Reusing a `Workspace`
puts the port at 73 ns — level with the C libraries Vela was measured
against, and it makes `json_parse_inline`'s 96 KB static BSS buffer (task J2)
unnecessary. Even the *fresh allocation* path is 41× faster than Vela,
because `Vec` uses the allocator rather than going to the kernel.

---

## 5. Query workloads

Raw DOM throughput flatters lazy representations, so these measure parse plus
a realistic access pattern (4 MiB of records).

**Sum one integer field from every record** — MiB/s:

| | 256 KiB | 4 MiB |
|---|---|---|
| sonic-rs `Value` | **610** | **511** |
| vela faithful | 492 | 487 |
| vela strict | 324 | 322 |
| sonic-rs lazy iter | 260 | 263 |
| serde_json `Value` | 146 | 126 |

**Read every field, decoding strings** — MiB/s:

| | 256 KiB | 4 MiB |
|---|---|---|
| vela faithful | **386** | **389** |
| vela strict | 274 | 277 |
| serde_json `Value` | 145 | 134 |

The pool holds its advantage when everything is read, because decoding on
demand into a `Cow<str>` still borrows for the (common) escape-free case.

**One field from the first record** — the case that exposes the limit of the
whole approach:

| | time |
|---|---|
| sonic-rs lazy pointer | **54 ns** |
| vela faithful | 421 µs |
| serde_json `Value` | 1 684 µs |

7 800× — because `sonic_rs::get(bytes, pointer![0, "name"])` never builds a
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

## Reproducing

```bash
cargo test                          # 129 tests
cargo bench --bench scan            # Stage 1: classifiers, phases, scanners
cargo bench --bench parse           # DOM parse vs serde_json / simd-json / sonic-rs
cargo bench --bench query           # parse + access patterns
cargo bench --bench structs         # struct de/ser + escaping
cargo bench --bench zerocopy        # jsonflat vs rkyv vs re-parsing
cargo clippy --lib                  # enforces panic-freedom
cargo run --example typestate       # the unwrap-free demo
cargo run --release --example sizes # jsonflat buffer sizes
tools/profile.sh vela_de 200        # sampling profile with symbols
```

Test coverage: 129 tests, including ~60 000 differential cases against
`serde_json`, exhaustive verification of both classifier tables over all 256
bytes, 40 000 hostile `jsonflat` buffers, and 30 000 random-byte inputs
asserting panic freedom.
