# Code size on 32-bit ARM

How big is a program that parses JSON, on a target where that question
has consequences? Measured on `thumbv7em-none-eabihf` — a Cortex-M4F with
no operating system, no allocator unless the program supplies one, no
double-precision FPU and no 64-bit integer divide.

Reproduce with `tools/size.sh [z|s|3]`.

---

## The job

Every contender does the same thing: parse a fixed 268-byte buffer of
three seven-field records and sum an integer field. Same input, same
bare-metal shim, same bump allocator, same linker script, same
optimisation settings — so the difference between two binaries is the
JSON library.

## Method

`llvm-size -A`, summing `.text` and `.rodata`. Not file size: that
includes headers and debug sections that vary by toolchain.

**Linked, not compiled.** An rlib holds generic code uninstantiated, so
it has no size at all until monomorphisation and `--gc-sections` have
run. Comparing rlibs measures nothing, and comparing an rlib to a `.o` is
worse — a `.o` of yyjson is the whole library including the writer, while
a linked Rust binary is only what the job reaches. So the C side is
linked too, with `-ffunction-sections -fdata-sections --gc-sections`, and
only `yyjson_read_opts` survives out of yyjson's public API.

The C binaries link Rust's own `compiler_builtins` rlib. ARMv7-M needs
soft-float doubles and a long-division routine either way; using the same
copy keeps the choice of runtime out of the comparison.

yyjson is built with `-DYYJSON_DISABLE_NON_STANDARD`, as `build.rs` does
for the throughput benchmarks. It drops yyjson's comment/inf/nan
extensions, which `dacodec` does not have either, so setting it is what
makes the two parsers answer the same question.

The floor — the shim and nothing else — is 320 bytes for Rust and 324 for
C, and is subtracted.

---

## Results

`opt-level = "z"`, `lto = "fat"`, `codegen-units = 1`, `panic = "abort"`.
Bytes of `.text` + `.rodata`, with the floor subtracted.

| | total | Rust runtime | the library |
|---|---:|---:|---:|
| `dacodec::pull`, bytes only | **1 742** | 130 | 1 612 |
| `dacodec::write` | **5 896** | 3 686 | 2 210 |
| `dacodec::pull`, converting numbers | **24 486** | 19 496 | 4 990 |
| `serde_json` (`no_std` + `alloc`) | **29 964** | 13 479 | 16 485 |
| `dacodec` typed (`serde` + `alloc`) | **50 948** | 38 369 | 12 579 |
| yyjson (C, reader only) | **69 316** | 6 698 | 62 618 |
| simdjson | — | — | does not compile |
| sonic-rs | — | — | does not compile |

"Rust runtime" is `core` + `compiler_builtins` + `alloc`. It is broken
out because a hosted C binary gets the equivalent from libc and libgcc
and does not pay for it in its own `.text` — but here, nothing else
supplies it, so the total is what actually lands in flash.

### All three optimisation levels

| | `z` | `s` | `3` |
|---|---:|---:|---:|
| `pull`, bytes only | 1 742 | 2 058 | 5 228 |
| `write` | 5 896 | 4 196 | 4 640 |
| `pull` | 24 486 | 25 050 | 29 198 |
| `serde_json` | 29 964 | 32 520 | 59 760 |
| `dacodec` typed | 50 948 | 53 804 | 82 836 |
| yyjson | 69 316 | 79 402 | 113 822 |

Ordering is stable across all three. `write` is smaller at `s` than at
`z` because `z` declines to inline a copy loop and links
`compiler_builtins`' out-of-line `memmove` instead — 3 686 bytes of
runtime against 1 848.

---

## What the numbers say

### 93% of the `pull` binary is `core`'s float parser

`dacodec::pull` doing the whole job is 24 486 bytes. The same code
stopping at `Raw::bytes` — locating the field but not converting it — is
1 742. Everything in between is reached through `Raw::as_i64`, which goes
through `parse_number`, which can return an `f64`, which instantiates
`core`'s `f64::from_str`:

| | bytes |
|---|---:|
| `core::num::dec2flt::table::POWER_OF_FIVE_128` | 10 416 |
| `core::num::dec2flt::…::from_str` | 2 836 |
| `dec2flt` helpers (`lemire`, `DecimalSeq`) | ~1 700 |
| soft-float `__divdf3`, `__muldf3` | 1 696 |
| **`dacodec::pull` itself** | **2 544** |

