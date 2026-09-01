//! The port against the C libraries Vela's tiers were modelled on.
//!
//! Vela's `AUTHORS` file credits **yyjson** for tier 3 ("flat 16-byte value
//! nodes in a pre-allocated pool", "capacity heuristic: len+1 nodes") and
//! **simdjson** for tier 2's structural indexing. Everything measured so far
//! has been against Rust libraries; this is the comparison against the
//! originals.
//!
//! Vela's own numbers, `docs/stage2/JSON_IMPROVEMENT_PLAN.md:52-73`, on a
//! 10 MB payload:
//!
//! | | throughput |
//! |---|---|
//! | simdjson | 860 MB/s |
//! | yyjson | 711 MB/s |
//! | Vela tier 3 (`json_pool_parse_ws`) | 166 MB/s |
//!
//! Those are the figures to beat.
//!
//! # Fairness
//!
//! Every C entry point is a **whole workload** implemented in C
//! (`vendor/shim.c`, `vendor/shim_simdjson.cpp`) — no FFI crossing per
//! field. Both are built at `-O3`. simdjson runs its `arm64` kernel.
//!
//! Where the semantics differ, the difference favours the C libraries:
//! they parse real floating-point numbers, validate the grammar, and
//! (yyjson) unescape strings, none of which the faithful port does. The row
//! to compare against them is **`vela_strict`**, not `vela_faithful`.
//!
//! | | validates | numbers | strings | buffer reuse | mutates input |
//! |---|---|---|---|---|---|
//! | `vela_faithful` | no | `i64`, lossy | raw slices | yes | no |
//! | `vela_strict` | yes | `i64` + `f64` | raw slices | yes | no |
//! | `yyjson` | yes | full | unescaped, owned | pool variant | insitu variant |
//! | `simdjson` on-demand | yes | full | on request | yes | no (needs padding) |

use criterion::{criterion_group, criterion_main, BatchSize, BenchmarkId, Criterion, Throughput};
use std::hint::black_box;
use dacodec::cbench::{Padded, SimdJson, YyJson, YyPool};
use dacodec::corpus;
use dacodec::strict::StrictParser;
use dacodec::Workspace;

const SEED: u64 = 0x2C0;

// =====================================================================
// Workload wrappers
// =====================================================================
//
// Every timed operation goes through an `#[inline(never)]` function.
//
// This is not cosmetic. With `lto = "fat"` and `codegen-units = 1`, a
// benchmark function containing many closures gives the inliner a large
// budget to spend unevenly: adding or removing an unrelated call site in
// the same function changed one of these measurements by **2.5x**
// (225 -> 561 MiB/s) with no change to the code under test. Forcing a
// call boundary makes every contender pay the same fixed overhead — a few
// nanoseconds on operations that take milliseconds — and makes the numbers
// reproducible and comparable.
//
// See docs/CBASELINE.md for the full account.

#[inline(never)]
fn w_vela_faithful(ws: &mut Workspace, src: &[u8]) -> usize {
    ws.parse(src).pool().len()
}

#[inline(never)]
fn w_vela_strict(p: &mut StrictParser, src: &[u8]) -> usize {
    p.parse(src).map(|d| d.pool().len()).unwrap_or(0)
}

#[inline(never)]
fn w_yy_parse(src: &[u8]) -> usize {
    YyJson::parse(src)
}

#[inline(never)]
fn w_yy_insitu(p: &mut Padded) -> usize {
    YyJson::parse_insitu(p)
}

#[inline(never)]
fn w_yy_pool(pool: &mut YyPool, src: &[u8]) -> usize {
    pool.parse(src)
}

#[inline(never)]
fn w_sj_dom(p: &Padded) -> usize {
    SimdJson::parse_dom(p)
}

#[inline(never)]
fn w_sj_ondemand(p: &Padded) -> usize {
    SimdJson::parse_ondemand(p)
}

#[inline(never)]
fn w_serde_valid(src: &[u8]) -> bool {
    serde_json::from_slice::<serde_json::Value>(src).is_ok()
}

fn corpora(target: usize) -> Vec<(&'static str, String)> {
    corpus::suite(target, SEED)
}

/// Print the exact versions so the numbers are attributable.
fn banner() {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        println!("\n  baselines: yyjson {} | simdjson {} ({} kernel, {} B padding)\n",
            YyJson::version(),
            SimdJson::version(),
            SimdJson::implementation(),
            SimdJson::padding());
    });
}

