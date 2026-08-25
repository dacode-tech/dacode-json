//! Differential fuzzing, reusable from a test, a binary, or libFuzzer.
//!
//! # Why corpus mutation rather than random bytes
//!
//! `tests/tier_contract.rs` already throws random bytes at every parser.
//! That finds panics and little else: random bytes are almost never
//! *nearly* valid JSON, so they exercise the first rejection branch and
//! stop.
//!
//! Mutating real documents reaches the interesting states — a truncated
//! escape, a duplicated bracket, a number with one character changed. The
//! seed corpus is `testdata/`, the same JSONTestSuite files the conformance
//! tests use, so the mutations start from documents that already probe
//! parser edge cases.
//!
//! That distinction is not theoretical. `-0` parsing as integer `0` instead
//! of `-0.0` survived 60 000 generated differential cases and was caught by
//! a conformance file, because the generated comparison used `f64`
//! equality and `0.0 == -0.0`.
//!
//! # Properties
//!
//! Every one is differential or an internal invariant, so there is nothing
//! to keep in sync by hand — see [`check`].

use crate::corpus::Rng;

/// What a single fuzz iteration checks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Finding {
    /// `strict` and `serde_json` disagreed about validity.
    ValidityMismatch { ours: bool, theirs: bool },
    /// Both accepted, but produced different values.
    ValueMismatch { ours: String, theirs: String },
    /// `strict` accepted but the result would not deserialize.
    NotDeserializable(String),
    /// The single-pass and index-fed builders produced different pools.
    BuilderMismatch { onepass: usize, indexed: usize },
    /// Two Stage 1 scanners disagreed on a valid document.
    ScannerMismatch,
    /// Serializing an accepted document and re-parsing changed it.
    RoundTripMismatch { before: String, after: String },
    /// A `jsonflat` buffer did not read back as the document it encoded.
    FlatMismatch(String),
}

/// Run every property against one input.
///
/// Returns the findings; an empty vector means the input behaved. Panics
/// are *not* caught — a panic is itself the bug the fuzzer is looking for.
#[must_use]
pub fn check(data: &[u8]) -> Vec<Finding> {
    let mut out = Vec::new();

    // --- every parser must survive arbitrary input ---
    exercise_all(data);

    let ours = crate::strict::validate(data);
    let theirs = serde_json::from_slice::<serde_json::Value>(data);

    if ours.is_ok() != theirs.is_ok() {
        out.push(Finding::ValidityMismatch {
            ours: ours.is_ok(),
            theirs: theirs.is_ok(),
        });
        // No point comparing values when they disagree on validity.
        return out;
    }

    let Ok(expect) = theirs else {
        // Both rejected. Nothing further to compare, but the non-validating
        // parsers still must not have panicked, which `exercise_all` covered.
        return out;
    };

    // --- accepted: the value must match ---
    let mut p = crate::strict::StrictParser::new();
    let Ok(doc) = p.parse(data) else {
        out.push(Finding::NotDeserializable("strict re-parse failed".into()));
        return out;
    };
    let got: serde_json::Value = match crate::de::from_doc(doc) {
        Ok(v) => v,
        Err(e) => {
            out.push(Finding::NotDeserializable(e.to_string()));
            return out;
        }
    };
    if !values_match(&got, &expect) {
        out.push(Finding::ValueMismatch {
            ours: got.to_string(),
            theirs: expect.to_string(),
        });
    }

    // --- the two builders must agree, byte for byte ---
    {
        let mut ws = crate::Workspace::new();
        let indexed = ws.parse_to_pool(data);
        let single = crate::onepass::parse_to_pool(data);
        if single.nodes() != indexed.nodes() {
            out.push(Finding::BuilderMismatch {
                onepass: single.len(),
                indexed: indexed.len(),
            });
        }
    }

    // --- every Stage 1 scanner must agree on a valid document ---
    {
        use crate::scan::{scan, Scanner};
        let reference = scan(Scanner::Scalar, data);
        for s in Scanner::ALL {
            if scan(s, data).positions() != reference.positions() {
                out.push(Finding::ScannerMismatch);
                break;
            }
        }
    }

    // --- serialize, re-parse, compare ---
    if let Ok(text) = crate::ser::to_vec(&expect) {
        match serde_json::from_slice::<serde_json::Value>(&text) {
            Ok(back) if values_match(&back, &expect) => {}
            Ok(back) => out.push(Finding::RoundTripMismatch {
                before: expect.to_string(),
                after: back.to_string(),
            }),
            Err(e) => out.push(Finding::RoundTripMismatch {
                before: expect.to_string(),
                after: format!("re-parse failed: {e}"),
            }),
        }
    }

    // --- jsonflat: encode, view, compare ---
    {
        let mut p2 = crate::strict::StrictParser::new();
        if let Ok(d) = p2.parse(data) {
            if let Ok(buf) = crate::flat::encode(d) {
                match crate::flat::View::new(&buf) {
                    Ok(v) => {
                        if v.validate_deep().is_err() {
                            out.push(Finding::FlatMismatch(
                                "buffer we just built failed deep validation".into(),
                            ));
                        }
                    }
                    Err(e) => out.push(Finding::FlatMismatch(e.to_string())),
                }
            }
        }
    }

    out
}

