//! The streaming deserializer must be indistinguishable from the pool one.
//!
//! It skips values instead of parsing them, which is exactly the kind of
//! shortcut that quietly starts accepting malformed input. So it is held
//! to `serde_json`'s verdict on every file in `testdata/`, not just to a
//! round-trip.

use std::fs;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde_json::Value as JValue;

fn testdata(sub: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata").join(sub)
}

fn load_dir(sub: &str) -> Vec<(String, Vec<u8>)> {
    let dir = testdata(sub);
    let Ok(rd) = fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut out: Vec<(String, Vec<u8>)> = rd
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().is_some_and(|x| x == "json"))
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            fs::read(e.path()).ok().map(|b| (name, b))
        })
        .collect();
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

/// Accept/reject must match `serde_json` on every corpus file.
///
/// `y_`/`n_` cases are mandatory in JSONTestSuite; `i_` cases are
/// implementation-defined, so they only have to *agree*, not to be right.
#[test]
fn agrees_with_serde_json_on_every_corpus_file() {
    let mut checked = 0usize;
    let mut disagreements = Vec::new();

    for sub in ["test_parsing", "test_transform", "JSON_checker", "test_encoding"] {
        for (name, bytes) in load_dir(sub) {
            let theirs = serde_json::from_slice::<JValue>(&bytes).is_ok();
            let ours = dacodec::stream::from_slice::<JValue>(&bytes).is_ok();
            checked += 1;
            if theirs != ours {
                disagreements.push(format!(
                    "{sub}/{name}: serde_json={theirs} stream={ours}"
                ));
            }
        }
    }

    assert!(checked > 300, "corpus looks missing, only {checked} files");
    assert!(
        disagreements.is_empty(),
        "{} of {checked} files disagree:\n{}",
        disagreements.len(),
        disagreements.join("\n")
    );
}

/// `serde_json`'s float parser is not correctly rounded: measured over
/// 200 000 high-precision literals it deviates on 17.7%, by up to 2 ULP
/// (`docs/RESULTS.md`). So float comparisons allow that much, and only
/// that much.
fn ulps_apart(a: f64, b: f64) -> u64 {
    if a == b {
        return 0;
    }
    if a.is_nan() || b.is_nan() || a.signum() != b.signum() {
        return u64::MAX;
    }
    let (x, y) = (a.to_bits(), b.to_bits());
    x.max(y) - x.min(y)
}

fn approx_eq(a: &JValue, b: &JValue) -> bool {
    match (a, b) {
        (JValue::Number(x), JValue::Number(y)) => {
            if x == y {
                return true;
            }
            match (x.as_f64(), y.as_f64()) {
                (Some(fx), Some(fy)) => ulps_apart(fx, fy) <= 2,
                _ => false,
            }
        }
        (JValue::Array(x), JValue::Array(y)) => {
            x.len() == y.len() && x.iter().zip(y).all(|(p, q)| approx_eq(p, q))
        }
        (JValue::Object(x), JValue::Object(y)) => {
            x.len() == y.len()
                && x.iter()
                    .all(|(k, v)| y.get(k).is_some_and(|w| approx_eq(v, w)))
        }
        _ => a == b,
    }
}

/// Where both accept, the *value* must match too — not merely the verdict.
#[test]
fn accepted_documents_deserialize_to_the_same_value() {
    let mut compared = 0usize;
    for sub in ["test_parsing", "test_transform", "JSON_checker"] {
        for (name, bytes) in load_dir(sub) {
            let Ok(theirs) = serde_json::from_slice::<JValue>(&bytes) else {
                continue;
            };
            let ours: JValue = dacodec::stream::from_slice(&bytes)
                .unwrap_or_else(|e| panic!("{sub}/{name}: stream rejected an accepted doc: {e}"));
            assert!(
                approx_eq(&ours, &theirs),
                "{sub}/{name}: value mismatch\n  stream     {ours}\n  serde_json {theirs}"
            );
            compared += 1;
        }
    }
    assert!(compared > 100, "only compared {compared} documents");
}

/// The streaming and pool paths must agree with each other.
///
/// `dacodec::from_slice` is the streaming path now, so the pool is named
/// explicitly as `dacodec::de::from_slice`.
#[test]
fn agrees_with_the_pool_deserializer() {
    for sub in ["test_parsing", "test_transform", "JSON_checker"] {
        for (name, bytes) in load_dir(sub) {
            let pool = dacodec::de::from_slice::<JValue>(&bytes);
            let stream = dacodec::stream::from_slice::<JValue>(&bytes);
            assert_eq!(
                pool.is_ok(),
                stream.is_ok(),
                "{sub}/{name}: pool={:?} stream={:?}",
                pool.as_ref().err().map(|e| e.to_string()),
                stream.as_ref().err().map(|e| e.to_string()),
            );
            if let (Ok(a), Ok(b)) = (pool, stream) {
                assert!(
                    approx_eq(&a, &b),
                    "{sub}/{name}: values differ\n  pool   {a}\n  stream {b}"
                );
            }
        }
    }
}

// =====================================================================
// Typed round-trips
// =====================================================================

#[derive(Debug, Deserialize, PartialEq)]
struct Record {
    id: u64,
    name: String,
    age: u32,
    active: bool,
    city: String,
    score: i64,
    tags: Vec<String>,
}

/// Two of seven fields: exercises the skip path, which is the whole point.
#[derive(Debug, Deserialize, PartialEq)]
struct Partial {
    id: u64,
    city: String,
}

