# `no_std`

`--no-default-features` builds this crate for a target with no operating
system and no allocator. What survives is the part worth having there:
reading fields out of a JSON document, and writing one into a buffer the
caller supplies.

```toml
dacodec = { version = "0.1", default-features = false }
```

`tools/check-features.sh` builds every combination below, plus
`thumbv7em-none-eabihf` (bare-metal Cortex-M4F, no allocator) and
`armv7-unknown-linux-gnueabihf` (32-bit, full `std`).

---

## The three tiers

### `--no-default-features` — neither an allocator nor an OS

| module | what it is |
|---|---|
| `pull` | pull named fields out of a document, skip the rest |
| `write` | append JSON into a `&mut [u8]` |
| `stream::Error` | the error type both report, and the number/literal readers |
| `scan` | the byte classifiers (not the index — see below) |
| `scalar::skip_ws` | whitespace skipping |
| `tag` | the node type tags |
| `unescape::borrow_str` | a string as a subslice, when it can be one |
| `stream::Integer` | the widths `pull::Raw::as_int` and `as_fixed` read into |
| `flat`'s readers | `View`, `Ref`, `TypedView`, `StrList` — see below |

Nothing here allocates, on any path, including the error path —
`tests/error_alloc.rs` holds it to a literal zero across 19 malformed
inputs.

The only external dependency is `zmij`, which is itself `no_std` and
formats floats into a stack buffer. It is not optional, because `write`
needs it.

### `--features alloc` — an allocator, but no OS

Adds `pool`, `query`, `builder`, `workspace`, `strict`, `corpus`, the
`flat` *builders*, `unescape`'s expanding path, and `scan`'s
`StructuralIndex`.

#### Why `flat` is split rather than gated

Reading a `jsonflat` buffer needs no allocator and never did: `View::new`
validates a 32-byte header, and every accessor after it is arithmetic on
a borrowed slice. Building one does, because the output length is not
known until the walk is done. So the module is available with no
features and the builders inside it are gated, rather than the whole
module being gated on `alloc` because it happens to contain both.

Build on a host, read on a device. It is the cheapest read path in the
crate — 2 118 bytes linked for random access to a whole document, where
`pull` costs 2 102 for one named field per pass — and the buffer can come
from `mmap`, from flash, or from a `&'static [u8]` in the image.

### `--features std` — an OS

Adds `memstat`, `api::from_reader`, and the `cbench` and `fuzzing`
harnesses. That is all: `std` buys strikingly little, which is the
finding.

### `--features serde` — the typed API

`api`, `de`, `ser`, `direct`, and `stream`'s deserializer. Implies
`alloc`, and turns on `serde` itself with `default-features = false` so a
`no_std` build does not drag `std` back in through it.

---

## Decisions worth recording

### `core::error::Error`, not a `#[cfg]`

`std::error::Error` has been a re-export of `core::error::Error` since
Rust 1.81, so every `impl` in the crate names the `core` one and none of
them is gated. A `no_std` user gets `Error` impls; a `std` user cannot
tell the difference.

### The error message type

`serde::de::Error::custom` takes `T: Display`, so its text only exists at
the moment it is formatted. Every other message in the crate is a string
literal. `src/errmsg.rs` is that split: `Msg::Static(&'static str)`, plus
`Msg::Custom(Box<str>)` when `alloc` and `serde` are both on. Without an
allocator a custom message collapses to `"custom error"` — lossy in the
text of an error, not in the verdict.

### `stream` is two modules

`stream::Error` and the number readers are shared by `pull`, `direct` and
the deserializer, and need nothing. The deserializer needs `serde` and,
through the structural index, an allocator. They are `src/stream/mod.rs`
and `src/stream/deserializer.rs` respectively, which is what lets `pull`
build for a target with no heap while reporting errors that read
identically to the deserializer's.

### Stage 1 still needs an allocator

`StructuralIndex` is a `Vec<u32>` whose length is not known until the
scan has run. Every *classifier* under `src/scan/` is allocation-free —
`classify`, `find_escaped`, `prefix_xor16`, the 256-entry tables, the
NEON and x86 paths — but the drivers that write into an index are gated
on `alloc`.

A sink trait (`fn push(&mut self, u32)`) would let a caller supply a
fixed `[u32; N]` and make Stage 1 usable with no heap. It has not been
done because the embedded surface is `pull` and `write`, and neither uses
the index: `pull` scans bytes directly, precisely so that it does not
need one. Doing it would put a virtual call, or another monomorphisation
axis, on the hottest loop in the crate to serve a caller that does not
exist yet. Note also that `StructuralIndex::write_bits` relies on
`Vec::reserve` plus `spare_capacity_mut` for its unrolled over-write, so
a fixed-buffer sink would have to honour the same `SPILL = 32` headroom
contract.

