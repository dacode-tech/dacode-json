//! Parse-and-extract: the workload applications actually run.
//!
//! Raw DOM-construction throughput flatters lazy representations. This
//! group measures parse plus a realistic access pattern, which is where the
//! Vela pool design should pay off — string nodes are `(offset, len)` pairs,
//! so a field you never read costs nothing to decode.
//!
//! Three access patterns:
//!
//! * `sum_field` — one integer field from every record. Touches the index
//!   but almost no string data.
//! * `extract_all` — every field of every record, strings decoded to `&str`.
//!   This forces the Vela port to pay the unescaping it deferred.
//! * `first_only` — one field from the first record. Pure per-call overhead.

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use std::hint::black_box;
use sonic_rs::{JsonContainerTrait, JsonValueTrait};
use dacode_json::corpus;
use dacode_json::strict::StrictParser;
use dacode_json::Workspace;

const SEED: u64 = 0x9E11;

fn corpora() -> Vec<(&'static str, String)> {
    vec![
        ("records_256kb", corpus::sized(256 * 1024, SEED, corpus::records)),
        ("records_4mb", corpus::sized(4 << 20, SEED, corpus::records)),
    ]
}

fn bench_sum_field(c: &mut Criterion) {
    let mut group = c.benchmark_group("query_sum_field");
    group.sample_size(30);

    for (name, json) in corpora() {
        let bytes = json.as_bytes();
        group.throughput(Throughput::Bytes(bytes.len() as u64));

        let mut ws = Workspace::with_capacity(bytes.len());
        {
            // Sanity-check the three implementations agree before timing.
            let doc = ws.parse(bytes);
            let ours: i64 = doc
                .root()
                .elements()
                .filter_map(|r| r.get("score").and_then(|v| v.as_i64()))
                .sum();
            let theirs: i64 = serde_json::from_slice::<serde_json::Value>(bytes)
                .expect("valid")
                .as_array()
                .map(|a| a.iter().filter_map(|r| r.get("score")?.as_i64()).sum())
                .unwrap_or(0);
            assert_eq!(ours, theirs, "{name}: implementations disagree");
        }

        group.bench_with_input(BenchmarkId::new("vela_faithful", name), bytes, |b, bytes| {
            b.iter(|| {
                let doc = ws.parse(black_box(bytes));
                let sum: i64 = doc
                    .root()
                    .elements()
                    .filter_map(|r| r.get("score").and_then(|v| v.as_i64()))
                    .sum();
                black_box(sum)
            });
        });

        let mut sp = StrictParser::with_capacity(bytes.len());
        let _ = sp.validate(bytes);
        group.bench_with_input(BenchmarkId::new("vela_strict", name), bytes, |b, bytes| {
            b.iter(|| {
                let doc = sp.parse(black_box(bytes)).expect("valid");
                let sum: i64 = doc
                    .root()
                    .elements()
                    .filter_map(|r| r.get("score").and_then(|v| v.as_i64()))
                    .sum();
                black_box(sum)
            });
        });

        group.bench_with_input(BenchmarkId::new("serde_json_value", name), bytes, |b, bytes| {
            b.iter(|| {
                let v: serde_json::Value = serde_json::from_slice(black_box(bytes)).expect("valid");
                let sum: i64 = v
                    .as_array()
                    .map(|a| a.iter().filter_map(|r| r.get("score")?.as_i64()).sum())
                    .unwrap_or(0);
                black_box(sum)
            });
        });

        group.bench_with_input(BenchmarkId::new("sonic_rs_value", name), bytes, |b, bytes| {
            b.iter(|| {
                let v: sonic_rs::Value = sonic_rs::from_slice(black_box(bytes)).expect("valid");
                let sum: i64 = v
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .filter_map(|r| r.get("score").and_then(sonic_rs::JsonValueTrait::as_i64))
                            .sum()
                    })
                    .unwrap_or(0);
                black_box(sum)
            });
        });

        // sonic-rs' lazy API is the design closest to the Vela pool: it
        // skips values it is not asked for.
        group.bench_with_input(BenchmarkId::new("sonic_rs_lazy", name), bytes, |b, bytes| {
            b.iter(|| {
                let mut sum: i64 = 0;
                for item in sonic_rs::to_array_iter(black_box(bytes)) {
                    let Ok(item) = item else { continue };
                    if let Ok(v) = sonic_rs::get(item.as_raw_str().as_bytes(), ["score"]) {
                        sum += sonic_rs::JsonValueTrait::as_i64(&v).unwrap_or(0);
                    }
                }
                black_box(sum)
            });
        });
    }

    group.finish();
}

