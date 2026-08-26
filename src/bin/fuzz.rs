//! Corpus-mutation differential fuzzer.
//!
//! Seeds from `testdata/` (JSONTestSuite and friends), mutates, and checks
//! every property in [`dacodec::fuzz::check`] against `serde_json` and
//! against the crate's own invariants.
//!
//! Deterministic: a run is fully described by its seed, so a failure is
//! reproducible with `fuzz <seed> 1`.
//!
//! Usage: `fuzz [seed] [iterations]`

use std::hint::black_box;
use dacodec::corpus::Rng;
use dacodec::fuzz::{check, mutate, seed_corpus, Finding};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let seed: u64 = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(1);
    let iters: usize = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(100_000);

    let seeds = seed_corpus();
    if seeds.is_empty() {
        eprintln!("no seed corpus under testdata/; falling back to generated documents");
    }
    eprintln!("seed={seed} iterations={iters} corpus={} files", seeds.len());

    let mut rng = Rng::new(seed);
    let mut buf = Vec::new();
    let mut findings: Vec<(u64, Vec<u8>, Vec<Finding>)> = Vec::new();
    let mut accepted = 0usize;

    for i in 0..iters {
        // Record the RNG state before each case so a failure can be
        // replayed exactly.
        let case_seed = rng.next_u64();
        let mut case_rng = Rng::new(case_seed);

        if seeds.is_empty() {
            buf = dacodec::corpus::random_value(&mut case_rng, 4).into_bytes();
        } else {
            let pick = case_rng.below(seeds.len() as u64) as usize;
            let s = seeds.get(pick).map(Vec::as_slice).unwrap_or(b"{}");
            mutate(&mut case_rng, s, &mut buf);
        }

        if dacodec::strict::validate(&buf).is_ok() {
            accepted += 1;
        }

        let f = check(&buf);
        if !f.is_empty() {
            findings.push((case_seed, buf.clone(), f));
            if findings.len() >= 20 {
                eprintln!("stopping after 20 findings at iteration {i}");
                break;
            }
        }
        black_box(&buf);
    }

    eprintln!(
        "done: {accepted}/{iters} mutations were still valid JSON ({:.1}%)",
        100.0 * accepted as f64 / iters as f64
    );

    if findings.is_empty() {
        println!("no findings");
        return;
    }

    println!("{} finding(s):", findings.len());
    for (case_seed, input, fs) in &findings {
        println!("\n  case_seed={case_seed}");
        println!("  input: {:?}", String::from_utf8_lossy(input.get(..200).unwrap_or(input)));
        for f in fs {
            println!("    {f:?}");
        }
    }
    std::process::exit(1);
}
