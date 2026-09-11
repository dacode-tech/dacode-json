//! The real conformance suites, run against [`dacode_json::strict`].
//!
//! Everything before this measured conformance against `serde_json` on
//! generated data. That checks agreement, not correctness — two parsers can
//! agree and both be wrong, and generated data only exercises what the
//! generator knows how to produce.
//!
//! `testdata/` carries the standard suites instead:
//!
//! * **JSONTestSuite** (Nicolas Seriot, MIT) — 319 files, `y_` must be
//!   accepted, `n_` must be rejected, `i_` is implementation-defined.
//! * **JSON_checker** (Crockford) — 36 `pass`/`fail` files.
//! * **test_transform** — structures parsers read differently.
//! * **num/** — number literal edge cases, one per line.
//!
//! See `testdata/README.md` for provenance.
//!
//! Where this project's parsers are *deliberately* non-conformant — the
//! faithful tier-3 port and tiers 0–2 do not validate at all — that is
//! recorded rather than asserted, so the suite documents the gap instead of
//! hiding it.

use std::fs;
use std::path::{Path, PathBuf};
use dacode_json::strict;

fn testdata(sub: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata").join(sub)
}

/// Every file in a directory, sorted, as `(name, bytes)`.
fn load_dir(sub: &str) -> Vec<(String, Vec<u8>)> {
    let dir = testdata(sub);
    let mut out: Vec<(String, Vec<u8>)> = fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("read {}: {e}", dir.display()))
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().is_some_and(|x| x == "json"))
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            fs::read(e.path()).ok().map(|b| (name, b))
        })
        .collect();
    out.sort_by(|a, b| a.0.cmp(&b.0));
    assert!(!out.is_empty(), "no .json files in {}", dir.display());
    out
}

// =====================================================================
// JSONTestSuite
// =====================================================================

