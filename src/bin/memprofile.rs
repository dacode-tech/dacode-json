//! Memory profile of one implementation, on one corpus.
//!
//! # Metric choice, and why
//!
//! The obvious instrument — `mstats()` on macOS, `mallinfo2()` on glibc —
//! turns out to be unusable here. `memprofile selftest` calibrates it
//! against known sizes and finds two disqualifying problems on macOS:
//!
//! ```text
//! 8 MiB single Rust Vec  -> mstats reports 0 B      (large allocations are
//!                                                    mmap-backed and not counted)
//! 8 MiB C malloc, freed  -> mstats reports 48 B freed (frees are not observed)
//! ```
//!
//! Since every implementation of interest uses a handful of large buffers,
//! that instrument would have reported near-zero for the ones that matter.
//! Small allocations *are* counted correctly, which is exactly why the bug
//! is easy to miss: `serde_json`'s 130 000 little allocations show up fine
//! and the tape parsers' three big ones do not.
//!
//! So the primary metric is **peak RSS**, via
//! `getrusage(RUSAGE_SELF).ru_maxrss`:
//!
//! | column | meaning |
//! |---|---|
//! | `peak RSS` | process high-water minus the `baseline` run. Counts everything: Rust, C, `mmap`, stacks. |
//! | `rust allocs` | Rust allocation calls. Exact for Rust; **zero for the C libraries by construction**. |
//! | `rust bytes` | Rust bytes requested. Same caveat. |
//! | `heap (mstats)` | kept only as a cross-check. Do not quote it. |
//!
//! Peak RSS is monotonic per process, so exactly one implementation is
//! measured per invocation and a `baseline` run establishes the floor.
//! `tools/memprofile.sh` drives the matrix.
//!
//! Usage: `memprofile <impl> [corpus] [target-bytes]`
//!        `memprofile selftest`   — calibrate the instrument

use std::env;
use std::hint::black_box;
use vela_json::memstat::{human, Snapshot};
use vela_json::strict::StrictParser;
use vela_json::{corpus, flat, Workspace};

// Counting the Rust side needs a global allocator hook. It adds a couple of
// relaxed atomics per allocation, which is why this lives in its own binary
// and not in anything that gets timed.
#[global_allocator]
static ALLOC: vela_json::memstat::Counter = vela_json::memstat::Counter;

#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct Record {
    id: u64,
    name: String,
    age: u32,
    active: bool,
    city: String,
    score: i64,
    tags: Vec<String>,
}

