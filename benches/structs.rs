//! Struct-level serialize / deserialize — the workload most applications
//! actually run.
//!
//! Every contender fills the *same* `#[derive(Deserialize)]` type, so this
//! measures the parser and its serde glue rather than the shape of a `Value`
//! enum. It is also the fairest comparison in the repository: unlike the
//! DOM benchmarks, all five implementations do exactly the same work and
//! produce exactly the same result.
//!
//! Deserializing into a struct is the access pattern the tier-3 pool suits
//! best — each field is read once, unknown fields are skipped by an index
//! walk rather than a byte scan, and `&str`/`Cow<str>` fields borrow
//! straight out of the input when they contain no escapes.

use criterion::{criterion_group, criterion_main, BatchSize, BenchmarkId, Criterion, Throughput};
use serde::{Deserialize, Serialize};
use std::borrow::Cow;
use std::hint::black_box;
use dacode_json::corpus;
use dacode_json::strict::StrictParser;
use dacode_json::{de, ser};

const SEED: u64 = 0x57;

/// Owning form — every string is copied.
#[derive(Debug, Serialize, Deserialize, PartialEq)]
struct Record {
    id: u64,
    name: String,
    age: u32,
    active: bool,
    city: String,
    score: i64,
    tags: Vec<String>,
}

/// Borrowing form — strings point into the input when unescaped. This is
/// where a `(offset, len)` representation should pull ahead.
#[derive(Debug, Deserialize, PartialEq)]
struct RecordRef<'a> {
    id: u64,
    #[serde(borrow)]
    name: Cow<'a, str>,
    age: u32,
    active: bool,
    #[serde(borrow)]
    city: Cow<'a, str>,
    score: i64,
    #[serde(borrow)]
    tags: Vec<Cow<'a, str>>,
}

/// Only two of the seven fields — measures how cheaply each parser skips
/// what it was not asked for.
#[derive(Debug, Deserialize, PartialEq)]
struct RecordPartial {
    id: u64,
    score: i64,
}

fn padded(src: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(src.len() + 64);
    v.extend_from_slice(src);
    v
}

fn corpora() -> Vec<(&'static str, String)> {
    vec![
        ("256kb", corpus::sized(256 * 1024, SEED, corpus::records)),
        ("4mb", corpus::sized(4 << 20, SEED, corpus::records)),
    ]
}

/// Confirm every implementation produces the same value before timing them.
fn verify(bytes: &[u8]) {
    let reference: Vec<Record> = serde_json::from_slice(bytes).expect("serde_json");

    let ours: Vec<Record> = de::from_slice(bytes).expect("vela");
    assert_eq!(ours, reference, "vela disagrees with serde_json");

    let mut buf = padded(bytes);
    let theirs: Vec<Record> = simd_json::from_slice(&mut buf).expect("simd_json");
    assert_eq!(theirs, reference, "simd_json disagrees");

    let sonic: Vec<Record> = sonic_rs::from_slice(bytes).expect("sonic_rs");
    assert_eq!(sonic, reference, "sonic_rs disagrees");
}

fn bench_deserialize_owned(c: &mut Criterion) {
    let mut group = c.benchmark_group("struct_de_owned");
    group.sample_size(30);

    for (name, json) in corpora() {
        let bytes = json.as_bytes();
        verify(bytes);
        group.throughput(Throughput::Bytes(bytes.len() as u64));

        let mut p = StrictParser::with_capacity(bytes.len());
        let _ = p.validate(bytes);
        group.bench_with_input(BenchmarkId::new("vela_strict", name), bytes, |b, bytes| {
            b.iter(|| {
                let doc = p.parse(black_box(bytes)).expect("valid");
                let v: Vec<Record> = de::from_doc(doc).expect("de");
                black_box(v)
            });
        });

        group.bench_with_input(BenchmarkId::new("dacode_json_stream", name), bytes, |b, bytes| {
            let mut idx = dacode_json::stream::Index::default();
            idx.reserve_for(bytes.len());
            b.iter(|| {
                let v: Vec<Record> =
                    dacode_json::stream::from_slice_with(&mut idx, black_box(bytes)).expect("de");
                black_box(v)
            });
        });

        group.bench_with_input(BenchmarkId::new("dacode_json_direct", name), bytes, |b, bytes| {
            b.iter(|| {
                let v: Vec<Record> =
                    dacode_json::direct::from_slice_borrowed(black_box(bytes)).expect("de");
                black_box(v)
            });
        });

        group.bench_with_input(BenchmarkId::new("dacode_json_ascii", name), bytes, |b, bytes| {
            b.iter(|| {
                let v: Vec<Record> =
                    dacode_json::direct::from_slice_ascii_borrowed(black_box(bytes)).expect("de");
                black_box(v)
            });
        });

        group.bench_with_input(BenchmarkId::new("serde_json", name), bytes, |b, bytes| {
            b.iter(|| {
                let v: Vec<Record> = serde_json::from_slice(black_box(bytes)).expect("de");
                black_box(v)
            });
        });

        group.bench_with_input(BenchmarkId::new("simd_json", name), bytes, |b, bytes| {
            b.iter_batched_ref(
                || padded(bytes),
                |buf| {
                    let v: Vec<Record> = simd_json::from_slice(buf).expect("de");
                    black_box(v)
                },
                BatchSize::LargeInput,
            );
        });

        group.bench_with_input(BenchmarkId::new("sonic_rs", name), bytes, |b, bytes| {
            b.iter(|| {
                let v: Vec<Record> = sonic_rs::from_slice(black_box(bytes)).expect("de");
                black_box(v)
            });
        });
    }

    group.finish();
}