fn bench_extract_all(c: &mut Criterion) {
    let mut group = c.benchmark_group("query_extract_all");
    group.sample_size(30);

    for (name, json) in corpora() {
        let bytes = json.as_bytes();
        group.throughput(Throughput::Bytes(bytes.len() as u64));

        let mut ws = Workspace::with_capacity(bytes.len());
        let _ = ws.parse(bytes);
        group.bench_with_input(BenchmarkId::new("vela_faithful", name), bytes, |b, bytes| {
            b.iter(|| {
                let doc = ws.parse(black_box(bytes));
                let mut acc = 0usize;
                for rec in doc.root().elements() {
                    for (k, v) in rec.entries() {
                        acc += k.len();
                        acc += match v.typ() {
                            dacode_json::Type::String => v.as_str().map_or(0, |s| s.len()),
                            dacode_json::Type::Number => v.as_i64().unwrap_or(0) as usize,
                            dacode_json::Type::Bool => usize::from(v.as_bool() == Some(true)),
                            dacode_json::Type::Array => v.elements().count(),
                            _ => 0,
                        };
                    }
                }
                black_box(acc)
            });
        });

        let mut sp = StrictParser::with_capacity(bytes.len());
        let _ = sp.validate(bytes);
        group.bench_with_input(BenchmarkId::new("vela_strict", name), bytes, |b, bytes| {
            b.iter(|| {
                let doc = sp.parse(black_box(bytes)).expect("valid");
                let mut acc = 0usize;
                for rec in doc.root().elements() {
                    for (k, v) in rec.entries() {
                        acc += k.len();
                        acc += match v.typ() {
                            dacode_json::Type::String => v.as_str().map_or(0, |s| s.len()),
                            dacode_json::Type::Number => v.as_i64().unwrap_or(0) as usize,
                            dacode_json::Type::Bool => usize::from(v.as_bool() == Some(true)),
                            dacode_json::Type::Array => v.elements().count(),
                            _ => 0,
                        };
                    }
                }
                black_box(acc)
            });
        });

        group.bench_with_input(BenchmarkId::new("serde_json_value", name), bytes, |b, bytes| {
            b.iter(|| {
                let v: serde_json::Value = serde_json::from_slice(black_box(bytes)).expect("valid");
                let mut acc = 0usize;
                for rec in v.as_array().map(Vec::as_slice).unwrap_or_default() {
                    for (k, val) in rec.as_object().into_iter().flatten() {
                        acc += k.len();
                        acc += match val {
                            serde_json::Value::String(s) => s.len(),
                            serde_json::Value::Number(n) => n.as_i64().unwrap_or(0) as usize,
                            serde_json::Value::Bool(b) => usize::from(*b),
                            serde_json::Value::Array(a) => a.len(),
                            _ => 0,
                        };
                    }
                }
                black_box(acc)
            });
        });
    }

    group.finish();
}

/// One field from the first record — per-call overhead with the parse cost
/// amortised away by the lazy contenders.
fn bench_first_only(c: &mut Criterion) {
    let mut group = c.benchmark_group("query_first_only");

    let json = corpus::sized(256 * 1024, SEED, corpus::records);
    let bytes = json.as_bytes();
    group.throughput(Throughput::Bytes(bytes.len() as u64));

    let mut ws = Workspace::with_capacity(bytes.len());
    let _ = ws.parse(bytes);
    group.bench_function("vela_faithful", |b| {
        b.iter(|| {
            let doc = ws.parse(black_box(bytes));
            black_box(
                doc.root()
                    .at(0)
                    .and_then(|r| r.get("name"))
                    .and_then(|v| v.as_str())
                    .map(|s| s.len()),
            )
        });
    });

    group.bench_function("serde_json_value", |b| {
        b.iter(|| {
            let v: serde_json::Value = serde_json::from_slice(black_box(bytes)).expect("valid");
            black_box(v.get(0).and_then(|r| r.get("name")).and_then(|s| s.as_str()).map(str::len))
        });
    });

    // The lazy parsers should crush everything here: they can stop reading
    // after the first record.
    group.bench_function("sonic_rs_pointer", |b| {
        b.iter(|| {
            let v = sonic_rs::get(black_box(bytes), sonic_rs::pointer![0, "name"]);
            black_box(v.ok().map(|v| v.as_raw_str().len()))
        });
    });

    group.finish();
}

criterion_group!(benches, bench_first_only, bench_sum_field, bench_extract_all);
criterion_main!(benches);
