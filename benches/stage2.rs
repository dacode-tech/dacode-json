//! Stage 2 in isolation — the DOM builder, with Stage 1 already done.
//!
//! `docs/PROFILING.md` §3 shows simdjson spending 37–43% of its time in
//! Stage 1 where this port spends 13–25%. Same two-stage architecture, so
//! the port's Stage 2 must be proportionally much more expensive. This
//! measures it directly, with the structural index pre-computed and reused,
//! so nothing but the builder is timed.

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use dacode_json::builder::{build_from_index, pool_capacity_for, Stack};
use dacode_json::corpus;
use dacode_json::pool::Pool;
use dacode_json::scan::{scan, Scanner, StructuralIndex};
use std::hint::black_box;

const SEED: u64 = 0x5CA7;

/// Everything Stage 2 needs, prepared once.
struct Prepared {
    input: Vec<u8>,
    si: StructuralIndex,
    pool: Pool,
    stack: Stack,
}

fn prepare(json: &str) -> Prepared {
    let input = json.as_bytes().to_vec();
    let si = scan(Scanner::default(), &input);
    let mut pool = Pool::with_capacity(pool_capacity_for(si.len()));
    let mut stack = Stack::new();
    // Warm: fault the pages in and settle the capacity.
    pool.reset(input.len());
    build_from_index(&input, &si, &mut pool, &mut stack);
    Prepared {
        input,
        si,
        pool,
        stack,
    }
}

#[inline(never)]
fn w_build(p: &mut Prepared) -> usize {
    p.pool.reset(p.input.len());
    build_from_index(&p.input, &p.si, &mut p.pool, &mut p.stack);
    p.pool.len()
}

/// Stage 2 alone, per corpus.
fn bench_stage2(c: &mut Criterion) {
    let mut group = c.benchmark_group("stage2_build");
    group.sample_size(50);

    for (name, json) in corpus::suite(1 << 20, SEED) {
        let mut p = prepare(&json);
        let bytes = p.input.len();
        let nodes = p.pool.len();
        let structurals = p.si.len();
        println!(
            "  {name:10} {bytes:>9} B  {structurals:>8} structurals  {nodes:>8} nodes  \
             ({:.1} B/node)",
            bytes as f64 / nodes.max(1) as f64
        );

        group.throughput(Throughput::Bytes(bytes as u64));
        group.bench_with_input(BenchmarkId::new("build", name), &name, |b, _| {
            b.iter(|| black_box(w_build(&mut p)));
        });
    }

    group.finish();
}

/// Stage 1 and Stage 2 side by side on the same corpus, so their shares are
/// directly comparable.
fn bench_split(c: &mut Criterion) {
    let mut group = c.benchmark_group("stage_split");
    group.sample_size(50);

    for (name, json) in corpus::suite(1 << 20, SEED) {
        let mut p = prepare(&json);
        let bytes = p.input.len();
        group.throughput(Throughput::Bytes(bytes as u64));

        let mut idx = StructuralIndex::with_capacity(bytes + 64);
        let input = p.input.clone();
        group.bench_with_input(BenchmarkId::new("stage1_scan", name), &name, |b, _| {
            b.iter(|| {
                idx.clear();
                dacode_json::scan::scan_into(Scanner::default(), black_box(&input), &mut idx);
                black_box(idx.len())
            });
        });

        group.bench_with_input(BenchmarkId::new("stage2_build", name), &name, |b, _| {
            b.iter(|| black_box(w_build(&mut p)));
        });
    }

    group.finish();
}