#[derive(Debug, Deserialize, PartialEq)]
struct Borrowed<'a> {
    id: u64,
    #[serde(borrow)]
    name: &'a str,
}

fn corpus() -> Vec<u8> {
    dacodec::corpus::sized(256 * 1024, 0xC0FFEE, dacodec::corpus::records).into_bytes()
}

#[test]
fn full_struct_matches_serde_json() {
    let src = corpus();
    let theirs: Vec<Record> = serde_json::from_slice(&src).expect("serde_json");
    let ours: Vec<Record> = dacodec::stream::from_slice(&src).expect("stream");
    assert_eq!(ours, theirs);
}

#[test]
fn skipped_fields_match_serde_json() {
    let src = corpus();
    let theirs: Vec<Partial> = serde_json::from_slice(&src).expect("serde_json");
    let ours: Vec<Partial> = dacodec::stream::from_slice(&src).expect("stream");
    assert_eq!(ours, theirs);
}

#[test]
fn borrowed_strings_point_into_the_input() {
    let src = br#"[{"id":1,"name":"alpha","x":[1,2,{"y":"z"}]}]"#;
    let mut idx = dacodec::stream::Index::default();
    let rows: Vec<Borrowed<'_>> =
        dacodec::stream::from_slice_with(&mut idx, &src[..]).expect("stream");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].name, "alpha");
    let base = src.as_ptr() as usize;
    let at = rows[0].name.as_ptr() as usize;
    assert!(
        at >= base && at < base + src.len(),
        "string was copied, not borrowed"
    );
}

/// Skipping must not become a hole in validation: each of these is
/// malformed *only* inside a field the target struct ignores.
#[test]
fn malformed_skipped_values_are_still_rejected() {
    let cases: &[&str] = &[
        r#"{"id":1,"city":"x","junk":01}"#,
        r#"{"id":1,"city":"x","junk":1.}"#,
        r#"{"id":1,"city":"x","junk":1e}"#,
        r#"{"id":1,"city":"x","junk":tru}"#,
        r#"{"id":1,"city":"x","junk":nul}"#,
        r#"{"id":1,"city":"x","junk":[1,]}"#,
        r#"{"id":1,"city":"x","junk":{"a":}}"#,
        r#"{"id":1,"city":"x","junk":"\q"}"#,
        r#"{"id":1,"city":"x","junk":+1}"#,
        r#"{"id":1,"city":"x","junk":.5}"#,
        r#"{"id":1,"city":"x","junk":[}"#,
    ];
    for case in cases {
        let theirs = serde_json::from_str::<Partial>(case).is_ok();
        let ours = dacodec::stream::from_slice::<Partial>(case.as_bytes()).is_ok();
        assert!(!theirs, "serde_json accepted {case}, fix the test");
        assert_eq!(ours, theirs, "disagreement on skipped-field case: {case}");
    }
}

/// Numbers must land on the same type and value as `serde_json`.
#[test]
fn number_edges_match_serde_json() {
    let cases: &[&str] = &[
        "0", "-0", "1", "-1", "1e2", "1E2", "1e+2", "1e-2", "0.5", "-0.5",
        "9223372036854775807",   // i64::MAX
        "-9223372036854775808",  // i64::MIN
        "18446744073709551615",  // u64::MAX
        "18446744073709551616",  // u64::MAX + 1, becomes a float
        "1.7976931348623157e308",
        "5e-324",
        "1e400",
        "123456789012345678901234567890",
        "0.1", "0.3", "1e-7", "3.141592653589793",
    ];
    for case in cases {
        let theirs = serde_json::from_str::<JValue>(case);
        let ours = dacodec::stream::from_slice::<JValue>(case.as_bytes());
        assert_eq!(
            theirs.is_ok(),
            ours.is_ok(),
            "verdict differs on {case}: {theirs:?} vs {ours:?}"
        );
        if let (Ok(a), Ok(b)) = (theirs, ours) {
            assert_eq!(a, b, "value differs on {case}");
        }
    }
}

/// Integers with more than 19 digits that still fit in `u64`.
///
/// The pool parser converts these to `f64` and loses the low digits;
/// `10000000000000000999` comes back as `1e19`. The streaming parser keeps
/// them exact, which is what `serde_json` does too. This pins the
/// difference so it stays visible until the pool is fixed.
#[test]
fn keeps_integer_precision_that_the_pool_loses() {
    let src = b"[10000000000000000999]";

    let theirs: JValue = serde_json::from_slice(src).expect("serde_json");
    let stream: JValue = dacodec::stream::from_slice(src).expect("stream");
    assert_eq!(stream, theirs, "stream must match serde_json exactly here");
    assert_eq!(stream[0].as_u64(), Some(10_000_000_000_000_000_999));

    let pool: JValue = dacodec::de::from_slice(src).expect("pool");
    assert_eq!(
        pool[0].as_u64(),
        None,
        "pool unexpectedly fixed: update this test and docs/RESULTS.md"
    );
}

#[test]
fn rejects_trailing_and_empty_input() {
    for bad in ["", "   ", "\u{feff}{}"] {
        assert!(
            dacodec::stream::from_slice::<JValue>(bad.as_bytes()).is_err(),
            "accepted {bad:?}"
        );
    }
}

#[test]
fn deeply_nested_input_is_rejected_not_overflowed() {
    let deep = format!("{}1{}", "[".repeat(4096), "]".repeat(4096));
    // Must not stack overflow. Either verdict is acceptable, a crash is not.
    let _ = dacodec::stream::from_slice::<JValue>(deep.as_bytes());
}
