//! Zero-copy access: `jsonflat` vs `rkyv` vs re-parsing JSON.
//!
//! The question a zero-copy format answers is not "how fast can you parse"
//! but "how fast can you *read* data you already have". So the shape of
//! this benchmark is:
//!
//! * **open** — turn a `&[u8]` into something queryable. For JSON parsers
//!   this is a full parse; for `jsonflat` and `rkyv` it is a header check.
//! * **read_one** — open, then read a single field. Dominated by open cost.
//! * **read_all** — open, then walk every record. Dominated by access cost.
//! * **amortised** — open once, read N times. The curve that decides
//!   whether a zero-copy format is worth its size.
//! * **encode** — the write side, which zero-copy formats make *more*
//!   expensive.
//! * **size** — printed, not timed. The price of the whole idea.
//!
//! `rkyv` is the fair opponent: a real zero-copy framework with a fixed
//! schema. It should win, because it knows the shape of the data at compile
//! time and `jsonflat` does not — `jsonflat` still carries key strings and
//! per-node type tags. The interesting question is by how much.

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use rkyv::{rancor::Error as RkyvError, Archive, Deserialize as RkyvDe, Serialize as RkyvSer};
use serde::{Deserialize, Serialize};
use std::hint::black_box;
use vela_json::flat::typed::{TypedView, TypedWriter};
use vela_json::flat::{self, View};
use vela_json::flat_struct;
use vela_json::strict::StrictParser;
use vela_json::{corpus, Workspace};

const SEED: u64 = 0x2C0;

#[derive(Debug, Serialize, Deserialize, Archive, RkyvSer, RkyvDe, PartialEq)]
struct Record {
    id: u64,
    name: String,
    age: u32,
    active: bool,
    city: String,
    score: i64,
    tags: Vec<String>,
}

flat_struct! {
    /// Schema-driven layout of the records corpus: no key strings stored.
    pub struct FlatRecord : FlatRecordFields {
        id: u64,
        age: u32,
        active: bool,
        score: i64,
        name: str,
        city: str,
        tags: [str],
    }
}

/// Build the typed buffer from already-parsed records.
fn encode_typed(records: &[Record]) -> Vec<u8> {
    let mut w = TypedWriter::<FlatRecord>::new();
    for r in records {
        w.record()
            .u64(r.id)
            .u32(r.age)
            .bool(r.active)
            .i64(r.score)
            .str(&r.name)
            .str(&r.city)
            .str_list(r.tags.iter().map(String::as_str));
    }
    w.finish()
}

fn corpus_json() -> String {
    corpus::sized(1 << 20, SEED, corpus::records)
}

fn encode_flat(json: &str) -> Vec<u8> {
    let mut p = StrictParser::new();
    let doc = p.parse(json.as_bytes()).expect("valid");
    flat::encode(doc).expect("encode")
}

fn encode_rkyv(records: &Vec<Record>) -> Vec<u8> {
    rkyv::to_bytes::<RkyvError>(records)
        .expect("rkyv encode")
        .to_vec()
}

/// Sum one integer field across every record — the read workload.
fn expected_sum(records: &[Record]) -> i64 {
    records.iter().map(|r| r.score).sum()
}

fn report_sizes(json: &str, flatbuf: &[u8], typedbuf: &[u8], rkyvbuf: &[u8]) {
    println!("\n  --- buffer sizes (1 MiB corpus) ---");
    let j = json.len() as f64;
    println!("  json           {:>10} bytes  1.00x", json.len());
    println!(
        "  jsonflat dyn   {:>10} bytes  {:.2}x",
        flatbuf.len(),
        flatbuf.len() as f64 / j
    );
    println!(
        "  jsonflat typed {:>10} bytes  {:.2}x",
        typedbuf.len(),
        typedbuf.len() as f64 / j
    );
    println!(
        "  rkyv           {:>10} bytes  {:.2}x",
        rkyvbuf.len(),
        rkyvbuf.len() as f64 / j
    );
    println!(
        "  vela pool      {:>10} bytes  {:.2}x  (nodes only, needs the input too)",
        {
            let mut w = Workspace::new();
            w.parse_to_pool(json.as_bytes()).len() * 16
        },
        {
            let mut w = Workspace::new();
            (w.parse_to_pool(json.as_bytes()).len() * 16) as f64 / j
        }
    );
    println!();
}