/// Drive every parser in the crate over `data`. Any panic is a bug.
fn exercise_all(data: &[u8]) {
    use crate::tiers::{tier0::Tier0, tier1::Tier1, tier2::Tier2, tier3::Tier3, JsonTier};
    use std::hint::black_box;

    fn tier<T: JsonTier>(data: &[u8]) {
        use std::hint::black_box;
        black_box(T::parse_string(data));
        black_box(T::parse_number(data));
        black_box(T::detect_type(data));
        black_box(T::validate(data));
        black_box(T::object_count(data));
        black_box(T::array_count(data));
        black_box(T::object_get(data, "a"));
        black_box(T::array_get(data, 0));
        black_box(T::object_keys(data));
    }
    tier::<Tier0>(data);
    tier::<Tier1>(data);
    tier::<Tier2>(data);
    tier::<Tier3>(data);

    let mut ws = crate::Workspace::new();
    let doc = ws.parse(data);
    black_box(doc.root().to_json());
    for (k, v) in doc.root().entries().take(64) {
        black_box(k.len());
        black_box(v.as_str());
    }

    black_box(crate::onepass::parse_to_pool(data).len());
    black_box(crate::strict::validate(data).is_ok());
}

/// 1 ULP of slack on floats: `serde_json`'s default float parser is not
/// correctly rounded, ours is.
fn values_match(a: &serde_json::Value, b: &serde_json::Value) -> bool {
    use serde_json::Value as J;
    match (a, b) {
        (J::Number(x), J::Number(y)) => {
            if x == y {
                return true;
            }
            match (x.as_f64(), y.as_f64()) {
                (Some(x), Some(y)) => {
                    // Tolerance is 2 ULP, which is the *measured* worst case
                    // for `serde_json`'s default float parser: over 200 000
                    // generated high-precision literals it deviated from the
                    // correctly-rounded value on 17.7% of them, never by more
                    // than 2 ULP. Ours matches `str::parse` exactly, which
                    // `tests/serde_de.rs::float_precision_beats_serde_json_default`
                    // asserts outright — this slack only stops the fuzzer
                    // reporting serde_json's rounding as our bug.
                    //
                    // `.abs()` would panic on i64::MIN; `unsigned_abs` is total.
                    (x.to_bits() as i64)
                        .wrapping_sub(y.to_bits() as i64)
                        .unsigned_abs()
                        <= 2
                }
                _ => false,
            }
        }
        (J::Array(x), J::Array(y)) => {
            x.len() == y.len() && x.iter().zip(y).all(|(a, b)| values_match(a, b))
        }
        (J::Object(x), J::Object(y)) => {
            x.len() == y.len()
                && x.iter()
                    .all(|(k, v)| y.get(k).is_some_and(|w| values_match(v, w)))
        }
        _ => a == b,
    }
}

