# Code size on small machines

How big is a program that parses JSON, on a target where that question
has consequences? Measured on `thumbv7em-none-eabihf` — a Cortex-M4F with
no operating system, no allocator unless the program supplies one, no
double-precision FPU and no 64-bit integer divide — and, for the integer
width question, on 16-bit MSP430 and 8-bit AVR.

Reproduce with `tools/size.sh [z|s|3]` and `tools/width.sh`.

The short version: on a bare-metal target the JSON library is not what
costs. `dacodec::pull`'s own code is 2 544 bytes; the binary around it
was 24 KB, and 22 of those were `core`'s float parser, reached through
an accessor that had to be able to return an `f64`. `Raw::as_int::<T>()`
is the fix, and it takes the binary to 2 102 bytes.

---

## The job

Every contender does the same thing: parse a fixed 268-byte buffer of
three seven-field records and sum an integer field. Same input, same
bare-metal shim, same bump allocator, same linker script, same
optimisation settings — so the difference between two binaries is the
JSON library.

The `pull` rows differ only in which accessor reads the field, so the
difference between *those* is one method call.

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
| `pull`, locating fields only | **1 742** | 130 | 1 612 |
| `flat`, read-only | **2 118** | 858 | 1 260 |
| `pull` + `as_int::<i32>` | **2 134** | 122 | 2 012 |
| `pull` + `as_fixed::<i32>(3)` | **2 214** | 122 | 2 092 |
| `pull` + `as_int` at three widths | **2 928** | 144 | 2 784 |
| `write` | **5 896** | 3 686 | 2 210 |
| `pull` + `as_i64` | **24 486** | 19 496 | 4 990 |
| `serde_json` (`no_std` + `alloc`) | **29 964** | 13 479 | 16 485 |
| `dacodec` typed (`serde` + `alloc`) | **43 508** | 28 875 | 14 633 |
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
| `pull`, locating fields only | 1 742 | 2 058 | 5 228 |
| `flat`, read-only | 2 118 | 1 302 | 1 304 |
| `pull` + `as_int::<i32>` | 2 134 | 2 778 | 5 338 |
| `pull` + `as_fixed::<i32>(3)` | 2 214 | 2 882 | 5 492 |
| `pull` + `as_int`, three widths | 2 928 | 3 460 | 6 780 |
| `write` | 5 896 | 4 196 | 4 640 |
| `pull` + `as_i64` | 24 486 | 25 082 | 29 286 |
| `serde_json` | 29 964 | 32 520 | 59 760 |
| `dacodec` typed | 43 508 | 46 992 | 81 480 |
| yyjson | 69 316 | 79 402 | 113 822 |

Ordering is stable across all three, with one exception: `flat` is the
smallest thing in the table at `s` and `3` and third smallest at `z`,
because it is the only reader with no parser to inline — `z` declines to
inline its copy and links `compiler_builtins`' `memmove` (858 bytes of
runtime against 116), which the other two levels do not.

`write` is smaller at `s` than at `z` for the same reason, and more of
it: 3 686 bytes of runtime against 1 848.

---

## What the numbers say

### 93% of a `pull` binary was `core`'s float parser

`dacodec::pull` summing an integer field through `as_i64` is 24 486
bytes. The same code stopping at `Raw::bytes` — locating the field but
not converting it — is 1 742. Everything in between is reached because
`as_i64` goes through `parse_number`, which has to be able to answer
`f64`, which instantiates `core`'s correctly-rounded float parser:

| | bytes |
|---|---:|
| `core::num::dec2flt::table::POWER_OF_FIVE_128` | 10 416 |
| `core::num::dec2flt::…::from_str` | 2 836 |
| `dec2flt` helpers (`lemire`, `DecimalSeq`) | ~1 700 |
| soft-float `__divdf3`, `__muldf3` | 1 696 |
| **`dacodec::pull` itself** | **2 544** |

`Raw::as_int::<T>()` is the accessor that does not. It reads the digit
range `number_syntax` already found, accumulates in `T`, and refuses
anything with a fraction or an exponent — so it never reaches the float
parser, and never links it. **24 486 bytes to 2 134.**

### A fraction does not need a float either

`as_int` refuses `1.5`, which leaves a feed quoting decimals with no
option but the 22 KB. `Raw::as_fixed::<T>(scale)` is the third reading:
`12.34` at `scale = 2` is the integer `1234`. **2 214 bytes**, against
24 486 for the same field through `as_f64`.

There is no new machinery. `number_syntax` was already locating the
fraction digits in order to step over them, so returning where they are
costs nothing; scaling by `10^scale` is the same `checked_mul(10)` digit
loop with the decimal point moved, padding a short fraction with `b'0'`
and simply not draining the iterator on a long one.