That is the price of a correctly-rounded float parser on a chip with no
double-precision FPU. `docs/RESULTS.md` records that `serde_json`'s float
parser is *not* correctly rounded, by up to 2 ULP; this is the other side
of that ledger, and 13 KB of flash is a real thing to weigh against 2 ULP.

An integer-only accessor that never reaches the float path would cut a
`pull` binary to roughly 3 KB. It does not exist yet — see the end.

### `dacodec`'s typed path is bigger than `serde_json`'s, and it is not the parser

51 KB against 30 KB. But by attributed library code `dacodec` is the
*smaller* of the two — 12 579 bytes against 16 485. The 21 KB difference
is `core`: 32 803 bytes of it against `serde_json`'s 6 607.

Two causes, both avoidable in principle:

* `serde_json` ships its own float parser and never instantiates
  `core::dec2flt`, saving the 13 KB above.
* `dacodec` drags in `core::fmt`'s float *formatter* as well — 4 064 +
  3 358 bytes for `float_to_decimal_common_shortest` and `_exact`, plus
  1 296 for grisu's `CACHED_POW10`. Nothing in the crate formats a float
  deliberately; it arrives through `serde::de::Unexpected::Float`, whose
  `Display` is reachable from `Error::custom` because `direct` forwards
  typed scalars to `deserialize_any`. An error message nobody reads costs
  8.7 KB.

### yyjson is the largest thing here, which was not the prediction

The prediction recorded before measuring was "yyjson is famously
compact". On this target it is not: 69 KB at `-Oz`, more than twice the
`serde_json` binary and 2.8× the `pull` one.

The reason is visible in one symbol. `yyjson_read_opts` is **47 184
bytes** — a single function. yyjson marks its internals
`static_inline` (`__attribute__((always_inline))`), so `-Oz` does not get
a say, and the entire reader collapses into one enormous inlined state
machine. That is a deliberate design choice and it is why yyjson is fast;
on a 1 MB flash budget it is also 7% of the device.

`--gc-sections` is working — the writer, the mutable-document API and the
file API are all gone, and `yyjson_read_opts` is the only public symbol
left. This is the reader alone.

### simdjson does not compile for the target

```
fatal error: 'cassert' file not found
```

`simdjson.h` and `simdjson.cpp` include 38 C++ standard library headers —
`<string>`, `<vector>`, `<memory>`, `<iostream>`, `<thread>`,
`<mutex>` — and there is no C++ standard library for a bare-metal ARM
target. This is not a gap in the measurement; it is the measurement. The
attempt is re-run on every `tools/size.sh` invocation so the finding
stays true rather than becoming folklore.

### sonic-rs is `std`-only

It builds for `armv7-unknown-linux-gnueabihf`, so the ARM32 concern in
the original scoping was unfounded — its scalar fallback is fine on
32-bit ARM. It does not build for `thumbv7em-none-eabihf`: no `no_std`
support at all.

---

## Caveats

* **The per-crate breakdown is indicative, not exact.** With `lto =
  "fat"` a library function inlined into the harness is attributed to the
  harness. `tools/size_attr.py` reports the harness bucket as
  `(harness + inlined)` and reports the residual as `unattributed` rather
  than hiding it. What is exact is the total and the runtime/library
  split.
* **`.rodata` includes the 268-byte input**, in every binary, and is
  subtracted with the floor.
* **The three-record input is small on purpose.** Code size is the
  question; a larger input would grow `.rodata` by the same amount
  everywhere and change no ordering.
* **`dacodec::pull` and yyjson do not do the same amount of work.**
  yyjson builds a full random-access DOM; `pull` streams and extracts.
  The comparable row for yyjson is `dacodec` typed (51 KB against 69 KB),
  and even that is not exact — yyjson leaves you a document you can query
  again.
* **Only `.text` and `.rodata`.** `.bss` is dominated by the 64 KB bump
  arena, which is the harness's choice, not any library's.

---

## Follow-up this measurement argues for

An integer-only number accessor on `pull` — `as_i64` that rejects a
fraction rather than parsing one — would take a `pull` binary from 24 KB
to about 3 KB for the very common case of reading integer fields off a
sensor feed. The measurement above is the argument; the API is not
written.