fn bench_deserialize_borrowed(c: &mut Criterion) {
    let mut group = c.benchmark_group("struct_de_borrowed");
    group.sample_size(30);

    for (name, json) in corpora() {
        let bytes = json.as_bytes();
        group.throughput(Throughput::Bytes(bytes.len() as u64));

        let mut p = StrictParser::with_capacity(bytes.len());
        let _ = p.validate(bytes);
        group.bench_with_input(BenchmarkId::new("vela_strict", name), bytes, |b, bytes| {
            b.iter(|| {
                let doc = p.parse(black_box(bytes)).expect("valid");
                let v: Vec<RecordRef<'_>> = de::from_doc(doc).expect("de");
                black_box(v.len())
            });
        });

        group.bench_with_input(BenchmarkId::new("dacode_json_stream", name), bytes, |b, bytes| {
            let mut idx = dacode_json::stream::Index::default();
            idx.reserve_for(bytes.len());
            b.iter(|| {
                let v: Vec<RecordRef<'_>> =
                    dacode_json::stream::from_slice_with(&mut idx, black_box(bytes)).expect("de");
                black_box(v.len())
            });
        });

        group.bench_with_input(BenchmarkId::new("dacode_json_direct", name), bytes, |b, bytes| {
            b.iter(|| {
                let v: Vec<RecordRef<'_>> =
                    dacode_json::direct::from_slice_borrowed(black_box(bytes)).expect("de");
                black_box(v.len())
            });
        });

        group.bench_with_input(BenchmarkId::new("serde_json", name), bytes, |b, bytes| {
            b.iter(|| {
                let v: Vec<RecordRef<'_>> = serde_json::from_slice(black_box(bytes)).expect("de");
                black_box(v.len())
            });
        });

        group.bench_with_input(BenchmarkId::new("sonic_rs", name), bytes, |b, bytes| {
            b.iter(|| {
                let v: Vec<RecordRef<'_>> = sonic_rs::from_slice(black_box(bytes)).expect("de");
                black_box(v.len())
            });
        });
    }

    group.finish();
}

fn bench_deserialize_partial(c: &mut Criterion) {
    let mut group = c.benchmark_group("struct_de_partial");
    group.sample_size(30);

    for (name, json) in corpora() {
        let bytes = json.as_bytes();
        group.throughput(Throughput::Bytes(bytes.len() as u64));

        let mut p = StrictParser::with_capacity(bytes.len());
        let _ = p.validate(bytes);
        group.bench_with_input(BenchmarkId::new("vela_strict", name), bytes, |b, bytes| {
            b.iter(|| {
                let doc = p.parse(black_box(bytes)).expect("valid");
                let v: Vec<RecordPartial> = de::from_doc(doc).expect("de");
                black_box(v.len())
            });
        });

        group.bench_with_input(BenchmarkId::new("dacode_json_stream", name), bytes, |b, bytes| {
            let mut idx = dacode_json::stream::Index::default();
            idx.reserve_for(bytes.len());
            b.iter(|| {
                let v: Vec<RecordPartial> =
                    dacode_json::stream::from_slice_with(&mut idx, black_box(bytes)).expect("de");
                black_box(v.len())
            });
        });

        group.bench_with_input(BenchmarkId::new("dacode_json_direct", name), bytes, |b, bytes| {
            b.iter(|| {
                let v: Vec<RecordPartial> =
                    dacode_json::direct::from_slice_borrowed(black_box(bytes)).expect("de");
                black_box(v.len())
            });
        });

        group.bench_with_input(BenchmarkId::new("serde_json", name), bytes, |b, bytes| {
            b.iter(|| {
                let v: Vec<RecordPartial> = serde_json::from_slice(black_box(bytes)).expect("de");
                black_box(v.len())
            });
        });

        group.bench_with_input(BenchmarkId::new("simd_json", name), bytes, |b, bytes| {
            b.iter_batched_ref(
                || padded(bytes),
                |buf| {
                    let v: Vec<RecordPartial> = simd_json::from_slice(buf).expect("de");
                    black_box(v.len())
                },
                BatchSize::LargeInput,
            );
        });

        group.bench_with_input(BenchmarkId::new("sonic_rs", name), bytes, |b, bytes| {
            b.iter(|| {
                let v: Vec<RecordPartial> = sonic_rs::from_slice(black_box(bytes)).expect("de");
                black_box(v.len())
            });
        });
    }

    group.finish();
}