The cost is on the other side of the ledger and should be recorded:
carrying the fraction range through `number_syntax` added **32 bytes to
`as_int`** (2 102 → 2 134) and 40 to the typed path, because the struct
it returns grew a field. Two shapes were tried and rejected by
measurement — a four-offset struct (+48 rather than +32, too wide for
the return registers) and caching `is_float` in the padding byte that
was already there (+36, because the derived compare folds into the
branch that follows it and a stored flag does not).

Truncation, not rounding: `12.345` at `scale = 2` is `1234`, towards
zero on both signs. Rounding would mean carrying a decision back through
the digits, which is the arithmetic being avoided, and a half-count bias
about zero is worse than a lost digit for a caller reading a sensor.

Exponents are refused. `1.234e2` would need a second, signed shift of
the point before any of this applies, and it is not a spelling that
devices emitting fixed-point data use.

### `scale` is an argument; width is a type

Width is a type parameter because narrowing it removes arithmetic (see
below). `scale` removes nothing, so a const generic would only add a
monomorphisation axis. Measured:

| | bytes |
|---|---:|
| `as_fixed::<i32>(3)`, runtime argument | 2 214 |
| `as_fixed::<i32, 3>()`, const generic | 2 214 |
| three scales, runtime argument | 2 358 |
| three scales, const generic | 3 022 |

At one call site they are byte-identical, because a literal argument
const-folds and the loop unrolls anyway. At three, the const generic
costs 664 bytes to say the same thing. This is the mirror image of the
width result: the same reasoning, run again, coming out the other way —
which is why it was measured rather than assumed.

`docs/RESULTS.md` records that `serde_json`'s float parser is *not*
correctly rounded, by up to 2 ULP. This is the other side of that
ledger: `dacodec` keeps `core`'s correct one, and 13 KB of flash is a
real thing to weigh against 2 ULP — so the answer is to let a caller
that does not need floats not pay for them, rather than to make the
float parser worse.

### `flat` is the cheapest way to read a whole document

2 118 bytes, of which 1 260 is the library, for random access to every
field of every record — where `pull` at 2 134 gives you one named field
per pass and nothing else.

That is not a parser, which is the point. The buffer is built on a host
and `View::new` validates a 32-byte header; every accessor after that is
arithmetic on a borrowed slice. So the readers need no allocator, and
`flat` is available with `--no-default-features` while its *builders*
stay gated on `alloc` — the output length is not known until the walk is
done.

The row carries 471 bytes of `.rodata` for the buffer itself, which the
other rows do not. That is data, not code: on a real device it is an
`mmap` or a flash address, not part of the image. `tools/size.sh` also
touches `INPUT` in this binary so the floor being subtracted is the same
floor as everywhere else.

### An `f32` accessor would not have helped

`f32::from_str` and `f64::from_str` share `POWER_OF_FIVE_128` in `core`,
so parsing as `f32` still links the 10 416-byte table. Measured: a probe
doing `str::parse::<f32>()` is 19 210 bytes against 24 486 for `f64` —
it saves the soft-float arithmetic and nothing else. Narrowing the float
is not the lever; not parsing one is.

### Width is a type parameter, and that costs nothing

The obvious way to expose "use a narrower integer on a small machine" is
a Cargo feature. It would be a mistake, and it is also unnecessary.

Unnecessary, because a generic instantiated once is exactly what a
feature compiles to. Measured, on ARM32:

| | bytes |
|---|---:|
| `as_int::<i32>()`, generic | 2 422 |
| hand-written non-generic `i32` path — what a feature would emit | 2 446 |
| `as_int` at three widths in one program | 3 210 |

Unlike the rest of this file these three are `.text` + `.rodata` before
the floor is subtracted, and all three were taken at one commit, before
`as_fixed` added 32 bytes to every `as_int` row. The middle row's probe
was written for the comparison and not kept, so the table is left as it
was measured rather than half-refreshed; the current figures are 2 454
and 3 248, and the gaps are what the argument rests on.

The generic is 24 bytes *smaller* at one width, which is codegen noise;
the point is that there is no monomorphisation penalty to pay unless the
choice is actually used, and then it is ~395 bytes per extra width.
A feature could not offer that choice at all.

A mistake, because Cargo features are additive and global. A crate
anywhere in the dependency tree turning on `num-i16` would silently
narrow every other crate's integers — truncating IDs in code that never
asked. `flat` refused the same bargain for the same reason; see the
README on why its layout is a type and not a feature.

### What width buys on 8- and 16-bit machines

`tools/width.sh`, on real tier-3 targets via `-Z build-std`. Bytes added
over a probe that locates the field and converts nothing:

