# dacodec

Fast, allocation-light JSON for Rust — a drop-in `serde_json` replacement
for the typed path, plus a zero-copy wire format for data you read more than
once.

```toml
[dependencies]
dacodec = "0.1"
```

```rust
use serde::{Serialize, Deserialize};

#[derive(Serialize, Deserialize)]
struct Config { name: String, port: u16 }

let cfg: Config = dacodec::from_str(r#"{"name":"edge","port":8080}"#)?;
let json = dacodec::to_string(&cfg)?;
```

Change `serde_json::` to `dacodec::` and nothing else. Output is
byte-identical, verified over the whole test corpus.

---

## Why

| | dacodec | serde_json |
|---|---|---|
| DOM build, 10 MiB | **419 MiB/s** | 132 MiB/s |
| validate, 1 MiB | **2.47 ms** | 7.22 ms |
| string escaping, clean text | **9.24 GiB/s** | 2.19 GiB/s |
| DOM memory, 10 MiB | **43.4 MiB** | 97.1 MiB |
| deserialize to structs, 4 MiB | **331 MiB/s** | 293 MiB/s |
| …with `from_slice_ascii` | **366 MiB/s** | — |
| deserialize borrowing structs | **352 MiB/s** | 337 MiB/s |
| deserialize 2 of 7 fields | **631 MiB/s** | 526 MiB/s |
| serialize from structs, 4 MiB | 717 MiB/s | **761 MiB/s** |
| RFC 8259 (JSONTestSuite) | 284/284 | 284/284 |
| float parsing | correctly rounded | off by ≤2 ULP on 17.7% of high-precision literals |

Read that table honestly. `dacodec` edges ahead of `serde_json` on
deserialization — 6% owned, 4% borrowing, 6% skipping most fields — and
uses the same memory to the allocation. It is still **behind on
serialization**, and `sonic-rs` is well ahead on the first two (378 and
444 MiB/s) by spending 236 `unsafe` blocks to get there. Single-digit
margins on one corpus and one machine: pick on features, not on 6%.

dacodec wins where a document is *inspected* rather than converted: DOM
construction (3.2×), validation (2.9×), escaping (4.2×), memory (2.2×) —
and by three to six orders of magnitude on anything you read more than once,
via the zero-copy format below.

All figures: Apple M-series, `lto="fat"`, one process per benchmark.
Reproduce with `tools/isolate.sh`. Full tables, including the corpora where
these reverse: [`docs/RESULTS.md`](docs/RESULTS.md).

## On-demand: pull fields, skip the rest

`Deserialize` cannot say "give me two of these forty fields and stop
looking" — serde must yield every key so the derived matcher can reject the
unknown ones. `dacodec::pull` is the other shape, and it **allocates
nothing**:

```rust
let mut total = 0i64;
dacodec::pull::select(json, &[b"id", b"score"], |got| {
    total += got[1].and_then(|v| v.as_i64()).unwrap_or(0);
    Ok(())
})?;
```

Summing one field across 4 MiB of records: **778 MiB/s**, against 656 for
the serde path and 538 for `serde_json` into structs. Selecting the *first*
field of seven reaches 998 MiB/s, because the rest of the record is skipped
by counting braces rather than parsed.

**It validates less, on purpose.** Content in the skipped tail is not
checked: `[{"a":1,"b":01}]` is rejected by `from_slice` and accepted here.
That is the same trade simdjson's On-Demand API makes, and it is why this is
a separate function rather than a quiet speed-up of `from_slice`. Call
[`validate`] first if you need both.