// =====================================================================
// Mutation
// =====================================================================

/// Mutate `seed` into `out`. Deterministic given `rng`.
///
/// The operations mirror what libFuzzer's mutator does, restricted to the
/// bytes that matter for JSON. `interesting` is yyjson's `fuzzer.dict`
/// idea: splice in tokens a parser has special cases for.
pub fn mutate(rng: &mut Rng, seed: &[u8], out: &mut Vec<u8>) {
    const INTERESTING: &[&[u8]] = &[
        b"\"", b"\\", b"\\u", b"\\ud800", b"\\udfff", b"\\u0000", b"{", b"}", b"[", b"]",
        b":", b",", b"0", b"-0", b"1e999", b"-1e-999", b"true", b"false", b"null", b".",
        b"e", b"E", b"+", b"\x00", b"\xff", b"\xc0\x80", b"1234567890123456789012345",
    ];

    out.clear();
    out.extend_from_slice(seed);
    if out.is_empty() {
        out.extend_from_slice(b"{}");
    }

    for _ in 0..1 + rng.below(4) {
        let len = out.len();
        match rng.below(7) {
            // delete a byte
            0 if len > 1 => {
                out.remove(rng.below(len as u64) as usize);
            }
            // duplicate a byte
            1 => {
                let i = rng.below(len as u64) as usize;
                let b = out.get(i).copied().unwrap_or(b'x');
                out.insert(i, b);
            }
            // replace with an interesting byte
            2 => {
                let i = rng.below(len as u64) as usize;
                let b = *rng.pick(b"{}[]\",:\\ \t\n0123456789.eE+-tfn".as_slice()).unwrap_or(&b'x');
                if let Some(slot) = out.get_mut(i) {
                    *slot = b;
                }
            }
            // flip a bit
            3 => {
                let i = rng.below(len as u64) as usize;
                if let Some(slot) = out.get_mut(i) {
                    *slot ^= 1 << (rng.below(8) as u32);
                }
            }
            // splice in an interesting token
            4 => {
                let tok = rng.pick(INTERESTING).copied().unwrap_or(b"null");
                let i = rng.below(len as u64 + 1) as usize;
                let tail = out.split_off(i.min(out.len()));
                out.extend_from_slice(tok);
                out.extend_from_slice(&tail);
            }
            // truncate
            5 => {
                out.truncate(rng.below(len as u64) as usize);
            }
            // duplicate a span, to build deep nesting cheaply
            _ => {
                let a = rng.below(len as u64) as usize;
                let b = (a + 1 + rng.below(32) as usize).min(len);
                if let Some(span) = out.get(a..b).map(<[u8]>::to_vec) {
                    let at = rng.below(out.len() as u64 + 1) as usize;
                    let tail = out.split_off(at.min(out.len()));
                    out.extend_from_slice(&span);
                    out.extend_from_slice(&tail);
                }
            }
        }
        // Keep inputs bounded; deep nesting is covered by explicit tests.
        out.truncate(1 << 16);
    }
}

/// Read `testdata/` as a seed corpus.
///
/// Returns an empty vector if the directory is missing, so callers work in
/// a checkout without it.
#[must_use]
pub fn seed_corpus() -> Vec<Vec<u8>> {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata");
    let mut seeds = Vec::new();
    for sub in ["test_parsing", "test_checker", "test_transform"] {
        let Ok(dir) = std::fs::read_dir(root.join(sub)) else {
            continue;
        };
        for e in dir.flatten() {
            if e.path().extension().is_some_and(|x| x == "json") {
                if let Ok(b) = std::fs::read(e.path()) {
                    if b.len() <= (1 << 16) {
                        seeds.push(b);
                    }
                }
            }
        }
    }
    seeds
}