/// Turning bytes into something you can query.
fn bench_open(c: &mut Criterion) {
    let json = corpus_json();
    let records: Vec<Record> = serde_json::from_str(&json).expect("parse");
    let flatbuf = encode_flat(&json);
    let rkyvbuf = encode_rkyv(&records);
    let typedbuf = encode_typed(&records);
    report_sizes(&json, &flatbuf, &typedbuf, &rkyvbuf);

    let mut group = c.benchmark_group("zc_open");
    group.throughput(Throughput::Bytes(json.len() as u64));

    group.bench_function("jsonflat_header_only", |b| {
        b.iter(|| {
            let v = View::new(black_box(&flatbuf)).expect("view");
            black_box(v.len())
        });
    });

    group.bench_function("jsonflat_validate_deep", |b| {
        b.iter(|| {
            let v = View::new(black_box(&flatbuf)).expect("view");
            v.validate_deep().expect("valid");
            black_box(v.len())
        });
    });

    group.bench_function("jsonflat_typed_header_only", |b| {
        b.iter(|| {
            let v = TypedView::<FlatRecord>::new(black_box(&typedbuf)).expect("view");
            black_box(v.len())
        });
    });

    group.bench_function("rkyv_access_unchecked", |b| {
        b.iter(|| {
            // SAFETY: the buffer was produced by `encode_rkyv` in this
            // process and is not mutated. This is rkyv's trusted-input
            // path, the counterpart to `jsonflat_header_only`.
            let a = unsafe { rkyv::access_unchecked::<rkyv::Archived<Vec<Record>>>(black_box(&rkyvbuf)) };
            black_box(a.len())
        });
    });

    group.bench_function("rkyv_access_checked", |b| {
        b.iter(|| {
            let a = rkyv::access::<rkyv::Archived<Vec<Record>>, RkyvError>(black_box(&rkyvbuf))
                .expect("access");
            black_box(a.len())
        });
    });

    let mut ws = Workspace::with_capacity(json.len());
    let _ = ws.parse(json.as_bytes());
    group.bench_function("vela_pool_parse", |b| {
        b.iter(|| black_box(ws.parse(black_box(json.as_bytes())).pool().len()));
    });

    group.bench_function("serde_json_parse", |b| {
        b.iter(|| {
            let v: serde_json::Value = serde_json::from_slice(black_box(json.as_bytes()))
                .expect("parse");
            black_box(v)
        });
    });

    group.finish();
}