/// The floor for *any* index-fed builder.
///
/// Walks the structural index exactly as the real builder does — read the
/// position, read the byte at it, dispatch on it — and then does no work at
/// all. No pool writes, no scalar parsing, no stack.
///
/// This bounds what a smarter Stage 2 could ever achieve. If the floor is
/// already close to the real builder's throughput, the dispatch is not the
/// problem and rewriting it cannot help.
fn bench_floor(c: &mut Criterion) {
    let mut group = c.benchmark_group("stage2_floor");
    group.sample_size(50);

    #[inline(never)]
    fn walk_only(input: &[u8], positions: &[u32]) -> usize {
        // Same shape as the real loop: one position load, one input load at
        // a data-dependent offset, one dispatch. Accumulate so nothing can
        // be optimised away.
        let mut acc = 0usize;
        for &p in positions {
            let pos = p as usize;
            let ch = input.get(pos).copied().unwrap_or(0);
            acc += match ch {
                b'{' => 1,
                b'}' => 2,
                b'[' => 3,
                b']' => 4,
                b'"' => 5,
                b':' => 6,
                b',' => 7,
                _ => 8,
            };
        }
        acc
    }

    /// Even cheaper: never touch `input` at all, only the index. Isolates
    /// the cost of the data-dependent load into the document.
    #[inline(never)]
    fn positions_only(positions: &[u32]) -> usize {
        let mut acc = 0usize;
        for &p in positions {
            acc += p as usize & 7;
        }
        acc
    }

    /// The ceiling for "pack the class into the index": dispatch from a
    /// pre-computed 3-bit class with no access to the document at all.
    #[inline(never)]
    fn walk_preclassified(packed: &[u32]) -> usize {
        let mut acc = 0usize;
        for &w in packed {
            acc += match w & 7 {
                0 => 1,
                1 => 2,
                2 => 3,
                3 => 4,
                4 => 5,
                5 => 6,
                6 => 7,
                _ => 8,
            };
        }
        acc
    }

    for (name, json) in corpus::suite(1 << 20, SEED) {
        let p = prepare(&json);
        let bytes = p.input.len();
        group.throughput(Throughput::Bytes(bytes as u64));

        let input = p.input.clone();
        let pos = p.si.positions().to_vec();
        // (position << 3) | class, as the packed index would store it.
        let packed: Vec<u32> = pos
            .iter()
            .map(|&pp| {
                let c = match input.get(pp as usize).copied().unwrap_or(0) {
                    b'{' => 0u32,
                    b'}' => 1,
                    b'[' => 2,
                    b']' => 3,
                    b'"' => 4,
                    b':' => 5,
                    b',' => 6,
                    _ => 7,
                };
                (pp << 3) | c
            })
            .collect();

        group.bench_with_input(BenchmarkId::new("positions_only", name), &name, |b, _| {
            b.iter(|| black_box(positions_only(black_box(&pos))));
        });
        group.bench_with_input(BenchmarkId::new("walk_dispatch", name), &name, |b, _| {
            b.iter(|| black_box(walk_only(black_box(&input), black_box(&pos))));
        });
        group.bench_with_input(
            BenchmarkId::new("walk_preclassified", name),
            &name,
            |b, _| {
                b.iter(|| black_box(walk_preclassified(black_box(&packed))));
            },
        );
        let mut q = prepare(&json);
        group.bench_with_input(BenchmarkId::new("real_builder", name), &name, |b, _| {
            b.iter(|| black_box(w_build(&mut q)));
        });
    }

    group.finish();
}

/// One pass versus two: the whole point of porting `parse_onepass.vl`.
///
/// `indexed_full` is Stage 1 + Stage 2 with all buffers reused — the
/// fastest form of the two-stage path. `onepass` reads the document once.
/// Both produce a byte-identical pool (`tests/onepass.rs`).
/// Without `vela-compat` the single-pass builder is not compiled in, so
/// this group is empty rather than absent - keeps the criterion group list
/// stable across feature sets.
#[cfg(not(feature = "vela-compat"))]
fn bench_onepass(_c: &mut Criterion) {}

#[cfg(feature = "vela-compat")]
fn bench_onepass(c: &mut Criterion) {
    use dacode_json::{onepass::OnePass, Workspace};

    #[inline(never)]
    fn w_indexed(ws: &mut Workspace, src: &[u8]) -> usize {
        ws.parse(src).pool().len()
    }
    #[inline(never)]
    fn w_onepass(p: &mut OnePass, src: &[u8]) -> usize {
        p.parse(src).pool().len()
    }

    let mut group = c.benchmark_group("onepass_vs_indexed");
    group.sample_size(50);

    for (name, json) in corpus::suite(1 << 20, SEED) {
        let src = json.as_bytes();
        group.throughput(Throughput::Bytes(src.len() as u64));

        let mut ws = Workspace::with_capacity(src.len());
        let _ = ws.parse(src);
        group.bench_with_input(BenchmarkId::new("indexed_full", name), &name, |b, _| {
            b.iter(|| black_box(w_indexed(&mut ws, black_box(src))));
        });

        let mut op = OnePass::with_capacity(src.len());
        let _ = op.parse(src);
        group.bench_with_input(BenchmarkId::new("onepass", name), &name, |b, _| {
            b.iter(|| black_box(w_onepass(&mut op, black_box(src))));
        });
    }

    group.finish();
}

criterion_group!(
    benches,
    bench_onepass,
    bench_floor,
    bench_stage2,
    bench_split
);
criterion_main!(benches);