fn bench_serialize(c: &mut Criterion) {
    let mut group = c.benchmark_group("struct_ser");
    group.sample_size(30);

    for (name, json) in corpora() {
        let data: Vec<Record> = serde_json::from_str(&json).expect("parse corpus");

        // All four must agree byte-for-byte before we time them.
        let reference = serde_json::to_vec(&data).expect("serde_json");
        assert_eq!(ser::to_vec(&data).expect("vela"), reference, "vela output differs");

        group.throughput(Throughput::Bytes(reference.len() as u64));

        let mut buf = Vec::with_capacity(reference.len() + 64);
        group.bench_with_input(BenchmarkId::new("vela", name), &data, |b, data| {
            b.iter(|| {
                buf.clear();
                ser::to_writer(&mut buf, black_box(data)).expect("ser");
                black_box(buf.len())
            });
        });

        group.bench_with_input(BenchmarkId::new("serde_json", name), &data, |b, data| {
            b.iter(|| black_box(serde_json::to_vec(black_box(data)).expect("ser").len()));
        });

        group.bench_with_input(BenchmarkId::new("sonic_rs", name), &data, |b, data| {
            b.iter(|| black_box(sonic_rs::to_vec(black_box(data)).expect("ser").len()));
        });

        group.bench_with_input(BenchmarkId::new("simd_json", name), &data, |b, data| {
            b.iter(|| {
                black_box(
                    simd_json::to_vec(black_box(data))
                        .expect("ser")
                        .len(),
                )
            });
        });
    }

    group.finish();
}

/// String escaping in isolation — the part of serialization that actually
/// differs between implementations, and the part Vela's `emit_v2.vl` still
/// does in quadratic time via `json_escape_string`.
fn bench_escaping(c: &mut Criterion) {
    let mut group = c.benchmark_group("string_escaping");

    #[allow(clippy::type_complexity)]
    let cases: [(&str, String); 3] = [
        ("clean", "abcdefghij".repeat(6_000)),
        (
            "sparse_escapes",
            (0..3_000).map(|_| "abcdefghij\n").collect::<String>(),
        ),
        (
            "dense_escapes",
            (0..6_000).map(|_| "a\"b\\c\n").collect::<String>(),
        ),
    ];

    for (name, s) in &cases {
        group.throughput(Throughput::Bytes(s.len() as u64));

        let mut buf = Vec::with_capacity(s.len() * 2 + 16);
        group.bench_with_input(BenchmarkId::new("vela", name), s, |b, s| {
            b.iter(|| {
                buf.clear();
                ser::write_escaped(&mut buf, black_box(s), ser::Options::default());
                black_box(buf.len())
            });
        });

        group.bench_with_input(BenchmarkId::new("serde_json", name), s, |b, s| {
            b.iter(|| black_box(serde_json::to_string(black_box(s)).expect("ser").len()));
        });

        group.bench_with_input(BenchmarkId::new("sonic_rs", name), s, |b, s| {
            b.iter(|| black_box(sonic_rs::to_string(black_box(s)).expect("ser").len()));
        });
    }

    group.finish();
}

criterion_group!(
    benches,
    bench_deserialize_owned,
    bench_deserialize_borrowed,
    bench_deserialize_partial,
    bench_serialize,
    bench_escaping
);
criterion_main!(benches);