/// Cross-check that everything computes the same answer before timing.
fn verify(src: &[u8]) {
    let padded = Padded::new(src);

    let want: i64 = serde_json::from_slice::<serde_json::Value>(src)
        .expect("corpus is valid")
        .as_array()
        .map(|a| a.iter().filter_map(|r| r.get("score")?.as_i64()).sum())
        .unwrap_or(0);

    assert_eq!(YyJson::sum_field(src, "score"), want, "yyjson sum_field");
    assert_eq!(SimdJson::sum_field(&padded, "score"), want, "simdjson sum_field");

    let mut ws = Workspace::new();
    let doc = ws.parse(src);
    let ours: i64 = doc
        .root()
        .elements()
        .filter_map(|r| r.get("score")?.as_i64())
        .sum();
    assert_eq!(ours, want, "vela sum_field");

    assert!(YyJson::validate(src));
    assert!(SimdJson::validate(&padded));
}

// =====================================================================
// Full DOM construction — the headline comparison
// =====================================================================

fn bench_parse(c: &mut Criterion, size: usize, group_name: &str) {
    banner();
    let mut group = c.benchmark_group(group_name);
    group.sample_size(30);

    for (name, json) in corpora(size) {
        let src = json.as_bytes();
        if name == "records" {
            verify(src);
        }
        group.throughput(Throughput::Bytes(src.len() as u64));

        // --- the port ---
        {
            let mut ws = Workspace::with_capacity(src.len());
            let _ = ws.parse(src);
            group.bench_with_input(BenchmarkId::new("vela_faithful", name), src, |b, src| {
                b.iter(|| black_box(w_vela_faithful(&mut ws, black_box(src))));
            });
        }
        {
            let mut p = StrictParser::with_capacity(src.len());
            let _ = p.validate(src);
            group.bench_with_input(BenchmarkId::new("vela_strict", name), src, |b, src| {
                b.iter(|| black_box(w_vela_strict(&mut p, black_box(src))));
            });
        }

        // --- yyjson ---
        group.bench_with_input(BenchmarkId::new("yyjson", name), src, |b, src| {
            b.iter(|| black_box(w_yy_parse(black_box(src))));
        });

        // yyjson's fastest read path. Mutates the buffer, so it is restored
        // per batch; the restore is outside the timed section.
        group.bench_with_input(BenchmarkId::new("yyjson_insitu", name), src, |b, src| {
            b.iter_batched_ref(
                || Padded::new(src),
                |p| black_box(w_yy_insitu(p)),
                BatchSize::LargeInput,
            );
        });

        // Pooled allocator — the analogue of the port's reusable Workspace.
        if let Some(mut pool) = YyPool::new(src.len() * 24 + (1 << 20)) {
            group.bench_with_input(BenchmarkId::new("yyjson_pool", name), src, |b, src| {
                b.iter(|| black_box(w_yy_pool(&mut pool, black_box(src))));
            });
        }

        // --- simdjson ---
        let padded = Padded::new(src);
        group.bench_with_input(BenchmarkId::new("simdjson_dom", name), src, |b, _| {
            b.iter(|| black_box(w_sj_dom(black_box(&padded))));
        });
        group.bench_with_input(BenchmarkId::new("simdjson_ondemand", name), src, |b, _| {
            b.iter(|| black_box(w_sj_ondemand(black_box(&padded))));
        });
    }

    group.finish();
}

fn bench_parse_1mb(c: &mut Criterion) {
    bench_parse(c, 1 << 20, "c_parse_1mb");
}

/// The size Vela benchmarked at, so the numbers line up with
/// `JSON_IMPROVEMENT_PLAN.md:52-73`.
fn bench_parse_10mb(c: &mut Criterion) {
    bench_parse(c, 10 << 20, "c_parse_10mb");
}

// =====================================================================
// Realistic workloads
// =====================================================================

