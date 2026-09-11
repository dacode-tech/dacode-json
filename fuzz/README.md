# Fuzzing

Two harnesses, because they answer different questions.

## Stable — corpus mutation, runs anywhere

```bash
cargo run --release --features fuzzing --bin fuzz            # 100k iterations
cargo run --release --features fuzzing --bin fuzz -- 42 1000000
cargo test  --features fuzzing --test fuzz_bounded           # CI slice
```

Seeds from `testdata/` (JSONTestSuite), mutates, and checks every property
in `dacode_json::fuzz::check`. Fully deterministic: a failure prints a
`case_seed` that reproduces it with `fuzz <case_seed> 1`.

No nightly, no libFuzzer, no sanitizer — so it runs in `cargo test`.

## libFuzzer — coverage-guided, needs nightly

```bash
cargo install cargo-fuzz
cargo +nightly fuzz run differential
cargo +nightly fuzz run roundtrip
cargo +nightly fuzz run flat_view
```

| target | checks | analogue |
|---|---|---|
| `differential` | all of `fuzz::check` against `serde_json` | yyjson `fuzzer.c`, simdjson `fuzz_parser` |
| `roundtrip` | serialize → parse → serialize is a fixed point | simdjson `fuzz_minify` |
| `flat_view` | hostile bytes through the zero-copy reader | simdjson `fuzz_padded` |

`corpus/differential/` is seeded with JSONTestSuite.

The `fuzz/` crate has its own `[workspace]`, so a plain `cargo test` at the
repository root never tries to build it.

## What is checked

Not just "does it crash". `dacode_json::fuzz::check` verifies:

* `strict` agrees with `serde_json` on validity;
* accepted documents deserialize to the same value (2 ULP float slack — the
  measured worst case for serde_json's float parser);
* the single-pass and index-fed builders produce byte-identical pools;
* all nine Stage 1 scanners agree on valid documents;
* serialize → re-parse preserves the value;
* a `jsonflat` buffer we just built passes its own deep validation;
* no parser in the crate panics on any input.
