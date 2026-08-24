//! Stage 1 only — structural index construction.
//!
//! This is the half of the pipeline Vela already wins at:
//! `docs/stage2/JSON_IMPROVEMENT_PLAN.md:52-73` measures the branchless
//! scanner at 914 MB/s against simdjson's 860 MB/s for a full parse, and
//! concludes "the gap is entirely in DOM building". Measuring Stage 1 in
//! isolation checks whether that carries over to a Rust build.
//!
//! Nothing else in the comparison set exposes its Stage 1, so this group
//! only compares the three Vela scanners against each other.

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use std::hint::black_box;
use vela_json::corpus;
use vela_json::scan::{scan_into, Scanner, StructuralIndex};

const TARGET: usize = 1 << 20; // 1 MiB per corpus
const SEED: u64 = 0x5CA7;

fn bench_scan(c: &mut Criterion) {
    let mut group = c.benchmark_group("stage1_scan");

    for (name, json) in corpus::suite(TARGET, SEED) {
        let bytes = json.as_bytes();
        group.throughput(Throughput::Bytes(bytes.len() as u64));

        for scanner in [Scanner::Scalar, Scanner::Branchless, Scanner::Branchless2x] {
            let label = match scanner {
                Scanner::Scalar => "scalar",
                Scanner::Branchless => "branchless_s6",
                Scanner::Branchless2x => "branchless_s6b_2x",
            };
            let mut idx = StructuralIndex::with_capacity(bytes.len() + 1);

            group.bench_with_input(BenchmarkId::new(label, name), bytes, |b, bytes| {
                b.iter(|| {
                    idx.clear();
                    scan_into(scanner, black_box(bytes), &mut idx);
                    black_box(idx.len())
                });
            });
        }
    }

    group.finish();
}

/// Density matters: the extraction loop runs once per set bit, so a document
/// with many structural characters per byte behaves very differently from a
/// string-heavy one.
fn bench_density(c: &mut Criterion) {
    let mut group = c.benchmark_group("stage1_density");

    let cases = [
        // ~1 structural per 3 bytes.
        ("dense_ints", corpus::sized(1 << 20, SEED, corpus::int_array)),
        // ~1 structural per 40 bytes.
        ("sparse_strings", corpus::sized(1 << 20, SEED, corpus::strings)),
    ];

    for (name, json) in &cases {
        let bytes = json.as_bytes();
        let idx = vela_json::scan::scan(Scanner::Scalar, bytes);
        let density = idx.len() as f64 / bytes.len() as f64;
        println!("  {name}: {:.4} structurals/byte", density);

        group.throughput(Throughput::Bytes(bytes.len() as u64));
        let mut buf = StructuralIndex::with_capacity(bytes.len() + 1);
        group.bench_with_input(BenchmarkId::new("branchless_s6b_2x", name), bytes, |b, bytes| {
            b.iter(|| {
                buf.clear();
                scan_into(Scanner::Branchless2x, black_box(bytes), &mut buf);
                black_box(buf.len())
            });
        });
    }

    group.finish();
}

criterion_group!(benches, bench_scan, bench_density);
criterion_main!(benches);
