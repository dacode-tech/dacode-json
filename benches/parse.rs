//! Full-document parse throughput.
//!
//! # Reading these numbers honestly
//!
//! The contenders do **not** produce equivalent results, and the differences
//! favour the Vela port:
//!
//! | | validates | numbers | strings | reuses buffers | mutates input |
//! |---|---|---|---|---|---|
//! | `vela_faithful` | no | `i64` only, lossy | raw slices | yes | no |
//! | `vela_strict` | yes | `i64` + `f64` | raw slices | yes | no |
//! | `serde_json::Value` | yes | `i64`/`u64`/`f64` | owned `String` | no | no |
//! | `simd_json::to_tape` | yes | full | in-place unescape | yes | **yes** |
//! | `simd_json::to_borrowed_value` | yes | full | borrowed/in-place | yes | **yes** |
//! | `sonic_rs::Value` | yes | full | owned | no | no |
//!
//! `vela_faithful` skipping validation is worth real time, and it is the
//! reason `vela_strict` is the number to quote when comparing against
//! anything else here. simd-json's in-place unescaping means it needs a
//! fresh mutable copy of the input per iteration; that copy is excluded from
//! the timing (see `setup`), which flatters it slightly relative to a real
//! application that has to make it.

use criterion::{criterion_group, criterion_main, BatchSize, BenchmarkId, Criterion, Throughput};
use std::hint::black_box;
use simd_json::prelude::*;
use vela_json::corpus;
use vela_json::strict::StrictParser;
use vela_json::Workspace;

const SEED: u64 = 0x5CA7;

// =====================================================================
// Workload wrappers
// =====================================================================
//
// Every timed operation goes through an `#[inline(never)]` function.
//
// With `lto = "fat"` and `codegen-units = 1`, a benchmark function holding
// many closures gives the inliner a large budget to spend unevenly:
// adding or removing an unrelated call site in the same function moved one
// measurement by 2.5x with no change to the code under test. Forcing a call
// boundary makes every contender pay the same fixed overhead — nanoseconds
// against milliseconds — and makes the numbers reproducible.
//
// See docs/CBASELINE.md section 5.

#[inline(never)]
fn w_vela_faithful(ws: &mut Workspace, src: &[u8]) -> usize {
    ws.parse(src).pool().len()
}

#[inline(never)]
fn w_vela_strict(p: &mut StrictParser, src: &[u8]) -> usize {
    p.parse(src).map(|d| d.pool().len()).unwrap_or(0)
}

#[inline(never)]
fn w_vela_fresh(src: &[u8]) -> usize {
    vela_json::parse_to_pool(src).len()
}

#[inline(never)]
fn w_serde_value(src: &[u8]) -> usize {
    serde_json::from_slice::<serde_json::Value>(src)
        .ok()
        .map_or(0, |v| v.is_object() as usize + 1)
}

#[inline(never)]
fn w_simd_tape(buf: &mut [u8], buffers: &mut simd_json::Buffers) -> usize {
    simd_json::to_tape_with_buffers(buf, buffers)
        .map(|t| usize::from(t.as_value().is_object()))
        .unwrap_or(0)
}

#[inline(never)]
fn w_simd_borrowed(buf: &mut [u8]) -> usize {
    simd_json::to_borrowed_value(buf)
        .map(|v| usize::from(v.is_object()))
        .unwrap_or(0)
}

#[inline(never)]
fn w_sonic_value(src: &[u8]) -> usize {
    sonic_rs::from_slice::<sonic_rs::Value>(src)
        .map(|v| usize::from(sonic_rs::JsonValueTrait::is_object(&v)))
        .unwrap_or(0)
}

/// simd-json reads past the end of the buffer, so it needs padding.
fn padded(src: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(src.len() + 64);
    v.extend_from_slice(src);
    v.extend(std::iter::repeat_n(b' ', 64));
    v.truncate(src.len());
    v.reserve(64);
    v
}