/// Open, then read a single field. Open cost dominates.
fn bench_read_one(c: &mut Criterion) {
    let json = corpus_json();
    let records: Vec<Record> = serde_json::from_str(&json).expect("parse");
    let flatbuf = encode_flat(&json);
    let typedbuf = encode_typed(&records);
    let rkyvbuf = encode_rkyv(&records);
    let want = records.first().map(|r| r.score).unwrap_or(0);

    let mut group = c.benchmark_group("zc_read_one");

    group.bench_function("jsonflat", |b| {
        b.iter(|| {
            let v = View::new(black_box(&flatbuf)).expect("view");
            let got = v
                .root()
                .at(0)
                .and_then(|r| r.get("score"))
                .and_then(|s| s.as_i64());
            debug_assert_eq!(got, Some(want));
            black_box(got)
        });
    });

    group.bench_function("jsonflat_typed", |b| {
        b.iter(|| {
            let v = TypedView::<FlatRecord>::new(black_box(&typedbuf)).expect("view");
            let got = v.score(0);
            debug_assert_eq!(got, Some(want));
            black_box(got)
        });
    });

    group.bench_function("rkyv_unchecked", |b| {
        b.iter(|| {
            // SAFETY: buffer produced in-process, never mutated.
            let a = unsafe { rkyv::access_unchecked::<rkyv::Archived<Vec<Record>>>(black_box(&rkyvbuf)) };
            black_box(a.first().map(|r| r.score.to_native()))
        });
    });

    let mut ws = Workspace::with_capacity(json.len());
    let _ = ws.parse(json.as_bytes());
    group.bench_function("vela_pool", |b| {
        b.iter(|| {
            let doc = ws.parse(black_box(json.as_bytes()));
            black_box(
                doc.root()
                    .at(0)
                    .and_then(|r| r.get("score"))
                    .and_then(|s| s.as_i64()),
            )
        });
    });

    group.bench_function("serde_json", |b| {
        b.iter(|| {
            let v: serde_json::Value = serde_json::from_slice(black_box(json.as_bytes()))
                .expect("parse");
            black_box(v.get(0).and_then(|r| r.get("score")).and_then(|s| s.as_i64()))
        });
    });

    group.finish();
}

/// Open, then touch every record.
fn bench_read_all(c: &mut Criterion) {
    let json = corpus_json();
    let records: Vec<Record> = serde_json::from_str(&json).expect("parse");
    let flatbuf = encode_flat(&json);
    let typedbuf = encode_typed(&records);
    let rkyvbuf = encode_rkyv(&records);
    let want = expected_sum(&records);

    let mut group = c.benchmark_group("zc_read_all");
    group.throughput(Throughput::Bytes(json.len() as u64));

    group.bench_function("jsonflat", |b| {
        b.iter(|| {
            let v = View::new(black_box(&flatbuf)).expect("view");
            let sum: i64 = v
                .root()
                .elements()
                .filter_map(|r| r.get("score")?.as_i64())
                .sum();
            debug_assert_eq!(sum, want);
            black_box(sum)
        });
    });

    group.bench_function("jsonflat_typed", |b| {
        b.iter(|| {
            let v = TypedView::<FlatRecord>::new(black_box(&typedbuf)).expect("view");
            let sum: i64 = (0..v.len()).filter_map(|i| v.score(i)).sum();
            debug_assert_eq!(sum, want);
            black_box(sum)
        });
    });

    group.bench_function("rkyv_unchecked", |b| {
        b.iter(|| {
            // SAFETY: buffer produced in-process, never mutated.
            let a = unsafe { rkyv::access_unchecked::<rkyv::Archived<Vec<Record>>>(black_box(&rkyvbuf)) };
            let sum: i64 = a.iter().map(|r| r.score.to_native()).sum();
            debug_assert_eq!(sum, want);
            black_box(sum)
        });
    });

    let mut ws = Workspace::with_capacity(json.len());
    let _ = ws.parse(json.as_bytes());
    group.bench_function("vela_pool", |b| {
        b.iter(|| {
            let doc = ws.parse(black_box(json.as_bytes()));
            let sum: i64 = doc
                .root()
                .elements()
                .filter_map(|r| r.get("score")?.as_i64())
                .sum();
            black_box(sum)
        });
    });

    group.bench_function("serde_json", |b| {
        b.iter(|| {
            let v: serde_json::Value = serde_json::from_slice(black_box(json.as_bytes()))
                .expect("parse");
            let sum: i64 = v
                .as_array()
                .map(|a| a.iter().filter_map(|r| r.get("score")?.as_i64()).sum())
                .unwrap_or(0);
            black_box(sum)
        });
    });

    group.finish();
}