#[test]
fn jsontestsuite_accepts_every_y_case() {
    let mut failures = Vec::new();
    let mut n = 0usize;

    for (name, bytes) in load_dir("test_parsing") {
        if !name.starts_with("y_") {
            continue;
        }
        n += 1;
        if let Err(e) = strict::validate(&bytes) {
            failures.push(format!("  {name}: {e}"));
        }
    }

    assert!(n >= 90, "expected ~95 y_ cases, found {n}");
    assert!(
        failures.is_empty(),
        "{} of {n} y_ cases were wrongly rejected:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

#[test]
fn jsontestsuite_rejects_every_n_case() {
    let mut failures = Vec::new();
    let mut n = 0usize;

    for (name, bytes) in load_dir("test_parsing") {
        if !name.starts_with("n_") {
            continue;
        }
        n += 1;
        if strict::validate(&bytes).is_ok() {
            failures.push(format!(
                "  {name}: {:?}",
                String::from_utf8_lossy(bytes.get(..60).unwrap_or(&bytes))
            ));
        }
    }

    assert!(n >= 180, "expected ~189 n_ cases, found {n}");
    assert!(
        failures.is_empty(),
        "{} of {n} n_ cases were wrongly accepted:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

/// `i_` cases are implementation-defined. Nothing is asserted about the
/// verdict; what *is* asserted is that we agree with `serde_json`, since
/// two RFC-conformant parsers disagreeing on these is worth knowing about.
#[test]
fn jsontestsuite_i_cases_agree_with_serde_json() {
    let mut disagree = Vec::new();
    let mut n = 0usize;

    for (name, bytes) in load_dir("test_parsing") {
        if !name.starts_with("i_") {
            continue;
        }
        n += 1;
        let ours = strict::validate(&bytes).is_ok();
        let theirs = serde_json::from_slice::<serde_json::Value>(&bytes).is_ok();
        if ours != theirs {
            disagree.push(format!("  {name}: ours={ours} serde_json={theirs}"));
        }
    }

    assert!(n >= 30, "expected ~35 i_ cases, found {n}");
    if !disagree.is_empty() {
        println!(
            "{} of {n} implementation-defined cases differ from serde_json:\n{}",
            disagree.len(),
            disagree.join("\n")
        );
    }
}

/// The whole suite against `serde_json`, as a single agreement figure.
#[test]
fn jsontestsuite_agreement_with_serde_json() {
    let mut agree = 0usize;
    let mut total = 0usize;
    let mut diffs = Vec::new();

    for (name, bytes) in load_dir("test_parsing") {
        let ours = strict::validate(&bytes).is_ok();
        let theirs = serde_json::from_slice::<serde_json::Value>(&bytes).is_ok();
        total += 1;
        if ours == theirs {
            agree += 1;
        } else {
            diffs.push(format!("  {name}: ours={ours} serde={theirs}"));
        }
    }

    println!("JSONTestSuite: agree with serde_json on {agree}/{total}");
    if !diffs.is_empty() {
        println!("{}", diffs.join("\n"));
    }
    // Only the implementation-defined cases may differ.
    assert!(
        diffs.len() <= 35,
        "too many disagreements ({}), which means one of us is wrong on a\n\
         mandatory case:\n{}",
        diffs.len(),
        diffs.join("\n")
    );
}

// =====================================================================
// JSON_checker
// =====================================================================

#[test]
fn json_checker_pass_and_fail() {
    let mut wrong = Vec::new();
    let (mut passes, mut fails) = (0usize, 0usize);

    for (name, bytes) in load_dir("test_checker") {
        // Upstream marks two cases inapplicable: fail01 was relaxed by
        // RFC 7159 (a bare scalar is a valid document) and fail18 tests a
        // nesting depth RFC 8259 does not specify.
        if name.contains("_EXCLUDE") {
            continue;
        }
        let ok = strict::validate(&bytes).is_ok();
        if name.starts_with("pass") {
            passes += 1;
            if !ok {
                wrong.push(format!("  {name} should have been accepted"));
            }
        } else if name.starts_with("fail") {
            fails += 1;
            if ok {
                wrong.push(format!("  {name} should have been rejected"));
            }
        }
    }

    assert!(passes >= 3 && fails >= 25, "suite looks incomplete: {passes} pass, {fails} fail");
    assert!(wrong.is_empty(), "{}", wrong.join("\n"));
}

// =====================================================================
// test_transform — no verdict specified, but we must match serde_json
// =====================================================================

#[test]
fn transform_cases_match_serde_json() {
    let mut diffs = Vec::new();
    for (name, bytes) in load_dir("test_transform") {
        let ours = strict::validate(&bytes).is_ok();
        let theirs = serde_json::from_slice::<serde_json::Value>(&bytes).is_ok();
        if ours != theirs {
            diffs.push(format!("  {name}: ours={ours} serde={theirs}"));
        }
    }
    assert!(diffs.is_empty(), "transform cases differ:\n{}", diffs.join("\n"));
}

/// Accepted documents must also *parse* to the same values, not merely be
/// accepted. A validator that says yes and then builds the wrong tree is
/// worse than one that says no.
#[test]
fn accepted_documents_parse_to_the_same_values() {
    let mut p = strict::StrictParser::new();
    let mut checked = 0usize;

    for dir in ["test_parsing", "test_checker", "test_transform"] {
        for (name, bytes) in load_dir(dir) {
            let Ok(expect) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
                continue;
            };
            let Ok(doc) = p.parse(&bytes) else { continue };
            let got: serde_json::Value = match dacode_json::de::from_doc(doc) {
                Ok(v) => v,
                Err(e) => panic!("{dir}/{name}: accepted but would not deserialize: {e}"),
            };
            checked += 1;
            assert!(
                values_match(&got, &expect),
                "{dir}/{name}: parsed to a different value\n  ours:  {got}\n  serde: {expect}"
            );
        }
    }

    assert!(checked > 100, "only {checked} documents cross-checked");
    println!("cross-checked {checked} accepted documents against serde_json");
}

/// 2 ULP of slack on floats — the measured worst case for `serde_json`'s
/// default float parser (17.7% of high-precision literals deviate, never by
/// more than 2). Ours matches `str::parse` exactly; see
/// `tests/serde_de.rs::float_precision_beats_serde_json_default`.
fn values_match(a: &serde_json::Value, b: &serde_json::Value) -> bool {
    use serde_json::Value as J;
    match (a, b) {
        (J::Number(x), J::Number(y)) => {
            if x == y {
                return true;
            }
            match (x.as_f64(), y.as_f64()) {
                (Some(x), Some(y)) => {
                    (x.to_bits() as i64).wrapping_sub(y.to_bits() as i64).unsigned_abs() <= 2
                        || (x.is_nan() && y.is_nan())
                }
                _ => false,
            }
        }
        (J::Array(x), J::Array(y)) => {
            x.len() == y.len() && x.iter().zip(y).all(|(a, b)| values_match(a, b))
        }
        (J::Object(x), J::Object(y)) => {
            x.len() == y.len()
                && x.iter().all(|(k, v)| y.get(k).is_some_and(|w| values_match(v, w)))
        }
        _ => a == b,
    }
}

// =====================================================================
// Number edge cases
// =====================================================================

/// `testdata/num/*.txt` hold one literal per line, `#` comments.
fn load_numbers(file: &str) -> Vec<String> {
    let path = testdata("num").join(file);
    let Ok(text) = fs::read_to_string(&path) else {
        return Vec::new();
    };
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(str::to_owned)
        .collect()
}

#[test]
fn integer_literals_parse_exactly() {
    let nums = load_numbers("int.txt");
    assert!(nums.len() > 100, "expected many integers, got {}", nums.len());

    let mut p = strict::StrictParser::new();
    let mut checked = 0usize;
    for lit in &nums {
        let doc = match p.parse(lit.as_bytes()) {
            Ok(d) => d,
            Err(e) => panic!("rejected integer {lit:?}: {e}"),
        };
        let root = doc.root();
        // `-0` is deliberately a float, to keep the sign that an integer
        // 0 would discard. Matches serde_json; see src/strict.rs.
        if lit == "-0" {
            assert_eq!(root.as_f64(), Some(-0.0), "on {lit:?}");
            assert!(root.as_f64().is_some_and(f64::is_sign_negative), "-0 lost its sign");
            checked += 1;
            continue;
        }
        // Anything else that fits i64 must come back exactly, not as a float.
        if let Ok(want) = lit.parse::<i64>() {
            assert_eq!(root.as_i64(), Some(want), "on {lit:?}");
            checked += 1;
        } else {
            // Out of i64 range: must degrade to f64, like serde_json.
            assert!(root.as_f64().is_some(), "on {lit:?}");
        }
    }
    println!("checked {checked} exact integers of {}", nums.len());
}

#[test]
fn malformed_numbers_are_rejected() {
    for file in ["int(fail).txt", "real(fail).txt", "hex(fail).txt"] {
        let nums = load_numbers(file);
        if nums.is_empty() {
            continue;
        }
        let mut wrong = Vec::new();
        for lit in &nums {
            if strict::validate(lit.as_bytes()).is_ok() {
                // Cross-check: if serde_json also accepts it, the file's
                // expectation is about yyjson extensions, not RFC 8259.
                if serde_json::from_str::<serde_json::Value>(lit).is_err() {
                    wrong.push(lit.clone());
                }
            }
        }
        assert!(
            wrong.is_empty(),
            "{file}: accepted {} literals serde_json rejects: {:?}",
            wrong.len(),
            wrong.get(..10.min(wrong.len()))
        );
    }
}

#[test]
fn real_literals_match_serde_json() {
    let nums = load_numbers("real.txt");
    if nums.is_empty() {
        return;
    }
    let mut p = strict::StrictParser::new();
    let mut ulp_diffs = 0usize;
    let mut checked = 0usize;

    for lit in &nums {
        let (Ok(doc), Ok(theirs)) = (
            p.parse(lit.as_bytes()),
            serde_json::from_str::<serde_json::Value>(lit),
        ) else {
            continue;
        };
        let (Some(ours), Some(want)) = (doc.root().as_f64(), theirs.as_f64()) else {
            continue;
        };
        checked += 1;
        // `str::parse` is the correctly-rounded reference.
        if let Ok(reference) = lit.parse::<f64>() {
            assert_eq!(
                ours.to_bits(),
                reference.to_bits(),
                "we deviated from the correctly-rounded value for {lit}"
            );
            if want.to_bits() != reference.to_bits() {
                ulp_diffs += 1;
            }
        }
    }
    println!(
        "checked {checked} reals; serde_json deviated from correctly-rounded on {ulp_diffs}"
    );
}

// =====================================================================
// What the non-validating parsers do with the suite
// =====================================================================

/// The faithful tier-3 port and tiers 0–2 do not validate. This records how
/// far off they are rather than asserting anything, so the gap is visible
/// and tracked.
#[cfg(feature = "vela-compat")]
#[test]
fn non_validating_parsers_scored_against_the_suite() {
    use dacode_json::tiers::{tier1::Tier1, tier2::Tier2, tier3::Tier3, JsonTier};
    use std::collections::BTreeMap;

    let cases: Vec<(String, Vec<u8>, bool)> = load_dir("test_parsing")
        .into_iter()
        .filter(|(n, _)| n.starts_with("y_") || n.starts_with("n_"))
        .map(|(n, b)| {
            let expect = n.starts_with("y_");
            (n, b, expect)
        })
        .collect();

    let mut score: BTreeMap<&str, usize> = BTreeMap::new();
    for (_, bytes, expect) in &cases {
        if strict::validate(bytes).is_ok() == *expect {
            *score.entry("strict").or_default() += 1;
        }
        if Tier1::validate(bytes) == *expect {
            *score.entry("tier1").or_default() += 1;
        }
        if Tier2::validate(bytes) == *expect {
            *score.entry("tier2").or_default() += 1;
        }
        if Tier3::validate(bytes) == *expect {
            *score.entry("tier3").or_default() += 1;
        }
        if serde_json::from_slice::<serde_json::Value>(bytes).is_ok() == *expect {
            *score.entry("serde_json").or_default() += 1;
        }
    }

    let n = cases.len();
    println!("JSONTestSuite y_/n_ verdicts, {n} mandatory cases:");
    for (k, v) in &score {
        println!("  {k:11} {v:3}/{n}  {:5.1}%", 100.0 * *v as f64 / n as f64);
    }

    // strict must be perfect; the others are documented, not constrained.
    assert_eq!(
        score.get("strict").copied().unwrap_or(0),
        n,
        "strict parser must get every mandatory case right"
    );
}
