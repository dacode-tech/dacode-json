//! Corpus-mutation differential fuzzer.
//!
//! Seeds from `testdata/` (JSONTestSuite and friends), mutates, and checks
//! every property in [`dacode_json::fuzz::check`] against `serde_json` and
//! against the crate's own invariants.
//!
//! Deterministic: a run is fully described by its seed, so a failure is
//! reproducible with `fuzz <seed> 1`.
//!
//! Usage: `fuzz [seed] [iterations]`

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

use dacode_json::corpus::Rng;
use dacode_json::fuzz::{check, mutate, seed_corpus, Finding};
use std::hint::black_box;

fn main() -> Result<(), Findings> {
    let args: Vec<String> = std::env::args().collect();
    let seed: u64 = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(1);
    let iters: usize = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(100_000);

    let seeds = seed_corpus();
    if seeds.is_empty() {
        eprintln!("no seed corpus under testdata/; falling back to generated documents");
    }
    eprintln!(
        "seed={seed} iterations={iters} corpus={} files",
        seeds.len()
    );

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
            buf = dacode_json::corpus::random_value(&mut case_rng, 4).into_bytes();
        } else {
            let pick = case_rng.below(seeds.len() as u64) as usize;
            let s = seeds.get(pick).map(Vec::as_slice).unwrap_or(b"{}");
            mutate(&mut case_rng, s, &mut buf);
        }

        if dacode_json::strict::validate(&buf).is_ok() {
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
        return Ok(());
    }
    Err(Findings(findings))
}

/// What the fuzzer found, reported by returning it from `main`.
///
/// A fuzzer that found something must exit non-zero, and this is how to
/// do that without `process::exit` or a panic: `main` returning `Err`
/// prints `Error: {Debug}` and exits 1. `Debug` is therefore the report,
/// which is why it is hand-written rather than derived.
struct Findings(Vec<(u64, Vec<u8>, Vec<Finding>)>);

impl std::fmt::Debug for Findings {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(f, "{} finding(s):", self.0.len())?;
        for (case_seed, input, fs) in &self.0 {
            writeln!(f, "\n  case_seed={case_seed}")?;
            writeln!(
                f,
                "  input: {:?}",
                String::from_utf8_lossy(input.get(..200).unwrap_or(input))
            )?;
            for finding in fs {
                writeln!(f, "    {finding:?}")?;
            }
        }
        Ok(())
    }
}