/// Parse and sum one integer field across every record.
fn bench_sum_field(c: &mut Criterion) {
    banner();
    let mut group = c.benchmark_group("c_sum_field");
    group.sample_size(30);

    for size in [256 * 1024usize, 4 << 20] {
        let json = corpus::sized(size, SEED, corpus::records);
        let src = json.as_bytes();
        verify(src);
        let label = if size < (1 << 20) { "256kb" } else { "4mb" };
        group.throughput(Throughput::Bytes(src.len() as u64));

        let mut ws = Workspace::with_capacity(src.len());
        let _ = ws.parse(src);
        group.bench_with_input(BenchmarkId::new("vela_faithful", label), src, |b, src| {
            b.iter(|| {
                let doc = ws.parse(black_box(src));
                let sum: i64 = doc
                    .root()
                    .elements()
                    .filter_map(|r| r.get("score")?.as_i64())
                    .sum();
                black_box(sum)
            });
        });

        let mut sp = StrictParser::with_capacity(src.len());
        let _ = sp.validate(src);
        group.bench_with_input(BenchmarkId::new("vela_strict", label), src, |b, src| {
            b.iter(|| {
                let doc = sp.parse(black_box(src)).expect("valid");
                let sum: i64 = doc
                    .root()
                    .elements()
                    .filter_map(|r| r.get("score")?.as_i64())
                    .sum();
                black_box(sum)
            });
        });

        group.bench_with_input(BenchmarkId::new("yyjson", label), src, |b, src| {
            b.iter(|| black_box(YyJson::sum_field(black_box(src), "score")));
        });

        let padded = Padded::new(src);
        group.bench_with_input(BenchmarkId::new("simdjson_ondemand", label), src, |b, _| {
            b.iter(|| black_box(SimdJson::sum_field(black_box(&padded), "score")));
        });

        // Deserializing only the field we want is what a caller would
        // actually write, and it is the workload `direct` is built for.
        #[derive(serde::Deserialize)]
        struct Score {
            score: i64,
        }

        group.bench_with_input(BenchmarkId::new("dacodec_direct", label), src, |b, src| {
            b.iter(|| {
                let rows: Vec<Score> =
                    dacodec::direct::from_slice_borrowed(black_box(src)).expect("valid");
                let sum: i64 = rows.iter().map(|r| r.score).sum();
                black_box(sum)
            });
        });

        // The indexed streaming path. Navigation is index arithmetic
        // rather than byte scanning, which should pay when most of the
        // document is skipped - this is the shape simdjson On-Demand wins
        // on, so it is the one worth testing.
        group.bench_with_input(BenchmarkId::new("dacodec_stream", label), src, |b, src| {
            let mut idx = dacodec::stream::Index::default();
            idx.reserve_estimated(src.len());
            b.iter(|| {
                let rows: Vec<Score> =
                    dacodec::stream::from_slice_with(&mut idx, black_box(src)).expect("valid");
                let sum: i64 = rows.iter().map(|r| r.score).sum();
                black_box(sum)
            });
        });

        group.bench_with_input(BenchmarkId::new("serde_json_typed", label), src, |b, src| {
            b.iter(|| {
                let rows: Vec<Score> = serde_json::from_slice(black_box(src)).expect("valid");
                let sum: i64 = rows.iter().map(|r| r.score).sum();
                black_box(sum)
            });
        });

        group.bench_with_input(BenchmarkId::new("serde_json", label), src, |b, src| {
            b.iter(|| {
                // Was parsing twice - `w_serde_value` and then `from_slice`
                // - which halved the reported throughput. The wrapper is
                // gone; this now measures one parse, like every other row.
                let v: serde_json::Value =
                    serde_json::from_slice(black_box(src)).expect("valid");
                let sum: i64 = v
                    .as_array()
                    .map(|a| a.iter().filter_map(|r| r.get("score")?.as_i64()).sum())
                    .unwrap_or(0);
                black_box(sum)
            });
        });
    }

    group.finish();
}

/// Read every field of every record.
fn bench_extract_all(c: &mut Criterion) {
    banner();
    let mut group = c.benchmark_group("c_extract_all");
    group.sample_size(30);

    let json = corpus::sized(1 << 20, SEED, corpus::records);
    let src = json.as_bytes();
    group.throughput(Throughput::Bytes(src.len() as u64));

    let mut ws = Workspace::with_capacity(src.len());
    let _ = ws.parse(src);
    group.bench_function("vela_faithful", |b| {
        b.iter(|| {
            let doc = ws.parse(black_box(src));
            let mut acc = 0usize;
            for rec in doc.root().elements() {
                for (k, v) in rec.entries() {
                    acc += k.len();
                    acc += match v.typ() {
                        dacodec::Type::String => v.as_str().map_or(0, |s| s.len()),
                        dacodec::Type::Number => v.as_i64().unwrap_or(0) as usize,
                        dacodec::Type::Bool => usize::from(v.as_bool() == Some(true)),
                        dacodec::Type::Array => v.elements().count(),
                        _ => 0,
                    };
                }
            }
            black_box(acc)
        });
    });

    group.bench_function("yyjson", |b| {
        b.iter(|| black_box(YyJson::extract_all(black_box(src))));
    });

    let padded = Padded::new(src);
    group.bench_function("simdjson_ondemand", |b| {
        b.iter(|| black_box(SimdJson::extract_all(black_box(&padded))));
    });

    group.finish();
}