### Reading a number should not require being able to read a float

`Raw::as_i64` accepts an integral float, so it goes through
`parse_number`, so it instantiates `core`'s `f64::from_str`, so a program
that only reads integers links 22 KB of float parser and soft-float
arithmetic. `Raw::as_int::<T>()` reads the digit range that
`number_syntax` already found, accumulates in `T`, and refuses anything
with a fraction or an exponent.

The width is a type parameter rather than a Cargo feature, and
`docs/SIZE.md` has the measurement for why: a generic instantiated once
compiles to the same thing a feature would, and a feature could not
express two widths in one program at all — while being additive and
global, so a crate anywhere in the tree could silently narrow another
crate's integers.

### Reading a fraction should not require it either

`as_int` refuses `1.5`, which left a device quoting decimals with no
option but the 22 KB. `Raw::as_fixed::<T>(scale)` reads `12.34` as
`1234`: the same digit loop with the decimal point moved, truncating
towards zero at the scale and refusing exponents. 2 214 bytes against
24 486 through `as_f64`.

`scale` is a plain argument and not a const generic — the one place in
the crate where the width argument does *not* carry over. Narrowing a
width removes arithmetic; a scale removes nothing, so a const generic
would buy only monomorphisations. Measured: identical at one call site,
664 bytes worse at three.

Carrying the fraction range out of `number_syntax` is not free for the
paths that ignore it: `as_int` grew 32 bytes and the typed path 40. That
is the price of the struct it returns growing a field, it was minimised
by measurement rather than reasoning — two narrower shapes were tried
and both were worse — and it is recorded in `docs/SIZE.md` rather than
quietly absorbed.

### `f64::fract` and `f64::abs` are in `std`, not `core`

`pull::Raw::as_i64` accepts an integral float. It used to say
`v.fract() == 0.0 && v.abs() < 2f64.powi(63)`; it now round-trips through
`i64` and compares, which says the same thing without the float methods.
The bound is still needed: `i64::MAX as f64` rounds *up* to 2^63, so a
value of exactly 2^63 would saturate and appear to round-trip.

### An error message was linking a float formatter

serde's default `Error::invalid_type` prints the offending value with
`Display`, and `Unexpected::Float`'s `Display` uses `{}` on an `f64`,
which instantiates `core::fmt`'s shortest-round-trip float formatter —
11 848 bytes on a chip with no double-precision FPU. `errmsg::Unexpected`
formats it with `zmij` instead, which is already a dependency and uses a
stack buffer. `serde_json` does the same thing, so the text is unchanged;
`tests/serde_de.rs` holds it to `serde_json`'s exact wording.

### `HashMap` where there is one

The `flat` builders intern strings in a map. `std::collections::HashMap`
when `std` is on, `alloc::collections::BTreeMap` otherwise
(`flat::Interner`). Only the builders touch it and only once per key —
reading a flat buffer never does — but building is measured
(`docs/ZEROCOPY.md`), so the faster map is kept where it exists rather
than moving everyone to `BTreeMap` for the benefit of the target that has
no choice.

### `i128` in the serializer

`serialize_i128`/`serialize_u128` used `write!` through
`std::io::Write` on a `Vec<u8>`. Those two lines were the only reason the
serializer needed an OS. They are now a digit loop in the same shape as
the `u64` one.

### `unescape` conflates two failures

`unescape::borrow_str` returns `None` both for "contains an escape, so
expanding it needs a buffer" and for "not valid UTF-8". Without an
allocator there is nothing useful to do with the difference, and the
failure is in the safe direction: a caller treating `None` as a rejection
rejects a well-formed escaped string rather than accepting a malformed
one. `pull::Raw::bytes` still hands over the raw extent.

---

## Threads

Nothing in the crate is shared mutable state, so nothing in it needs a
lock, and every public type is `Send + Sync` (`tests/thread_safety.rs`
asserts it at compile time and then stress-tests it under
ThreadSanitizer). That matters for `no_std` for one specific reason:
which SIMD kernel Stage 1 uses is chosen by `target_feature` at compile
time, not by a cached runtime probe. There is no `Once`, no atomic, and
no lazy initialisation to make work without `std`.

## What an rlib build does not prove

`cargo build --target thumbv7em-none-eabihf` on a library produces an
rlib, so nothing is linked. That is still the check that matters for
`no_std` — without `extern crate alloc`, the name `Vec` does not resolve
at all, so a stray one is a compile error whether or not it is ever
instantiated. What it cannot show is what a *linked* binary drags in.
That is what the size comparison measures.