Nothing here allocates — no index, no pool, no `String`, and nothing on the
error path either — so it is also the path to use where there is no
allocator at all. See [`no_std`](#no_std) below.

### Against the C and C++ libraries

The engines this was modelled on, measured in-process through FFI
(`--features cbench`). Summing one integer field across every record of a
4 MiB document — parse plus read, which is what a caller actually does:

| | MiB/s |
|---|---|
| simdjson On-Demand (C++) | **1 608** |
| yyjson (C) | ~1 010 |
| **dacodec `pull`** | **778** |
| dacodec `from_slice` (serde) | 656 |
| serde_json, into structs | 538 |
| serde_json, into `Value` | 122 |

And raw DOM construction, 1 MiB of records: yyjson 1.08 GiB/s, yyjson with
a reused pool 1.27 GiB/s, simdjson DOM 1.10 GiB/s, simdjson On-Demand
1.87 GiB/s, against our validating parser at 417 MiB/s and the
non-validating port at 753 MiB/s.

**simdjson is 2.1x ahead and yyjson about 1.3x.** They are C and C++, they
have had far more work put into them, and simdjson On-Demand skips whole
subtrees without materialising them. This crate is faster than every Rust
alternative measured on the typed path and slower than both C libraries on
theirs; both statements are worth knowing before choosing.

Full numbers, methodology and caveats: [`docs/RESULTS.md`](docs/RESULTS.md).

---

## Using it

### Owned types

```rust
let cfg: Config = dacodec::from_str(text)?;
let cfg: Config = dacodec::from_slice(bytes)?;
let cfg: Config = dacodec::from_reader(file)?;

let s: String   = dacodec::to_string(&cfg)?;
let v: Vec<u8>  = dacodec::to_vec(&cfg)?;
dacodec::to_writer(&mut buffer, &cfg)?;   // reuses the buffer
```

### Borrowing types — no copying

Strings borrow from the input whenever they contain no escapes:

```rust
#[derive(Deserialize)]
struct Row<'a> {
    id: u64,
    #[serde(borrow)] name: &'a str,
    #[serde(borrow)] tags: Vec<&'a str>,
}

let mut p = dacodec::Parser::new();
let row: Row<'_> = p.deserialize(bytes)?;   // row.name points into `bytes`
```

A borrowing type needs `Parser` rather than `from_str`, because the node
pool must outlive the borrow and a free function cannot express that
without leaking it.

### Parsing many documents

`from_str` allocates a parser per call. Reuse one and a steady-state parse
allocates **nothing**:

```rust
let mut p = dacodec::Parser::with_capacity(64 * 1024);
for line in lines {
    let doc = p.parse(line)?;
    if let Some(v) = doc.root().get("status") {
        println!("{:?}", v.as_i64());
    }
}
```

### Errors carry a position

```rust
match dacodec::from_str::<Config>(bad) {
    Err(e) => println!("{e} (byte {:?})", e.offset()),   // "expected ':' at byte 11"
    Ok(_)  => {}
}
```

---

## Zero-copy: `dacodec::flat`

For data you write once and read many times — caches, mmapped files, RPC
payloads — parsing at every read is wasted work. `flat` stores a document in
a self-contained buffer that is read directly.

```rust
use dacodec::flat::{self, View};

// once, on the writer side
let mut p = dacodec::Parser::new();
let buf = flat::encode(p.parse(json)?)?;      // store or send `buf`

// many times, on the reader side
let view = View::new(&buf)?;                   // O(1), no parsing
let name = view.root().at(0).and_then(|r| r.get("name"));
```

### Schema-driven, when both ends know the type

Field positions become compile-time constants, so key strings are never
stored and a read is a load at a fixed offset:

```rust
use dacodec::flat_struct;
use dacodec::flat::typed::{TypedView, TypedWriter, de};

flat_struct! {
    pub struct Record : RecordFields {
        id: u64, age: u32, active: bool, score: i64,
        name: str, city: str, tags: [str],
    }
}

let mut w = TypedWriter::<Record>::new();
w.record().u64(1).u32(30).bool(true).i64(-5)
         .str("alpha").str("london").str_list(["x", "y"]);
let buf = w.finish();

let v = TypedView::<Record>::new(&buf)?;
assert_eq!(v.name(0), Some("alpha"));          // borrowed, no copy

// or straight into a struct
let rows: Vec<Row<'_>> = de::from_all(&v)?;
```

The layout is chosen by **which type you construct**, not a Cargo feature —
features are additive and global, so one crate enabling the other mode would
silently switch every crate over. The header records which layout was
written plus a hash of the field names and types, so reading a buffer with
the wrong schema is `Err(SchemaMismatch)`, never silent garbage.

### What it costs and what it buys

Reading a **10.4 MiB** document:

| | allocations | bytes allocated |
|---|---|---|
| `flat::typed` | **2** | **36 B** |
| `flat` dynamic | **1** | **18 B** |
| `serde_json` (must parse) | 1 310 661 | 82.99 MiB |

Reading one field from a 1 MiB document:

| | time | size on disk |
|---|---|---|
| `flat::typed` | **2.49 ns** | 0.67× the JSON |
| `flat` dynamic | 13.6 ns | 1.48× |
| `rkyv` | 1.02 ns | 0.59× |
| `serde_json` (re-parse) | 7.06 ms | 1.00× |

So: **2.49 ns versus 7.06 ms**, at 0.67× the size — because nothing is
parsed, only addressed.

The trade: encoding costs more (848 µs per MiB, against `serde_json`'s
1.30 ms — so encoding is actually cheaper, but you must do it up front),
and the format is a fixed binary layout, not text anyone can read.
**`rkyv` is faster and smaller still** (1.02 ns, 0.59×) because it has a
compile-time schema and stores no type tags. Use `rkyv` if your data never
arrives as JSON. Use this if it does.

Details, including where this loses: [`docs/ZEROCOPY.md`](docs/ZEROCOPY.md).

---

## Examples

```bash
cargo run --example readme              # every snippet on this page
cargo run --example ndjson              # newline-delimited logs, borrowing
cargo run --example query               # inspect a document without a struct
cargo run --release --example zero_copy # the flat format, and why
```

| | |
|---|---|
| [`readme.rs`](examples/readme.rs) | every snippet on this page, compiled and asserted — including that a borrowed `&str` really points into the input |
| [`ndjson.rs`](examples/ndjson.rs) | one document per line: borrowing structs, reading two fields of many, recovering from a bad line |
| [`query.rs`](examples/query.rs) | walking a shape you do not control, reusing a `Parser`, validating |
| [`zero_copy.rs`](examples/zero_copy.rs) | `flat` and `flat::typed`, schema mismatch, corruption, and a re-read timing |

A README example that does not compile is worse than none, so they are all
real programs rather than fragments.

## Running the tests

```bash
cargo test                       # 202 tests
cargo test --features vela-compat  # + the reference tiers          (260)
cargo test --features fuzzing    # + the bounded fuzzer             (262)
cargo test --all-features        # + the C baselines (needs a C/C++ compiler)
cargo test --doc                 # the examples in the API docs
cargo run --example readme       # the examples on this page
cargo clippy --all-targets
tools/check-features.sh          # every feature combination + two cross targets
```

Clippy is clean at every feature combination, not just the default one, and
`cargo doc` raises no warnings — there are no broken intra-doc links.

What is covered:

| suite | what |
|---|---|
| `conformance_suite.rs` | JSONTestSuite (319 files), JSON_checker, encodings, number corpus |
| `edge_cases.rs` | the categories yyjson's and simdjson's unit tests cover |
| `serde_de.rs` / `serde_ser.rs` | differential against `serde_json`, byte-identical output |
| `flat.rs` / `flat_typed.rs` | round-trip plus 40 000 corrupted buffers |
| `onepass.rs` | two independent parsers must produce identical output |
| `stream.rs` | the streaming deserializer against `serde_json` and the pool, on all 351 corpus files |
| `tier_contract.rs` | the reference implementations against each other (`vela-compat`) |
| `fuzz_bounded.rs` | a deterministic slice of the mutation fuzzer |

Test data under `testdata/` is vendored with provenance and licences — see
[`testdata/README.md`](testdata/README.md). Nothing is downloaded at test
time.

### Benchmarks

```bash
cargo bench --bench parse       # vs serde_json, simd-json, sonic-rs
cargo bench --bench structs     # struct de/ser and escaping
cargo bench --bench zerocopy    # flat vs rkyv vs re-parsing
cargo bench --all-features --bench cbaseline   # vs yyjson and simdjson (C)

tools/isolate.sh parse '^parse_10mb/'   # one process per benchmark
```

Use `tools/isolate.sh` for anything you intend to quote. Running many
allocation-heavy benchmarks in one process moved one measurement here by
**2.5×** with no change to the code under test;
[`docs/RESULTS.md` §12](docs/RESULTS.md) records how that was found and
ruled out.

---

## Fuzzing

Two harnesses, because they answer different questions.

### Stable — works everywhere, no nightly

```bash
cargo run --release --features fuzzing --bin fuzz              # 100k iterations
cargo run --release --features fuzzing --bin fuzz -- 42 5000000
cargo test --features fuzzing --test fuzz_bounded              # CI slice
```

Seeds from `testdata/` and mutates with a JSON-aware operator set, so
mutations stay *nearly* valid — about 4.8% remain parseable, which is where
the interesting states are. Random bytes almost never get past the first
rejection branch.

Deterministic: a failure prints a `case_seed` that replays it exactly with
`fuzz <case_seed> 1`.

Runs on Linux, macOS and Windows, on stable, with no sanitizer.

### libFuzzer — coverage-guided, needs nightly

```bash
rustup toolchain install nightly
cargo install cargo-fuzz

cargo +nightly fuzz run differential
cargo +nightly fuzz run roundtrip
cargo +nightly fuzz run flat_view
```

| target | checks | upstream analogue |
|---|---|---|
| `differential` | everything below, against `serde_json` | yyjson `fuzzer.c`, simdjson `fuzz_parser` |
| `roundtrip` | serialize → parse → serialize is a fixed point | simdjson `fuzz_minify` |
| `flat_view` | hostile bytes through the zero-copy reader | simdjson `fuzz_padded` |

**Platform notes.** `cargo-fuzz` uses libFuzzer, which needs a compiler
runtime that ships with LLVM:

* **Linux** — works out of the box. ASan/UBSan via `--sanitizer address`.
* **macOS** — works; if linking fails, `xcode-select --install`. Apple
  Silicon needs no special flags.
* **Windows** — libFuzzer support is limited. Use WSL, or run the stable
  fuzzer, which is fully supported there.
* **Docker/CI** — `cargo +nightly fuzz run differential -- -max_total_time=300`
  gives a bounded run suitable for a pipeline.

`fuzz/` is its own workspace, so a plain `cargo test` never tries to build
it without nightly.

### What the fuzzers check

Not merely "does it crash":

* `strict` agrees with `serde_json` on validity;
* accepted documents deserialize to the same value (2 ULP float slack — the
  measured worst case for `serde_json`'s float parser);
* the single-pass and index-fed parsers produce byte-identical output;
* all nine structural scanners agree on valid documents;
* serialize → re-parse preserves the value;
* a `flat` buffer just built passes its own deep validation;
* nothing panics on any input.

---

## Guarantees

**No panics.** No `unwrap`, `expect`, `panic!` or slice indexing exists on
any path reachable from the public API. Enforced by
`#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic,
clippy::indexing_slicing)]` and checked by fuzzing.
[`docs/UNWRAP_FREE.md`](docs/UNWRAP_FREE.md) explains how far that idea
generalises and where it stops.

**No allocation in steady state.** A reused `Parser` allocates once and
resets.

**Correctly-rounded floats.** Every literal parses to the same `f64` as
`str::parse`. `serde_json`'s default parser does not — measured over 200 000
high-precision literals it deviates on 17.7%, by up to 2 ULP.

**Exact integers.** Values up to `u64::MAX` keep every digit, including the
20-digit ones that do not fit `i64`.

**Unsafe.** Six blocks, all in SIMD kernels or one `Vec::set_len` after a
`reserve`, each with a `SAFETY` comment stating the invariant.

---

## Features

| feature | default | what |
|---|---|---|
| `std` | ✅ | an operating system: `from_reader`, and the measurement machinery |
| `serde` | ✅ | the public API; implies `alloc` |
| `alloc` | | an allocator, but no OS: the node pool, `strict`, the `flat` builders |
| `vela-compat` | | the Vela tier ports (`tiers`, `onepass`) — see below |
| `fuzzing` | | the differential fuzz harness (implies `vela-compat`) |
| `cbench` | | vendored yyjson and simdjson, for benchmarking (needs a C/C++ compiler) |
| `profiling` | | the CPU and memory profiling binaries |

### `no_std`

`--no-default-features` builds for a bare-metal target with no operating
system and no allocator:

```toml
dacodec = { version = "0.1", default-features = false }
```

What survives is `pull` (read fields out of a document), `write` (build one
into a `&mut [u8]`), the shared error type, and the byte classifiers.
Nothing in that set allocates, on any path — `tests/error_alloc.rs` holds
the error path to a literal zero across 19 malformed inputs.

`tools/check-features.sh` builds every combination, plus
`thumbv7em-none-eabihf` (Cortex-M4F, no allocator) and
`armv7-unknown-linux-gnueabihf` (32-bit, full `std`), so a `std`-ism cannot
creep back in unnoticed. [`docs/NOSTD.md`](docs/NOSTD.md) records what is in
each tier and the decisions behind the split.

### Code size

Linked for bare-metal Cortex-M4F, `opt-level = "z"`, doing one job — parse
a fixed buffer and sum a field. `.text` + `.rodata`, floor subtracted:

| | bytes |
|---|---:|
| `pull`, locating fields only | **1 742** |
| `pull` + `as_int::<i32>` | **2 102** |
| `write` | **5 896** |
| `pull` + `as_i64` | 24 486 |
| `serde_json` (`no_std` + `alloc`) | 29 964 |
| `dacodec` typed (`serde` + `alloc`) | 43 468 |
| yyjson (C, reader only) | 69 316 |
| simdjson, sonic-rs | neither compiles for the target |

The gap between rows 2 and 4 is `core`'s correctly-rounded float parser —
22 KB, reached because `as_i64` accepts an integral float and so must be
able to parse one. `as_int::<T>()` reads the digits into the width you ask
for and refuses fractions, so it never links any of it:

```rust
dacodec::pull::select(json, &[b"t", b"h"], |got| {
    let temp: i16 = got[0].and_then(|v| v.as_int()).unwrap_or(0);
    let hum:  u8  = got[1].and_then(|v| v.as_int()).unwrap_or(0);
    Ok(())
})?;
```

Width is a type parameter and not a Cargo feature, for the same reason
`flat`'s layout is: features are additive and global. It costs nothing —
a generic instantiated once is 2 422 bytes against 2 446 for a
hand-written non-generic equivalent, and each additional width in one
program is 394 bytes. Measured on 8-bit AVR and 16-bit MSP430 as well as
ARM32.

yyjson is the largest thing in the table because `yyjson_read_opts` is a
single 47 KB `always_inline` function. Method, caveats and the full
breakdown: [`docs/SIZE.md`](docs/SIZE.md). Reproduce with `tools/size.sh`
and `tools/width.sh`.

### A warning about `vela-compat`

That feature exposes the original Vela tier ports. They are kept for
measurement and are **not usable as parsers**: they do not validate (tier 1
returns the correct accept/reject verdict on 50.7% of the JSONTestSuite
cases, tier 2 on 62.7%) and they truncate floats, so `3.14` parses as the
integer `314`.

That is Vela's behaviour, reproduced deliberately so the comparison in
[`docs/CBASELINE.md`](docs/CBASELINE.md) is honest. It is off by default
because those tiers benchmark faster than the real parser, and a fast
number next to a familiar name is exactly how someone ends up shipping a
parser that accepts truncated input.

---

## Documentation

| | |
|---|---|
| [`docs/RESULTS.md`](docs/RESULTS.md) | all benchmarks, methodology, bugs found |
| [`docs/ZEROCOPY.md`](docs/ZEROCOPY.md) | the `flat` format, and where it loses |
| [`docs/MEMORY.md`](docs/MEMORY.md) | peak RSS and allocation counts, and why peak RSS is the wrong instrument for the read path |
| [`docs/PROFILING.md`](docs/PROFILING.md) | CPU hot paths; four optimisations that failed and why |
| [`docs/CBASELINE.md`](docs/CBASELINE.md) | measured against yyjson and simdjson in C |
| [`docs/TIERS.md`](docs/TIERS.md) | the reference implementations this began as |
| [`docs/UNWRAP_FREE.md`](docs/UNWRAP_FREE.md) | panic-free design study |
| [`docs/NOSTD.md`](docs/NOSTD.md) | the `no_std` split, and what `std` actually buys |
| [`docs/SIZE.md`](docs/SIZE.md) | code size on bare-metal ARM32, against yyjson, serde_json and simdjson |

---

## Origins

The parser began as a port of the JSON tiers in the Vela compiler's standard
library, themselves modelled on [yyjson](https://github.com/ibireme/yyjson)
(a flat node pool) and [simdjson](https://github.com/simdjson/simdjson) (a
SIMD structural index). Those ports are still here behind `vela-compat` and
are measured against the C originals in
[`docs/CBASELINE.md`](docs/CBASELINE.md) — yyjson remains 1.36× ahead on
DOM construction, and this crate is 2.8× ahead of it on string-heavy data
because it never decodes a string until asked.

The shipping parser is `strict`: the same representation with RFC 8259
validation and correctly-rounded numbers added.

---

## What is in the published crate

`vendor/` (11 MB of C/C++ baselines) and `testdata/` (4.2 MB of conformance
corpora) are development-only and are excluded, which takes the package from
14.1 MB to 125 KB compressed. Clone the repository to run the conformance
suite, the fuzzers, or the C baselines — with `--features cbench` the build
script says so rather than failing in the C compiler.

## Licence

Apache-2.0, see [`LICENSE`](LICENSE). Vendored test data and benchmark
baselines keep their own licences; see `testdata/README.md` and
`vendor/README.md`.

Built by [Dacode Tech](https://dacode.tech), Romania.
