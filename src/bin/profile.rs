//! Profiling target: runs one named workload in a loop so a sampling
//! profiler has something to attach to.
//!
//! Usage: `profile <workload> [iterations]`
//! Run under a profiler with, for example:
//!   samply record -- ./target/release/profile vela_de 200

// Panic-freedom is a property of what ships, and these binaries ship in
// the repository even if not in the crate. `docs/UNWRAP_FREE.md` §5
// recommends denying these for library code and leaving tests alone; a
// `main` is neither, and `fn main() -> Result<_, _>` makes it free.
#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unreachable,
    clippy::indexing_slicing,
    clippy::unwrap_in_result,
    clippy::exit
)]

use std::env;
use std::hint::black_box;
use dacode_json::strict::StrictParser;
use dacode_json::{corpus, de, flat, ser, Workspace};

#[derive(Debug, serde::Serialize, serde::Deserialize, PartialEq)]
struct Record {
    id: u64,
    name: String,
    age: u32,
    active: bool,
    city: String,
    score: i64,
    tags: Vec<String>,
}

fn main() -> Result<(), Fatal> {
    run().map_err(|e| Fatal(e.to_string()))
}

/// A fatal error, rendered as a message rather than as a literal.
///
/// `main` returning `Err(e)` prints `Error: {e:?}` and exits 1 — no
/// panic, no `process::exit`. But the `Debug` of a `String` inside a
/// `Box<dyn Error>` prints with quotes and escaped inner quotes, which
/// looks like a bug report rather than a diagnostic. Delegating `Debug`
/// to `Display` fixes that, and keeping it at the `main` boundary lets
/// everything below use `?` on whatever error it has.
struct Fatal(String);

