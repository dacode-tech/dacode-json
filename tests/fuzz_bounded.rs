//! A bounded run of the differential fuzzer, so CI exercises it.
//!
//! The full fuzzer is `cargo run --release --features fuzzing --bin fuzz`.
//! This runs a fixed, deterministic slice of it: enough to catch a
//! regression, short enough for `cargo test`.

#![cfg(feature = "profiling")]

use dacodec::corpus::Rng;
use dacodec::fuzz::{check, mutate, seed_corpus};

#[test]
fn mutated_corpus_finds_nothing() {
    let seeds = seed_corpus();
    assert!(
        seeds.len() > 300,
        "expected the JSONTestSuite corpus under testdata/, found {} files",
        seeds.len()
    );

    let mut rng = Rng::new(1);
    let mut buf = Vec::new();
    let mut findings = Vec::new();
    let mut valid = 0usize;
    const N: usize = 20_000;

    for _ in 0..N {
        let case_seed = rng.next_u64();
        let mut case_rng = Rng::new(case_seed);
        let pick = case_rng.below(seeds.len() as u64) as usize;
        let s = seeds.get(pick).map(Vec::as_slice).unwrap_or(b"{}");
        mutate(&mut case_rng, s, &mut buf);

        if dacodec::strict::validate(&buf).is_ok() {
            valid += 1;
        }
        let f = check(&buf);
        if !f.is_empty() {
            findings.push(format!(
                "  case_seed={case_seed} input={:?}\n    {f:?}",
                String::from_utf8_lossy(buf.get(..160).unwrap_or(&buf))
            ));
            if findings.len() >= 5 {
                break;
            }
        }
    }

    // A mutation corpus that is never valid would be exercising only the
    // reject path, which is the failure mode this fuzzer exists to avoid.
    assert!(
        valid > N / 100,
        "only {valid}/{N} mutations were valid JSON; the mutator is too destructive"
    );

    assert!(
        findings.is_empty(),
        "{} finding(s):\n{}\nreproduce with: \
         cargo run --release --features fuzzing --bin fuzz -- <case_seed> 1",
        findings.len(),
        findings.join("\n")
    );
}

/// The seed corpus itself must behave, before any mutation.
#[test]
fn unmutated_corpus_finds_nothing() {
    let mut findings = Vec::new();
    for (i, s) in seed_corpus().iter().enumerate() {
        let f = check(s);
        if !f.is_empty() {
            findings.push(format!("  seed #{i}: {f:?}"));
        }
    }
    assert!(findings.is_empty(), "{}", findings.join("\n"));
}