fn bench_corpus(c: &mut Criterion, size: usize, group_name: &str) {
    let mut group = c.benchmark_group(group_name);
    group.sample_size(30);

    for (name, json) in corpus::suite(size, SEED) {
        let bytes = json.as_bytes();
        group.throughput(Throughput::Bytes(bytes.len() as u64));

        // --- Vela port, faithful (no validation, integer-only numbers) ---
        {
            let mut ws = Workspace::with_capacity(bytes.len());
            // Warm the buffers so the first sample is not an outlier.
            let _ = ws.parse(bytes).root().len();
            group.bench_with_input(BenchmarkId::new("vela_faithful", name), bytes, |b, bytes| {
                b.iter(|| black_box(w_vela_faithful(&mut ws, black_box(bytes))));
            });
        }

        // --- Vela port, RFC 8259 conformant ---
        {
            let mut p = StrictParser::with_capacity(bytes.len());
            let _ = p.validate(bytes);
            group.bench_with_input(BenchmarkId::new("vela_strict", name), bytes, |b, bytes| {
                b.iter(|| black_box(w_vela_strict(&mut p, black_box(bytes))));
            });
        }

        // --- Vela port, conformant but with string validation off ---
        {
            let opts = vela_json::strict::Options {
                validate_strings: false,
                ..Default::default()
            };
            let mut p = StrictParser::with_capacity(bytes.len());
            p.set_options(opts);
            group.bench_with_input(
                BenchmarkId::new("vela_strict_no_strcheck", name),
                bytes,
                |b, bytes| {
                    b.iter(|| black_box(w_vela_strict(&mut p, black_box(bytes))));
                },
            );
        }

        // --- serde_json ---
        group.bench_with_input(BenchmarkId::new("serde_json_value", name), bytes, |b, bytes| {
            b.iter(|| black_box(w_serde_value(black_box(bytes))));
        });

        // --- simd-json tape (the closest structural analogue) ---
        {
            let mut buffers = simd_json::Buffers::new(bytes.len());
            group.bench_with_input(BenchmarkId::new("simd_json_tape", name), bytes, |b, bytes| {
                b.iter_batched_ref(
                    || padded(bytes),
                    |buf| black_box(w_simd_tape(buf, &mut buffers)),
                    BatchSize::LargeInput,
                );
            });
        }

        // --- simd-json borrowed DOM ---
        group.bench_with_input(
            BenchmarkId::new("simd_json_borrowed", name),
            bytes,
            |b, bytes| {
                b.iter_batched_ref(
                    || padded(bytes),
                    |buf| black_box(w_simd_borrowed(buf)),
                    BatchSize::LargeInput,
                );
            },
        );

        // --- sonic-rs ---
        group.bench_with_input(BenchmarkId::new("sonic_rs_value", name), bytes, |b, bytes| {
            b.iter(|| black_box(w_sonic_value(black_box(bytes))));
        });
    }

    group.finish();
}

fn bench_1mb(c: &mut Criterion) {
    bench_corpus(c, 1 << 20, "parse_1mb");
}

fn bench_10mb(c: &mut Criterion) {
    bench_corpus(c, 10 << 20, "parse_10mb");
}

/// Small-payload latency, where per-call overhead dominates throughput.
///
/// This is the case Vela's `json_parse_inline` (J2, a 96 KB static BSS
/// buffer) exists to fix: `t860` measured 5590 ns for a 68-byte document
/// versus yyjson's 104 ns, essentially all of it `mmap` cost. The Rust
/// equivalent is simply reusing a `Workspace`.
fn bench_small(c: &mut Criterion) {
    let mut group = c.benchmark_group("parse_small");

    let cases: Vec<(&str, String)> = vec![
        ("68b", r#"{"id":1,"name":"alpha","ok":true,"tags":["x","y"]}"#.to_string()),
        ("1kb", corpus::sized(1024, SEED, corpus::records)),
        ("16kb", corpus::sized(16 * 1024, SEED, corpus::records)),
    ];

    for (name, json) in &cases {
        let bytes = json.as_bytes();
        group.throughput(Throughput::Bytes(bytes.len() as u64));

        let mut ws = Workspace::with_capacity(bytes.len());
        let _ = ws.parse(bytes);
        group.bench_with_input(BenchmarkId::new("vela_faithful_reuse", name), bytes, |b, bytes| {
            b.iter(|| black_box(w_vela_faithful(&mut ws, black_box(bytes))));
        });

        let mut p = StrictParser::with_capacity(bytes.len());
        let _ = p.validate(bytes);
        group.bench_with_input(BenchmarkId::new("vela_strict_reuse", name), bytes, |b, bytes| {
            b.iter(|| black_box(w_vela_strict(&mut p, black_box(bytes))));
        });

        // Fresh allocation every call — what Vela's `json_pool_parse_fast`
        // does, and what J2 was written to avoid.
        group.bench_with_input(BenchmarkId::new("vela_faithful_fresh", name), bytes, |b, bytes| {
            b.iter(|| black_box(w_vela_fresh(black_box(bytes))));
        });

        group.bench_with_input(BenchmarkId::new("serde_json_value", name), bytes, |b, bytes| {
            b.iter(|| black_box(w_serde_value(black_box(bytes))));
        });

        let mut buffers = simd_json::Buffers::new(bytes.len().max(128));
        group.bench_with_input(BenchmarkId::new("simd_json_tape", name), bytes, |b, bytes| {
            b.iter_batched_ref(
                || padded(bytes),
                |buf| black_box(w_simd_tape(buf, &mut buffers)),
                BatchSize::SmallInput,
            );
        });

        group.bench_with_input(BenchmarkId::new("sonic_rs_value", name), bytes, |b, bytes| {
            b.iter(|| black_box(w_sonic_value(black_box(bytes))));
        });
    }

    group.finish();
}

criterion_group!(benches, bench_small, bench_1mb, bench_10mb);
criterion_main!(benches);
