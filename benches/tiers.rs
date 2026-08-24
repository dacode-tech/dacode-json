//! All four Vela JSON tiers, head to head.
//!
//! Vela picks one tier at build time and the choice is essentially
//! permanent for that binary, so the question this answers is: **which one
//! should the default be, and at what document size does that change?**
//!
//! Vela's own answer is tier 1 (`BUILD.bazel:149`,
//! `"//conditions:default": glob([".../tier1/**/*.vl"])`). The design docs
//! assume tier 2 and tier 3 are strictly better and that tier 1 is the
//! fallback for size-constrained builds.
//!
//! Document size is the whole story, so every group sweeps it. An index is
//! a fixed cost amortised over the queries you run against it; run one
//! query and it is pure overhead.
//!
//! `serde_json` and `sonic-rs` appear as reference points, not as the
//! subject — they do more work (validation, real numbers), which
//! `docs/RESULTS.md` §3 covers.

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use std::hint::black_box;
use vela_json::corpus;
use vela_json::tiers::{
    tier0::Tier0, tier1::Tier1, tier2::Tier2, tier3::Tier3, JsonTier,
};
use vela_json::tiers::tier2::Tier2Cached;
use vela_json::Workspace;

const SEED: u64 = 0x71E45;

/// A flat object with `n` keys — the shape `object_get` is built for.
fn flat_object(n: usize) -> String {
    let mut s = String::from("{");
    for i in 0..n {
        if i > 0 {
            s.push(',');
        }
        s.push_str(&format!(r#""key_{i:05}":{i}"#));
    }
    s.push('}');
    s
}

/// Sizes chosen so the smallest is well below any index's break-even point
/// and the largest is well above it.
fn object_sizes() -> Vec<(&'static str, String)> {
    vec![
        ("obj_8keys", flat_object(8)),
        ("obj_64keys", flat_object(64)),
        ("obj_1k_keys", flat_object(1_024)),
        ("obj_16k_keys", flat_object(16_384)),
    ]
}

fn array_sizes() -> Vec<(&'static str, String)> {
    vec![
        ("arr_1kb", corpus::sized(1 << 10, SEED, corpus::records)),
        ("arr_64kb", corpus::sized(64 << 10, SEED, corpus::records)),
        ("arr_1mb", corpus::sized(1 << 20, SEED, corpus::records)),
    ]
}

// =====================================================================
// One field, one call
// =====================================================================

/// The single most common JSON operation: pull one field out of an object.
///
/// This is where tier 1's lack of an index should hurt most — and where the
/// per-call tape/pool rebuild in tiers 2 and 3 should hurt more.
fn bench_object_get(c: &mut Criterion) {
    let mut group = c.benchmark_group("tier_object_get");

    for (name, json) in object_sizes() {
        let src = json.as_bytes();
        group.throughput(Throughput::Bytes(src.len() as u64));

        // Ask for the last key, so a linear scan pays its full price.
        let n: usize = name
            .trim_start_matches("obj_")
            .trim_end_matches("keys")
            .trim_end_matches('_')
            .replace('k', "000")
            .parse()
            .unwrap_or(8);
        let key = format!("key_{:05}", n.saturating_sub(1));

        // Sanity: every tier that claims to handle containers must find it.
        assert!(Tier1::object_get(src, &key).is_some(), "{name}/{key}");
        assert_eq!(Tier2::object_get(src, &key), Tier1::object_get(src, &key));
        assert_eq!(Tier3::object_get(src, &key), Tier1::object_get(src, &key));

        group.bench_with_input(BenchmarkId::new("tier1_descent", name), src, |b, src| {
            b.iter(|| black_box(Tier1::object_get(black_box(src), &key).map(<[u8]>::len)));
        });
        group.bench_with_input(BenchmarkId::new("tier2_tape", name), src, |b, src| {
            b.iter(|| black_box(Tier2::object_get(black_box(src), &key).map(<[u8]>::len)));
        });
        group.bench_with_input(BenchmarkId::new("tier3_pool", name), src, |b, src| {
            b.iter(|| black_box(Tier3::object_get(black_box(src), &key).map(<[u8]>::len)));
        });

        // What tier 3 is actually for: parse once, then query.
        let mut ws = Workspace::with_capacity(src.len());
        let _ = ws.parse(src);
        group.bench_with_input(BenchmarkId::new("tier3_workspace", name), src, |b, src| {
            b.iter(|| {
                let doc = ws.parse(black_box(src));
                black_box(doc.root().get(&key).map(|v| v.index()))
            });
        });

        group.bench_with_input(BenchmarkId::new("serde_json", name), src, |b, src| {
            b.iter(|| {
                let v: serde_json::Value =
                    serde_json::from_slice(black_box(src)).expect("valid");
                black_box(v.get(&key).is_some())
            });
        });
    }

    group.finish();
}

/// Reading *k* fields from one document.
///
/// Tier 1 re-scans per lookup, so this is O(k·n) — the cost that an index
/// exists to remove. Whether it does depends entirely on whether the index
/// survives between calls.
fn bench_multi_field(c: &mut Criterion) {
    let mut group = c.benchmark_group("tier_multi_field");

    let json = flat_object(256);
    let src = json.as_bytes();
    group.throughput(Throughput::Bytes(src.len() as u64));

    for k in [1usize, 4, 16, 64] {
        let keys: Vec<String> = (0..k).map(|i| format!("key_{:05}", i * 4)).collect();

        group.bench_with_input(BenchmarkId::new("tier1_descent", k), &keys, |b, keys| {
            b.iter(|| {
                let mut acc = 0usize;
                for key in keys {
                    acc += Tier1::object_get(black_box(src), key).map_or(0, <[u8]>::len);
                }
                black_box(acc)
            });
        });

        group.bench_with_input(BenchmarkId::new("tier2_per_call", k), &keys, |b, keys| {
            b.iter(|| {
                let mut acc = 0usize;
                for key in keys {
                    acc += Tier2::object_get(black_box(src), key).map_or(0, <[u8]>::len);
                }
                black_box(acc)
            });
        });

        // The same tape, built once.
        let mut cached = Tier2Cached::with_capacity(src.len());
        group.bench_with_input(BenchmarkId::new("tier2_cached", k), &keys, |b, keys| {
            b.iter(|| {
                let doc = cached.parse(black_box(src));
                let mut acc = 0usize;
                for key in keys {
                    acc += doc.object_get(key).map_or(0, <[u8]>::len);
                }
                black_box(acc)
            });
        });

        let mut ws = Workspace::with_capacity(src.len());
        let _ = ws.parse(src);
        group.bench_with_input(BenchmarkId::new("tier3_workspace", k), &keys, |b, keys| {
            b.iter(|| {
                let doc = ws.parse(black_box(src));
                let root = doc.root();
                let mut acc = 0usize;
                for key in keys {
                    acc += root.get(key).map_or(0, |v| v.index());
                }
                black_box(acc)
            });
        });
    }

    group.finish();
}

// =====================================================================
// Whole-document operations
// =====================================================================

fn bench_counts(c: &mut Criterion) {
    let mut group = c.benchmark_group("tier_array_count");

    for (name, json) in array_sizes() {
        let src = json.as_bytes();
        group.throughput(Throughput::Bytes(src.len() as u64));

        let want = Tier1::array_count(src);
        assert_eq!(Tier2::array_count(src), want);
        assert_eq!(Tier3::array_count(src), want);

        group.bench_with_input(BenchmarkId::new("tier0_stub", name), src, |b, src| {
            b.iter(|| black_box(Tier0::array_count(black_box(src))));
        });
        group.bench_with_input(BenchmarkId::new("tier1_descent", name), src, |b, src| {
            b.iter(|| black_box(Tier1::array_count(black_box(src))));
        });
        group.bench_with_input(BenchmarkId::new("tier2_tape", name), src, |b, src| {
            b.iter(|| black_box(Tier2::array_count(black_box(src))));
        });
        group.bench_with_input(BenchmarkId::new("tier3_pool", name), src, |b, src| {
            b.iter(|| black_box(Tier3::array_count(black_box(src))));
        });

        let mut ws = Workspace::with_capacity(src.len());
        let _ = ws.parse(src);
        group.bench_with_input(BenchmarkId::new("tier3_workspace", name), src, |b, src| {
            b.iter(|| black_box(ws.parse(black_box(src)).root().len()));
        });
    }

    group.finish();
}

fn bench_validate(c: &mut Criterion) {
    let mut group = c.benchmark_group("tier_validate");

    for (name, json) in array_sizes() {
        let src = json.as_bytes();
        group.throughput(Throughput::Bytes(src.len() as u64));

        group.bench_with_input(BenchmarkId::new("tier1_descent", name), src, |b, src| {
            b.iter(|| black_box(Tier1::validate(black_box(src))));
        });
        group.bench_with_input(BenchmarkId::new("tier2_tape", name), src, |b, src| {
            b.iter(|| black_box(Tier2::validate(black_box(src))));
        });
        group.bench_with_input(BenchmarkId::new("tier3_pool", name), src, |b, src| {
            b.iter(|| black_box(Tier3::validate(black_box(src))));
        });

        // The only one that actually validates.
        let mut strict = vela_json::strict::StrictParser::with_capacity(src.len());
        let _ = strict.validate(src);
        group.bench_with_input(BenchmarkId::new("strict_rfc8259", name), src, |b, src| {
            b.iter(|| black_box(strict.validate(black_box(src)).is_ok()));
        });

        group.bench_with_input(BenchmarkId::new("serde_json", name), src, |b, src| {
            b.iter(|| {
                black_box(serde_json::from_slice::<serde_json::Value>(black_box(src)).is_ok())
            });
        });
    }

    group.finish();
}

/// DOM construction: tier 2's tape against tier 3's pool.
///
/// The only fair comparison between the two indexed tiers, with the
/// per-call rebuild taken out of the picture on both sides.
fn bench_dom_build(c: &mut Criterion) {
    let mut group = c.benchmark_group("tier_dom_build");
    group.sample_size(40);

    for (name, json) in array_sizes() {
        let src = json.as_bytes();
        group.throughput(Throughput::Bytes(src.len() as u64));

        let mut tape_fresh = vela_json::tiers::tier2::tape::TapeBuilder::new();
        group.bench_with_input(BenchmarkId::new("tier2_tape_fresh", name), src, |b, src| {
            b.iter(|| {
                // Fresh builder each time = Vela's json_tape_build.
                let mut t = vela_json::tiers::tier2::tape::TapeBuilder::new();
                black_box(t.build(black_box(src)).len())
            });
        });
        let _ = tape_fresh.build(src);
        group.bench_with_input(BenchmarkId::new("tier2_tape_reused", name), src, |b, src| {
            b.iter(|| black_box(tape_fresh.build(black_box(src)).len()));
        });

        group.bench_with_input(BenchmarkId::new("tier3_pool_fresh", name), src, |b, src| {
            b.iter(|| black_box(vela_json::parse_to_pool(black_box(src)).len()));
        });

        let mut ws = Workspace::with_capacity(src.len());
        let _ = ws.parse(src);
        group.bench_with_input(BenchmarkId::new("tier3_pool_reused", name), src, |b, src| {
            b.iter(|| black_box(ws.parse(black_box(src)).pool().len()));
        });
    }

    group.finish();
}

// =====================================================================
// Scalars — the only operations tier 0 can do
// =====================================================================

fn bench_scalars(c: &mut Criterion) {
    let mut group = c.benchmark_group("tier_scalars");

    let cases: [(&str, &[u8]); 4] = [
        ("number", b"-1234567890"),
        ("bool", b"true"),
        ("short_string", br#""hello world""#),
        ("escaped_string", br#""a\nb\tc\"d\\e\u0041f""#),
    ];

    for (name, src) in cases {
        group.bench_with_input(BenchmarkId::new("parse_string", name), src, |b, src| {
            b.iter(|| black_box(Tier0::parse_string(black_box(src))));
        });
        group.bench_with_input(BenchmarkId::new("parse_number", name), src, |b, src| {
            b.iter(|| black_box(Tier0::parse_number(black_box(src))));
        });
        group.bench_with_input(BenchmarkId::new("detect_type", name), src, |b, src| {
            b.iter(|| black_box(Tier0::detect_type(black_box(src))));
        });
        group.bench_with_input(BenchmarkId::new("validate_string", name), src, |b, src| {
            b.iter(|| black_box(Tier0::validate_string(black_box(src))));
        });
    }

    // Long string: where Vela's O(n^2) builder would fall over.
    let long = format!("\"{}\"", "abcdefghij".repeat(3_000));
    group.throughput(Throughput::Bytes(long.len() as u64));
    group.bench_function("parse_string/30kb_clean", |b| {
        b.iter(|| black_box(Tier0::parse_string(black_box(long.as_bytes())).len()));
    });

    group.finish();
}

criterion_group!(
    benches,
    bench_object_get,
    bench_multi_field,
    bench_counts,
    bench_validate,
    bench_dom_build,
    bench_scalars
);
criterion_main!(benches);
