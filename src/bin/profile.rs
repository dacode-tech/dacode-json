//! Profiling target: runs one named workload in a loop so a sampling
//! profiler has something to attach to.
//!
//! Usage: `profile <workload> [iterations]`
//! Run under a profiler with, for example:
//!   samply record -- ./target/release/profile vela_de 200

use std::env;
use std::hint::black_box;
use dacodec::strict::StrictParser;
use dacodec::{corpus, de, flat, ser, Workspace};

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

fn main() {
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
        return;
    }

    eprintln!("running {workload} x{iters} over {} bytes", bytes.len());

    match workload {
        "vela_scan" => {
            let mut idx = dacodec::StructuralIndex::with_capacity(bytes.len() + 64);
            for _ in 0..iters {
                idx.clear();
                dacodec::scan::scan_into(dacodec::Scanner::default(), bytes, &mut idx);
                black_box(idx.len());
            }
        }
        // Stage 2 alone: index pre-built and reused, so only the DOM
        // builder is sampled.
        "vela_stage2" => {
            use dacodec::builder::{build_from_index, pool_capacity_for, Stack};
            use dacodec::pool::Pool;
            let si = dacodec::scan::scan(dacodec::Scanner::default(), bytes);
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
                black_box(p.parse(bytes).expect("valid").pool().len());
            }
        }
        "vela_de" => {
            let mut p = StrictParser::with_capacity(bytes.len());
            for _ in 0..iters {
                let doc = p.parse(bytes).expect("valid");
                let v: Vec<Record> = de::from_doc(doc).expect("de");
                black_box(v.len());
            }
        }
        "vela_ser" => {
            let data: Vec<Record> = serde_json::from_slice(bytes).expect("parse");
            let mut out = Vec::with_capacity(bytes.len() + 64);
            for _ in 0..iters {
                out.clear();
                ser::to_writer(&mut out, &data).expect("ser");
                black_box(out.len());
            }
        }
        "serde_json_value" => {
            for _ in 0..iters {
                let v: serde_json::Value = serde_json::from_slice(bytes).expect("parse");
                black_box(v);
            }
        }
        "serde_json_de" => {
            for _ in 0..iters {
                let v: Vec<Record> = serde_json::from_slice(bytes).expect("de");
                black_box(v.len());
            }
        }
        "serde_json_ser" => {
            let data: Vec<Record> = serde_json::from_slice(bytes).expect("parse");
            for _ in 0..iters {
                black_box(serde_json::to_vec(&data).expect("ser").len());
            }
        }
        "simd_json_tape" => {
            let mut buffers = simd_json::Buffers::new(bytes.len());
            for _ in 0..iters {
                let mut buf = bytes.to_vec();
                let t = simd_json::to_tape_with_buffers(&mut buf, &mut buffers).expect("tape");
                black_box(t.as_value().is_object());
            }
        }
        "sonic_de" => {
            for _ in 0..iters {
                let v: Vec<Record> = sonic_rs::from_slice(bytes).expect("de");
                black_box(v.len());
            }
        }
        "flat_encode" => {
            let mut p = StrictParser::with_capacity(bytes.len());
            for _ in 0..iters {
                let doc = p.parse(bytes).expect("valid");
                black_box(flat::encode(doc).expect("encode").len());
            }
        }
        "flat_read" => {
            let mut p = StrictParser::with_capacity(bytes.len());
            let buf = flat::encode(p.parse(bytes).expect("valid")).expect("encode");
            for _ in 0..iters * 20 {
                let v = flat::View::new(&buf).expect("view");
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
                black_box(dacodec::cbench::YyJson::parse(bytes));
            }
        }
        #[cfg(feature = "cbench")]
        "yyjson_sum" => {
            for _ in 0..iters {
                black_box(dacodec::cbench::YyJson::sum_field(bytes, "score"));
            }
        }
        #[cfg(feature = "cbench")]
        "simdjson_dom" => {
            let p = dacodec::cbench::Padded::new(bytes);
            for _ in 0..iters {
                black_box(dacodec::cbench::SimdJson::parse_dom(&p));
            }
        }
        #[cfg(feature = "cbench")]
        "simdjson_ondemand" => {
            let p = dacodec::cbench::Padded::new(bytes);
            for _ in 0..iters {
                black_box(dacodec::cbench::SimdJson::sum_field(&p, "score"));
            }
        }

        other => {
            eprintln!("unknown workload {other:?}; try: {}", workloads.join(", "));
            std::process::exit(2);
        }
    }
}