| | `as_int::<i8>` | `<i16>` | `<i32>` | `<i64>` | `as_i64` |
|---|---:|---:|---:|---:|---:|
| ARM32 (Cortex-M4F) | +380 | +380 | +372 | +484 | +18 123 |
| MSP430 (16-bit) | +352 | +370 | +372 | +776 | +28 409 |
| AVR (8-bit) | +725 | +859 | +853 | +1 139 | +28 044 |

Three things fall out:

* **Not parsing floats is worth 18–28 KB on every target**, and more on
  the narrow ones — the 16-bit column pays 1.6× the 32-bit one for the
  same float parser.
* **Narrowing below the machine word buys nothing.** `i8`, `i16` and
  `i32` are within noise of each other everywhere, including on AVR,
  where a machine word is 8 bits.
* **`i64` is the only width that is genuinely dearer**, and by 112 bytes
  on ARM32, 404 on MSP430, 286 on AVR. Real, but two orders of magnitude
  below the float parser. Worth having the choice; not worth a Cargo
  feature to express it.

The whole crate builds `no_std` for both of these targets, which is the
other thing this table demonstrates.

8051 is absent because LLVM has no 8051 back end, so `rustc` has no
target for it. `rustc --print target-list` offers `avr-none` and
`msp430-none-elf` and nothing narrower.

### An error message nobody reads was costing 11 848 bytes

Nothing in the crate formats a float deliberately. `core::fmt`'s
shortest-round-trip float formatter was arriving anyway, through
`serde::de::Unexpected::Float`: serde's default `Error::invalid_type`
prints the offending value with `Display`, and `direct` forwards typed
scalars to `deserialize_any`, so a struct field typed `i64` meeting a
`1.5` reaches it.

Three ways out, measured:

| | bytes | the message |
|---|---:|---|
| serde's default `Display` | 50 948 | `floating point \`1.5\`` |
| drop the value | 39 100 | `floating point number` |
| **format it with `zmij`** | **43 468** | `floating point \`1.5\`` |

`zmij` is already a non-optional dependency — `write` needs it — it is
`no_std`, it formats into a stack buffer, and `serde_json` 1.0.151 uses
it for the same job, so the digits are identical. Keeping the text costs
4 368 bytes over dropping it, and buys back the claim on the front of the
README: a drop-in replacement whose errors read the same.
`tests/serde_de.rs::type_error_messages_match_serde_json` holds twelve
mismatches to `serde_json`'s exact wording.

That 4 368 bytes lands only on the typed path, which needs an allocator
anyway. The path that has to fit in flash is `pull`, and it never touches
any of this.

`crate::errmsg::Unexpected` spells out every arm rather than delegating
the rest to serde, as `serde_json` does. Delegating works only because
the optimiser can prove the float arm dead after the wrapper has handled
it; being explicit does not need that to hold.

### What is left, and why

`dacodec` typed is 43 KB against `serde_json`'s 30 KB. By attributed
library code `dacodec` is the *smaller* of the two — 14 621 bytes against
16 485 — so the gap is `core`, and it is the float parser again:
`serde_json` ships its own and never instantiates `core::dec2flt`.

That one is a deliberate trade, not an oversight.
`docs/RESULTS.md` records that `serde_json`'s float parser is not
correctly rounded, by up to 2 ULP. Matching its size would mean matching
that. The answer taken instead was to let callers who do not need floats
avoid them entirely, which is what `as_int` is.

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
  The comparable row for yyjson is `dacodec` typed (43 KB against
  69 KB), and even that is not exact — yyjson leaves you a document you
  can query again.
* **Only `.text` and `.rodata`.** `.bss` is dominated by the 64 KB bump
  arena, which is the harness's choice, not any library's.
* **`tools/width.sh` measures a `staticlib`, not a linked binary.** AVR
  and MSP430 have no linker here. That is sound for the question it
  asks — every column is the same crate compiled the same way with one
  entry point, and only the accessor differs, so the deltas are real. It
  would not be sound for comparing two different libraries, which is why
  `tools/size.sh` links.
* **AVR and MSP430 are tier 3.** They need nightly and `-Z build-std`,
  and neither is in `tools/check-features.sh`, so nothing stops them
  breaking silently.

---

## Follow-ups this measurement argues for

* **`serde_json` avoids `core::dec2flt` by shipping its own float
  parser.** The 13 KB is the price of a correctly-rounded one. A
  `pull`-shaped float accessor that reads a fixed-point value without
  ever building an `f64` would suit the sensor case, which mostly wants
  two decimal places rather than 17 significant figures.
* **`direct` forwards typed scalars to `deserialize_any`.** That is why
  a float can reach a visitor expecting an `i64` at all. Implementing
  `deserialize_i64` and friends directly would remove a whole class of
  error path, not just its message.