impl std::fmt::Debug for Fatal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = env::args().collect();
    let workload = args.get(1).map(String::as_str).unwrap_or("list");
    let iters: usize = args
        .get(2)
        .and_then(|s| s.parse().ok())
        .unwrap_or(200);

    let json = corpus::sized(1 << 20, 0x2C0, corpus::records);
    let bytes = json.as_bytes();

    let workloads = [
        "vela_scan", "vela_stage2", "vela_pool", "vela_strict", "vela_de", "vela_ser",
        "serde_json_value", "serde_json_de", "serde_json_ser",
        "simd_json_tape", "sonic_de", "flat_encode", "flat_read",
        #[cfg(feature = "cbench")]
        "yyjson", 
        #[cfg(feature = "cbench")]
        "yyjson_sum",
        #[cfg(feature = "cbench")]
        "simdjson_dom",
        #[cfg(feature = "cbench")]
        "simdjson_ondemand",
    ];

    if workload == "list" {
        println!("workloads: {}", workloads.join(", "));
        return Ok(());
    }

    eprintln!("running {workload} x{iters} over {} bytes", bytes.len());

    match workload {
        "vela_scan" => {
            let mut idx = dacode_json::StructuralIndex::with_capacity(bytes.len() + 64);
            for _ in 0..iters {
                idx.clear();
                dacode_json::scan::scan_into(dacode_json::Scanner::default(), bytes, &mut idx);
                black_box(idx.len());
            }
        }
        // Stage 2 alone: index pre-built and reused, so only the DOM
        // builder is sampled.
        "vela_stage2" => {
            use dacode_json::builder::{build_from_index, pool_capacity_for, Stack};
            use dacode_json::pool::Pool;
            let si = dacode_json::scan::scan(dacode_json::Scanner::default(), bytes);
            let mut pool = Pool::with_capacity(pool_capacity_for(si.len()));
            let mut stack = Stack::new();
            for _ in 0..iters {
                pool.reset(bytes.len());
                build_from_index(bytes, &si, &mut pool, &mut stack);
                black_box(pool.len());
            }
        }
        "vela_pool" => {
            let mut ws = Workspace::with_capacity(bytes.len());
            for _ in 0..iters {
                black_box(ws.parse(bytes).pool().len());
            }
        }
        "vela_strict" => {
            let mut p = StrictParser::with_capacity(bytes.len());
            for _ in 0..iters {
                black_box(p.parse(bytes)?.pool().len());
            }
        }
        "vela_de" => {
            let mut p = StrictParser::with_capacity(bytes.len());
            for _ in 0..iters {
                let doc = p.parse(bytes)?;
                let v: Vec<Record> = de::from_doc(doc)?;
                black_box(v.len());
            }
        }
        "stream_de" => {
            let mut idx = dacode_json::stream::Index::default();
            idx.reserve_for(bytes.len());
            for _ in 0..iters {
                let v: Vec<Record> = dacode_json::stream::from_slice_with(&mut idx, bytes)?;
                black_box(v.len());
            }
        }
        "vela_ser" => {
            let data: Vec<Record> = serde_json::from_slice(bytes)?;
            let mut out = Vec::with_capacity(bytes.len() + 64);
            for _ in 0..iters {
                out.clear();
                ser::to_writer(&mut out, &data)?;
                black_box(out.len());
            }
        }
        "serde_json_value" => {
            for _ in 0..iters {
                let v: serde_json::Value = serde_json::from_slice(bytes)?;
                black_box(v);
            }
        }
        "serde_json_de" => {
            for _ in 0..iters {
                let v: Vec<Record> = serde_json::from_slice(bytes)?;
                black_box(v.len());
            }
        }
        "serde_json_ser" => {
            let data: Vec<Record> = serde_json::from_slice(bytes)?;
            for _ in 0..iters {
                black_box(serde_json::to_vec(&data)?.len());
            }
        }
        "simd_json_tape" => {
            let mut buffers = simd_json::Buffers::new(bytes.len());
            for _ in 0..iters {
                let mut buf = bytes.to_vec();
                let t = simd_json::to_tape_with_buffers(&mut buf, &mut buffers)?;
                black_box(t.as_value().is_object());
            }
        }
        "sonic_de" => {
            for _ in 0..iters {
                let v: Vec<Record> = sonic_rs::from_slice(bytes)?;
                black_box(v.len());
            }
        }
        "flat_encode" => {
            let mut p = StrictParser::with_capacity(bytes.len());
            for _ in 0..iters {
                let doc = p.parse(bytes)?;
                black_box(flat::encode(doc)?.len());
            }
        }
        "flat_read" => {
            let mut p = StrictParser::with_capacity(bytes.len());
            let buf = flat::encode(p.parse(bytes)?)?;
            for _ in 0..iters * 20 {
                let v = flat::View::new(&buf)?;
                let sum: i64 = v
                    .root()
                    .elements()
                    .filter_map(|r| r.get("score")?.as_i64())
                    .sum();
                black_box(sum);
            }
        }
        // The C baselines. Their symbols resolve because both libraries are
        // compiled into this binary by build.rs and statically linked, so
        // `nm` on the executable sees them.
        #[cfg(feature = "cbench")]
        "yyjson" => {
            for _ in 0..iters {
                black_box(dacode_json::cbench::YyJson::parse(bytes));
            }
        }
        #[cfg(feature = "cbench")]
        "yyjson_sum" => {
            for _ in 0..iters {
                black_box(dacode_json::cbench::YyJson::sum_field(bytes, "score"));
            }
        }
        #[cfg(feature = "cbench")]
        "simdjson_dom" => {
            let p = dacode_json::cbench::Padded::new(bytes);
            for _ in 0..iters {
                black_box(dacode_json::cbench::SimdJson::parse_dom(&p));
            }
        }
        #[cfg(feature = "cbench")]
        "simdjson_ondemand" => {
            let p = dacode_json::cbench::Padded::new(bytes);
            for _ in 0..iters {
                black_box(dacode_json::cbench::SimdJson::sum_field(&p, "score"));
            }
        }

        other => {
            // Returning the error rather than `process::exit` keeps the
            // exit path out of the lint's way and still exits non-zero.
            return Err(
                format!("unknown workload {other:?}; try: {}", workloads.join(", ")).into(),
            );
        }
    }
    Ok(())
}