vela_json::flat_struct! {
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

const IMPLS: &[&str] = &[
    // Establishes the floor: generate and touch the corpus, parse nothing.
    "baseline",
    "vela_tier1",
    "vela_tier2_tape",
    "vela_tier3_pool",
    "vela_tier3_grow",
    "vela_strict",
    "jsonflat_dyn",
    "jsonflat_typed",
    "serde_json_value",
    "serde_json_structs",
    "simd_json_owned",
    "simd_json_tape",
    "sonic_rs_value",
    #[cfg(feature = "cbench")]
    "yyjson",
    #[cfg(feature = "cbench")]
    "simdjson_dom",
];

fn main() {
    let args: Vec<String> = env::args().collect();
    let which = args.get(1).map(String::as_str).unwrap_or("list");
    let corpus_name = args.get(2).map(String::as_str).unwrap_or("records");
    let target: usize = args
        .get(3)
        .and_then(|s| s.parse().ok())
        .unwrap_or(1 << 20);

    if which == "list" {
        println!("{}", IMPLS.join(" "));
        return;
    }
    if which == "selftest" {
        selftest();
        return;
    }

    let json = match corpus_name {
        "records" => corpus::sized(target, 0x2C0, corpus::records),
        "int_array" => corpus::sized(target, 0x2C0, corpus::int_array),
        "strings" => corpus::sized(target, 0x2C0, corpus::strings),
        "geo_int" => corpus::sized(target, 0x2C0, corpus::geo_int),
        "geo_float" => corpus::sized(target, 0x2C0, corpus::geo_float),
        other => {
            eprintln!("unknown corpus {other:?}");
            std::process::exit(2);
        }
    };
    let src = json.as_bytes();

    // Touch the input once so its pages are resident and do not land in the
    // measured window.
    black_box(src.iter().map(|b| *b as usize).sum::<usize>());

    // Three snapshots, because an absolute "heap after build" figure
    // over-attributes: freed intermediates sit on the allocator's free
    // lists and are not returned to the OS, so they still inflate the
    // number. Differencing across the drop isolates what the parsed form
    // itself holds.
    let a = Snapshot::now();
    let (keep, note) = run(which, src);
    let b = Snapshot::now();
    drop(keep);
    let c = Snapshot::now();

    // What the structure held: everything the drop gave back.
    let retained = (b.heap_in_use as i64 - c.heap_in_use as i64).max(0);
    let churn = b.since(&a);

    // Absolute peak, not a delta: the baseline run supplies the floor and
    // the driver subtracts it. A delta here would miss anything allocated
    // before the first snapshot.
    println!(
        "{which}\t{corpus_name}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
        src.len(),
        b.rss_peak,
        retained,
        b.heap_chunks as i64 - c.heap_chunks as i64,
        churn.rust_allocs,
        churn.rust_bytes,
        note
    );

    if env::var("MEMPROFILE_VERBOSE").is_ok() {
        eprintln!(
            "  {which:20} retained {:>10} | peak rss {:>10} | allocs {:>9} | alloc bytes {:>10}",
            human(retained),
            human(churn.rss_peak as i64),
            churn.rust_allocs,
            human(churn.rust_bytes as i64),
        );
    }
}

/// Calibrate the instrument against known allocation sizes.
///
/// A memory number is only worth reporting if the tool that produced it is
/// known to be accurate, and `mstats()` is not obviously trustworthy for
/// large or C-side allocations. This allocates exactly 8 MiB from Rust and
/// exactly 8 MiB from C and reports what each metric sees, both while alive
/// and after the free.
fn selftest() {
    const N: usize = 8 << 20;

    println!("calibration, expecting {} per case\n", human(N as i64));

    // --- Rust, one large Vec ---
    {
        let a = Snapshot::now();
        let v: Vec<u8> = vec![7u8; N];
        black_box(v.len());
        let b = Snapshot::now();
        drop(v);
        let c = Snapshot::now();
        println!(
            "rust vec        alive:{:>10}  freed_on_drop:{:>10}  rss:{:>10}  rust_bytes:{:>10}",
            human(b.heap_in_use as i64 - a.heap_in_use as i64),
            human(b.heap_in_use as i64 - c.heap_in_use as i64),
            human(b.since(&a).rss_peak as i64),
            human(b.since(&a).rust_bytes as i64),
        );
    }

    // --- Rust, many small allocations ---
    {
        let a = Snapshot::now();
        let v: Vec<Vec<u8>> = (0..(N / 64)).map(|_| vec![3u8; 64]).collect();
        black_box(v.len());
        let b = Snapshot::now();
        drop(v);
        let c = Snapshot::now();
        println!(
            "rust small*{:<5} alive:{:>10}  freed_on_drop:{:>10}  rss:{:>10}  rust_bytes:{:>10}",
            N / 64,
            human(b.heap_in_use as i64 - a.heap_in_use as i64),
            human(b.heap_in_use as i64 - c.heap_in_use as i64),
            human(b.since(&a).rss_peak as i64),
            human(b.since(&a).rust_bytes as i64),
        );
    }

    // --- C malloc, via yyjson's pool allocator (a single malloc of N) ---
    #[cfg(feature = "cbench")]
    {
        let a = Snapshot::now();
        let pool = vela_json::cbench::YyPool::new(N).expect("pool");
        let b = Snapshot::now();
        drop(pool);
        let c = Snapshot::now();
        println!(
            "c malloc        alive:{:>10}  freed_on_drop:{:>10}  rss:{:>10}  rust_bytes:{:>10}",
            human(b.heap_in_use as i64 - a.heap_in_use as i64),
            human(b.heap_in_use as i64 - c.heap_in_use as i64),
            human(b.since(&a).rss_peak as i64),
            human(b.since(&a).rust_bytes as i64),
        );
        println!("\nnote: a C malloc is invisible to the Rust GlobalAlloc counter by");
        println!("      construction, which is why `retained` uses mstats()/mallinfo2().");
    }
}

/// A type-erased box holding whatever must stay alive for the measurement.
type Keep = Box<dyn std::any::Any>;

fn run(which: &str, src: &[u8]) -> (Keep, String) {
    match which {
        // Corpus already generated and touched by the caller; do nothing.
        "baseline" => (Box::new(()), "floor".to_string()),

        // --- Vela tiers: navigation only, no persistent structure ---
        "vela_tier1" => {
            use vela_json::tiers::{tier1::Tier1, JsonTier};
            let n = Tier1::array_count(src);
            (Box::new(()), format!("elems={n}"))
        }
        "vela_tier2_tape" => {
            let mut b = vela_json::tiers::tier2::tape::TapeBuilder::new();
            let n = b.build(src).len();
            (Box::new(b), format!("tape_entries={n}"))
        }
        "vela_tier3_pool" => {
            let mut ws = Workspace::with_capacity(src.len());
            let n = ws.parse(src).pool().len();
            (Box::new(ws), format!("nodes={n}"))
        }
        // Pre-sized with `with_capacity`: fastest, but the heuristic
        // (len/2 + 64 nodes, len+1 index slots) over-allocates heavily.
        // `vela_tier3_grow` is the same parser letting the Vecs grow.
        "vela_tier3_grow" => {
            let mut ws = Workspace::new();
            let n = ws.parse(src).pool().len();
            (Box::new(ws), format!("nodes={n}"))
        }
        "vela_strict" => {
            let mut p = StrictParser::with_capacity(src.len());
            let n = p.parse(src).map(|d| d.pool().len()).unwrap_or(0);
            (Box::new(p), format!("nodes={n}"))
        }

        // --- zero-copy buffers: the buffer IS the parsed form ---
        "jsonflat_dyn" => {
            let mut p = StrictParser::new();
            let doc = p.parse(src).expect("valid");
            let buf = flat::encode(doc).expect("encode");
            let n = buf.len();
            // Drop the parser so only the buffer is retained.
            drop(p);
            (Box::new(buf), format!("buf_bytes={n}"))
        }
        "jsonflat_typed" => {
            let recs: Vec<Record> = serde_json::from_slice(src).expect("valid");
            let mut w = flat::typed::TypedWriter::<FlatRecord>::new();
            for r in &recs {
                w.record()
                    .u64(r.id)
                    .u32(r.age)
                    .bool(r.active)
                    .i64(r.score)
                    .str(&r.name)
                    .str(&r.city)
                    .str_list(r.tags.iter().map(String::as_str));
            }
            let buf = w.finish();
            let n = buf.len();
            drop(recs);
            (Box::new(buf), format!("buf_bytes={n}"))
        }

        // --- Rust reference libraries ---
        "serde_json_value" => {
            let v: serde_json::Value = serde_json::from_slice(src).expect("valid");
            let n = v.as_array().map_or(0, Vec::len);
            (Box::new(v), format!("elems={n}"))
        }
        "serde_json_structs" => {
            let v: Vec<Record> = serde_json::from_slice(src).expect("valid");
            let n = v.len();
            (Box::new(v), format!("records={n}"))
        }
        "simd_json_owned" => {
            let mut buf = src.to_vec();
            let owned = simd_json::to_owned_value(&mut buf).expect("valid");
            (Box::new((buf, owned)), "to_owned_value".to_string())
        }
        // simd-json's actual tape, which is what tier 2/3 should be
        // compared against. Borrowed, so it keeps `buf` alive.
        "simd_json_tape" => {
            let mut buf = src.to_vec();
            let n = {
                let tape = simd_json::to_tape(&mut buf).expect("valid");
                usize::from(tape.as_value().is_object())
            };
            (Box::new(buf), format!("tape root_is_object={n}"))
        }
        "sonic_rs_value" => {
            let v: sonic_rs::Value = sonic_rs::from_slice(src).expect("valid");
            (Box::new(v), "value".to_string())
        }

        // --- C libraries ---
        #[cfg(feature = "cbench")]
        "yyjson" => {
            let doc = vela_json::cbench::YyDoc::parse(src).expect("valid");
            let n = doc.value_count();
            (Box::new(doc), format!("values={n}"))
        }
        #[cfg(feature = "cbench")]
        "simdjson_dom" => {
            // The parser retains its tape and string buffer, so what it
            // holds after a parse is its memory cost.
            let padded = vela_json::cbench::Padded::new(src);
            let n = vela_json::cbench::SimdJson::parse_dom(&padded);
            (Box::new(padded), format!("bytes={n}"))
        }

        other => {
            eprintln!("unknown impl {other:?}; try one of: {}", IMPLS.join(" "));
            std::process::exit(2);
        }
    }
}