/// One field from the first record — the case a lazy parser wins outright.
fn bench_first_field(c: &mut Criterion) {
    banner();
    let mut group = c.benchmark_group("c_first_field");

    let json = corpus::sized(1 << 20, SEED, corpus::records);
    let src = json.as_bytes();

    let mut ws = Workspace::with_capacity(src.len());
    let _ = ws.parse(src);
    group.bench_function("vela_faithful", |b| {
        b.iter(|| {
            let doc = ws.parse(black_box(src));
            black_box(
                doc.root()
                    .at(0)
                    .and_then(|r| r.get("name"))
                    .and_then(|v| v.as_str())
                    .map(|s| s.len()),
            )
        });
    });

    group.bench_function("yyjson", |b| {
        b.iter(|| black_box(YyJson::first_field(black_box(src), "name")));
    });

    let padded = Padded::new(src);
    group.bench_function("simdjson_ondemand", |b| {
        b.iter(|| black_box(SimdJson::first_field(black_box(&padded), "name")));
    });

    // For scale: the zero-copy format from docs/ZEROCOPY.md.
    let mut p = StrictParser::new();
    let flatbuf = dacodec::flat::encode(p.parse(src).expect("valid")).expect("encode");
    group.bench_function("jsonflat", |b| {
        b.iter(|| {
            let v = dacodec::flat::View::new(black_box(&flatbuf)).expect("view");
            black_box(
                v.root()
                    .at(0)
                    .and_then(|r| r.get("name"))
                    .map(|s| s.as_str().map_or(0, str::len)),
            )
        });
    });

    group.finish();
}

/// Validation, where the port's `strict` mode is the only comparable row.
fn bench_validate(c: &mut Criterion) {
    banner();
    let mut group = c.benchmark_group("c_validate");
    group.sample_size(30);

    for (name, json) in corpora(1 << 20) {
        let src = json.as_bytes();
        group.throughput(Throughput::Bytes(src.len() as u64));

        let mut p = StrictParser::with_capacity(src.len());
        let _ = p.validate(src);
        group.bench_with_input(BenchmarkId::new("vela_strict", name), src, |b, src| {
            b.iter(|| black_box(p.validate(black_box(src)).is_ok()));
        });

        group.bench_with_input(BenchmarkId::new("yyjson", name), src, |b, src| {
            b.iter(|| black_box(YyJson::validate(black_box(src))));
        });

        let padded = Padded::new(src);
        group.bench_with_input(BenchmarkId::new("simdjson_ondemand", name), src, |b, _| {
            b.iter(|| black_box(SimdJson::validate(black_box(&padded))));
        });

        group.bench_with_input(BenchmarkId::new("serde_json", name), src, |b, src| {
            b.iter(|| black_box(w_serde_valid(black_box(src))));
        });
    }

    group.finish();
}

/// Parse then re-serialize.
fn bench_roundtrip(c: &mut Criterion) {
    banner();
    let mut group = c.benchmark_group("c_roundtrip");
    group.sample_size(30);

    let json = corpus::sized(1 << 20, SEED, corpus::records);
    let src = json.as_bytes();
    group.throughput(Throughput::Bytes(src.len() as u64));

    group.bench_function("yyjson", |b| {
        b.iter(|| black_box(YyJson::roundtrip(black_box(src))));
    });

    // The port's equivalent: parse to a typed value, serialize back.
    #[derive(serde::Serialize, serde::Deserialize)]
    struct Record {
        id: u64,
        name: String,
        age: u32,
        active: bool,
        city: String,
        score: i64,
        tags: Vec<String>,
    }

    let mut out = Vec::with_capacity(src.len() + 64);
    group.bench_function("vela_strict_serde", |b| {
        b.iter(|| {
            let v: Vec<Record> = dacodec::de::from_slice(black_box(src)).expect("de");
            out.clear();
            dacodec::ser::to_writer(&mut out, &v).expect("ser");
            black_box(out.len())
        });
    });

    group.bench_function("serde_json", |b| {
        b.iter(|| {
            let v: serde_json::Value = serde_json::from_slice(black_box(src)).expect("de");
            black_box(serde_json::to_vec(&v).expect("ser").len())
        });
    });

    group.finish();
}

criterion_group!(
    benches,
    bench_parse_1mb,
    bench_parse_10mb,
    bench_sum_field,
    bench_extract_all,
    bench_first_field,
    bench_validate,
    bench_roundtrip
);
criterion_main!(benches);