/// Read N times from one buffer. This is the curve that matters: JSON
/// parsers pay their cost on every open, zero-copy formats pay once.
fn bench_amortised(c: &mut Criterion) {
    let json = corpus_json();
    let flatbuf = encode_flat(&json);

    let mut group = c.benchmark_group("zc_amortised");

    for reads in [1usize, 4, 16, 64] {
        group.bench_with_input(BenchmarkId::new("jsonflat", reads), &reads, |b, &n| {
            b.iter(|| {
                let mut acc = 0i64;
                for _ in 0..n {
                    let v = View::new(black_box(&flatbuf)).expect("view");
                    acc = acc.wrapping_add(
                        v.root()
                            .at(0)
                            .and_then(|r| r.get("score"))
                            .and_then(|s| s.as_i64())
                            .unwrap_or(0),
                    );
                }
                black_box(acc)
            });
        });

        let mut ws = Workspace::with_capacity(json.len());
        let _ = ws.parse(json.as_bytes());
        group.bench_with_input(BenchmarkId::new("vela_pool", reads), &reads, |b, &n| {
            b.iter(|| {
                let mut acc = 0i64;
                for _ in 0..n {
                    let doc = ws.parse(black_box(json.as_bytes()));
                    acc = acc.wrapping_add(
                        doc.root()
                            .at(0)
                            .and_then(|r| r.get("score"))
                            .and_then(|s| s.as_i64())
                            .unwrap_or(0),
                    );
                }
                black_box(acc)
            });
        });

        group.bench_with_input(BenchmarkId::new("serde_json", reads), &reads, |b, &n| {
            b.iter(|| {
                let mut acc = 0i64;
                for _ in 0..n {
                    let v: serde_json::Value =
                        serde_json::from_slice(black_box(json.as_bytes())).expect("parse");
                    acc = acc.wrapping_add(
                        v.get(0)
                            .and_then(|r| r.get("score"))
                            .and_then(|s| s.as_i64())
                            .unwrap_or(0),
                    );
                }
                black_box(acc)
            });
        });
    }

    group.finish();
}

/// The write side. Zero-copy formats move work here.
fn bench_encode(c: &mut Criterion) {
    let json = corpus_json();
    let records: Vec<Record> = serde_json::from_str(&json).expect("parse");

    let mut group = c.benchmark_group("zc_encode");
    group.throughput(Throughput::Bytes(json.len() as u64));

    let mut p = StrictParser::with_capacity(json.len());
    let _ = p.validate(json.as_bytes());

    // Three interning modes plus unsorted, to price each build-time choice.
    for (label, builder) in [
        ("jsonflat_intern_none", flat::Builder::new().intern(flat::Intern::None)),
        ("jsonflat_intern_keys", flat::Builder::new().intern(flat::Intern::Keys)),
        ("jsonflat_intern_all", flat::Builder::new().intern(flat::Intern::All)),
        (
            "jsonflat_unsorted_keys",
            flat::Builder::new()
                .intern(flat::Intern::Keys)
                .sort_keys(false),
        ),
    ] {
        group.bench_function(label, |b| {
            b.iter(|| {
                let doc = p.parse(black_box(json.as_bytes())).expect("valid");
                black_box(builder.build(doc).expect("encode").len())
            });
        });
    }

    // The parse alone, so the build cost can be separated from it.
    group.bench_function("strict_parse_only", |b| {
        b.iter(|| black_box(p.parse(black_box(json.as_bytes())).expect("valid").pool().len()));
    });

    group.bench_function("jsonflat_typed_from_structs", |b| {
        b.iter(|| black_box(encode_typed(black_box(&records)).len()));
    });

    group.bench_function("rkyv_from_structs", |b| {
        b.iter(|| black_box(encode_rkyv(black_box(&records)).len()));
    });

    group.bench_function("serde_json_from_structs", |b| {
        b.iter(|| black_box(serde_json::to_vec(black_box(&records)).expect("ser").len()));
    });

    group.finish();
}

criterion_group!(
    benches,
    bench_open,
    bench_read_one,
    bench_read_all,
    bench_amortised,
    bench_encode
);
criterion_main!(benches);
