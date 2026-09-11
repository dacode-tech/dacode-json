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
use dacode_json::corpus;
use dacode_json::scan::{scan_into, Scanner, StructuralIndex};

const TARGET: usize = 1 << 20; // 1 MiB per corpus
const SEED: u64 = 0x5CA7;

fn bench_scan(c: &mut Criterion) {
    let mut group = c.benchmark_group("stage1_scan");

    for (name, json) in corpus::suite(TARGET, SEED) {
        let bytes = json.as_bytes();
        group.throughput(Throughput::Bytes(bytes.len() as u64));

        for scanner in Scanner::ALL {
            let label = scanner.label();
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
        let idx = dacode_json::scan::scan(Scanner::Scalar, bytes);
        let density = idx.len() as f64 / bytes.len() as f64;
        println!("  {name}: {:.4} structurals/byte", density);

        group.throughput(Throughput::Bytes(bytes.len() as u64));
        for scanner in [Scanner::Branchless2x, Scanner::Branchless2xTable] {
            let mut buf = StructuralIndex::with_capacity(bytes.len() + 1);
            group.bench_with_input(BenchmarkId::new(scanner.label(), name), bytes, |b, bytes| {
                b.iter(|| {
                    buf.clear();
                    scan_into(scanner, black_box(bytes), &mut buf);
                    black_box(buf.len())
                });
            });
        }
    }

    group.finish();
}

/// Classifier in isolation: no escape logic, no position extraction, just
/// bytes in and three bitmasks out. This is the measurement that decides
/// whether the lookup table Vela's design docs specified
/// (`P1_2_JSON_TIERS.md:91`) is actually the faster technique on this
/// hardware.
fn bench_classifier(c: &mut Criterion) {
    use dacode_json::scan::branchless::{classify, classify_scalar};
    use dacode_json::scan::table::{classify_lut256, classify_shuffle};

    let json = corpus::sized(1 << 20, SEED, corpus::records);
    let bytes = json.as_bytes();

    let mut group = c.benchmark_group("classifier_only");
    group.throughput(Throughput::Bytes(bytes.len() as u64));

    type ClassifyFn = fn(&[u8; 16]) -> dacode_json::scan::branchless::Classified;
    let variants: [(&str, ClassifyFn); 5] = [
        ("scalar_match", classify_scalar as ClassifyFn),
        ("lut256", classify_lut256),
        ("simd_compare_8x", classify),
        ("simd_shuffle_table", classify_shuffle),
        ("simd_hybrid", dacode_json::scan::table::classify_hybrid),
    ];

    for (name, f) in variants {
        group.bench_function(name, |b| {
            b.iter(|| {
                // XOR-accumulate so nothing can be optimised away, and so
                // consecutive chunks stay independent (no false dependency
                // that would hide the real critical path).
                let mut acc = (0u16, 0u16, 0u16);
                for arr in black_box(bytes).as_chunks::<16>().0 {
                    let r = f(arr);
                    acc.0 ^= r.structural;
                    acc.1 ^= r.quote;
                    acc.2 ^= r.backslash;
                }
                black_box(acc)
            });
        });
    }

    group.finish();
}

/// Where does Stage 1's time actually go?
///
/// Three cumulative phases, each adding one stage of the pipeline:
///
/// 1. `classify` — load + produce three bitmasks
/// 2. `+ carry_chain` — plus escape detection, prefix XOR, in-string state
/// 3. `+ extract` — plus `cttz` position extraction (the full scanner)
///
/// The deltas say which part is worth optimising.
fn bench_phases(c: &mut Criterion) {
    use dacode_json::scan::branchless::{find_escaped, prefix_xor16};
    use dacode_json::scan::table::classify_hybrid;

    let mut group = c.benchmark_group("stage1_phases");

    for (name, json) in corpus::suite(1 << 20, SEED) {
        let bytes = json.as_bytes();
        group.throughput(Throughput::Bytes(bytes.len() as u64));

        group.bench_with_input(BenchmarkId::new("1_classify", name), bytes, |b, bytes| {
            b.iter(|| {
                let mut acc = 0u16;
                for a in black_box(bytes).as_chunks::<16>().0 {
                    let r = classify_hybrid(a);
                    acc ^= r.structural ^ r.quote ^ r.backslash;
                }
                black_box(acc)
            });
        });

        group.bench_with_input(BenchmarkId::new("2_carry_chain", name), bytes, |b, bytes| {
            b.iter(|| {
                let mut esc = 0u16;
                let mut str_ = 0u16;
                let mut acc = 0u16;
                for a in black_box(bytes).as_chunks::<16>().0 {
                    let cl = classify_hybrid(a);
                    let (escaped, e) = find_escaped(cl.backslash, esc);
                    esc = e;
                    let real_q = cl.quote & !escaped;
                    let in_str = prefix_xor16(real_q) ^ str_;
                    str_ = if (in_str >> 15) & 1 != 0 { 0xFFFF } else { 0 };
                    acc ^= real_q | (cl.structural & !in_str);
                }
                black_box(acc)
            });
        });

        let mut idx = StructuralIndex::with_capacity(bytes.len() + 1);
        group.bench_with_input(BenchmarkId::new("3_full_extract", name), bytes, |b, bytes| {
            b.iter(|| {
                idx.clear();
                scan_into(Scanner::BranchlessHybrid, black_box(bytes), &mut idx);
                black_box(idx.len())
            });
        });
    }

    group.finish();
}

criterion_group!(benches, bench_classifier, bench_phases, bench_scan, bench_density);
criterion_main!(benches);
