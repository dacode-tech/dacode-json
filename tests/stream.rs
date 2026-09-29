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
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("testdata")
        .join(sub)
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

    for sub in [
        "test_parsing",
        "test_transform",
        "JSON_checker",
        "test_encoding",
    ] {
        for (name, bytes) in load_dir(sub) {
            let theirs = serde_json::from_slice::<JValue>(&bytes).is_ok();
            let ours = dacode_json::stream::from_slice::<JValue>(&bytes).is_ok();
            let direct = dacode_json::direct::from_slice::<JValue>(&bytes).is_ok();
            checked += 1;
            if theirs != ours {
                disagreements.push(format!("{sub}/{name}: serde_json={theirs} stream={ours}"));
            }
            if theirs != direct {
                disagreements.push(format!("{sub}/{name}: serde_json={theirs} direct={direct}"));
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
            let ours: JValue = dacode_json::stream::from_slice(&bytes)
                .unwrap_or_else(|e| panic!("{sub}/{name}: stream rejected an accepted doc: {e}"));
            assert!(
                approx_eq(&ours, &theirs),
                "{sub}/{name}: value mismatch\n  stream     {ours}\n  serde_json {theirs}"
            );
            let direct: JValue = dacode_json::direct::from_slice(&bytes)
                .unwrap_or_else(|e| panic!("{sub}/{name}: direct rejected an accepted doc: {e}"));
            assert!(
                approx_eq(&direct, &theirs),
                "{sub}/{name}: value mismatch\n  direct     {direct}\n  serde_json {theirs}"
            );
            compared += 1;
        }
    }
    assert!(compared > 100, "only compared {compared} documents");
}

/// The streaming and pool paths must agree with each other.
///
/// `dacode_json::from_slice` is the streaming path now, so the pool is named
/// explicitly as `dacode_json::de::from_slice`.
#[test]
fn agrees_with_the_pool_deserializer() {
    for sub in ["test_parsing", "test_transform", "JSON_checker"] {
        for (name, bytes) in load_dir(sub) {
            let pool = dacode_json::de::from_slice::<JValue>(&bytes);
            let stream = dacode_json::stream::from_slice::<JValue>(&bytes);
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
    dacode_json::corpus::sized(256 * 1024, 0xC0FFEE, dacode_json::corpus::records).into_bytes()
}

#[test]
fn full_struct_matches_serde_json() {
    let src = corpus();
    let theirs: Vec<Record> = serde_json::from_slice(&src).expect("serde_json");
    let ours: Vec<Record> = dacode_json::stream::from_slice(&src).expect("stream");
    assert_eq!(ours, theirs);
    let direct: Vec<Record> = dacode_json::direct::from_slice(&src).expect("direct");
    assert_eq!(direct, theirs);
}

#[test]
fn skipped_fields_match_serde_json() {
    let src = corpus();
    let theirs: Vec<Partial> = serde_json::from_slice(&src).expect("serde_json");
    let ours: Vec<Partial> = dacode_json::stream::from_slice(&src).expect("stream");
    assert_eq!(ours, theirs);
    let direct: Vec<Partial> = dacode_json::direct::from_slice(&src).expect("direct");
    assert_eq!(direct, theirs);
}

#[test]
fn borrowed_strings_point_into_the_input() {
    let src = br#"[{"id":1,"name":"alpha","x":[1,2,{"y":"z"}]}]"#;
    let mut idx = dacode_json::stream::Index::default();
    let rows: Vec<Borrowed<'_>> =
        dacode_json::stream::from_slice_with(&mut idx, &src[..]).expect("stream");
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
        let ours = dacode_json::stream::from_slice::<Partial>(case.as_bytes()).is_ok();
        let direct = dacode_json::direct::from_slice::<Partial>(case.as_bytes()).is_ok();
        assert!(!theirs, "serde_json accepted {case}, fix the test");
        assert_eq!(ours, theirs, "disagreement on skipped-field case: {case}");
        assert_eq!(
            direct, theirs,
            "direct disagrees on skipped-field case: {case}"
        );
    }
}

/// Numbers must land on the same type and value as `serde_json`.
#[test]
fn number_edges_match_serde_json() {
    let cases: &[&str] = &[
        "0",
        "-0",
        "1",
        "-1",
        "1e2",
        "1E2",
        "1e+2",
        "1e-2",
        "0.5",
        "-0.5",
        "9223372036854775807",  // i64::MAX
        "-9223372036854775808", // i64::MIN
        "18446744073709551615", // u64::MAX
        "18446744073709551616", // u64::MAX + 1, becomes a float
        "1.7976931348623157e308",
        "5e-324",
        "1e400",
        "123456789012345678901234567890",
        "0.1",
        "0.3",
        "1e-7",
        "3.141592653589793",
    ];
    for case in cases {
        let theirs = serde_json::from_str::<JValue>(case);
        let ours = dacode_json::stream::from_slice::<JValue>(case.as_bytes());
        let direct = dacode_json::direct::from_slice::<JValue>(case.as_bytes());
        assert_eq!(
            theirs.is_ok(),
            ours.is_ok(),
            "verdict differs on {case}: {theirs:?} vs {ours:?}"
        );
        assert_eq!(
            theirs.is_ok(),
            direct.is_ok(),
            "direct verdict differs on {case}: {theirs:?} vs {direct:?}"
        );
        if let (Ok(a), Ok(b)) = (theirs.as_ref(), ours.as_ref()) {
            assert_eq!(a, b, "value differs on {case}");
        }
        if let (Ok(a), Ok(b)) = (theirs, direct) {
            assert_eq!(a, b, "direct value differs on {case}");
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
    let stream: JValue = dacode_json::stream::from_slice(src).expect("stream");
    assert_eq!(stream, theirs, "stream must match serde_json exactly here");
    assert_eq!(stream[0].as_u64(), Some(10_000_000_000_000_000_999));

    let pool: JValue = dacode_json::de::from_slice(src).expect("pool");
    assert_eq!(
        pool[0].as_u64(),
        None,
        "pool unexpectedly fixed: update this test and docs/RESULTS.md"
    );
}

/// Negative integers that overflow `i64` and happen to be zero modulo
/// 2^64.
///
/// `-92233720368547758080` is 2^63 * 10. The pool parser accumulates into
/// a wrapping `u64`, so it landed on exactly 0, hit the `-0` special case
/// and returned `-0.0`. Found by the differential fuzzer.
#[test]
fn negative_overflow_that_wraps_to_zero() {
    for src in [
        &b"[-92233720368547758080]"[..],
        &b"[-18446744073709551616]"[..], // 2^64
        &b"[-36893488147419103232]"[..], // 2^65
    ] {
        let theirs: JValue = serde_json::from_slice(src).expect("serde_json");
        let got_all: [(&str, std::result::Result<JValue, String>); 3] = [
            (
                "stream",
                dacode_json::stream::from_slice::<JValue>(src).map_err(|e| e.to_string()),
            ),
            (
                "direct",
                dacode_json::direct::from_slice::<JValue>(src).map_err(|e| e.to_string()),
            ),
            (
                "pool",
                dacode_json::de::from_slice::<JValue>(src).map_err(|e| e.to_string()),
            ),
        ];
        for (who, got) in got_all {
            let got = got.unwrap_or_else(|e| panic!("{who} rejected {src:?}: {e}"));
            assert!(
                approx_eq(&got, &theirs),
                "{who} on {}: got {got}, serde_json {theirs}",
                String::from_utf8_lossy(src)
            );
        }
    }
    // And a real -0 still round-trips as negative zero.
    let z: JValue = dacode_json::de::from_slice(&b"[-0]"[..]).expect("pool");
    assert_eq!(z.to_string(), "[-0.0]");
}

/// The ASCII parser must agree with the general one on every ASCII file,
/// and must refuse anything that is not ASCII rather than guess.
#[test]
fn ascii_parser_agrees_wherever_it_applies() {
    let mut ascii_files = 0usize;
    let mut rejected_non_ascii = 0usize;

    for sub in [
        "test_parsing",
        "test_transform",
        "JSON_checker",
        "test_encoding",
    ] {
        for (name, bytes) in load_dir(sub) {
            let general = dacode_json::direct::from_slice::<JValue>(&bytes);
            let ascii = dacode_json::direct::from_slice_ascii::<JValue>(&bytes);

            if dacode_json::direct::is_ascii(&bytes) {
                ascii_files += 1;
                assert_eq!(
                    general.is_ok(),
                    ascii.is_ok(),
                    "{sub}/{name}: verdicts differ on ASCII input"
                );
                if let (Ok(a), Ok(b)) = (general, ascii) {
                    assert_eq!(a, b, "{sub}/{name}: values differ");
                }
            } else {
                rejected_non_ascii += 1;
                assert!(
                    ascii.is_err(),
                    "{sub}/{name}: ASCII parser accepted non-ASCII input"
                );
            }
        }
    }
    assert!(ascii_files > 250, "only {ascii_files} ASCII files");
    assert!(
        rejected_non_ascii > 10,
        "only {rejected_non_ascii} non-ASCII files"
    );
}

/// `is_ascii` must agree with the obvious byte-at-a-time version at every
/// length, since it reads eight bytes at a time.
#[test]
fn is_ascii_matches_a_scalar_scan() {
    for len in 0..40usize {
        for pos in 0..=len {
            let mut v = vec![b'a'; len];
            let want = if pos < len {
                if let Some(slot) = v.get_mut(pos) {
                    *slot = 0x80;
                }
                false
            } else {
                true
            };
            assert_eq!(
                dacode_json::direct::is_ascii(&v),
                want,
                "len={len} high byte at {pos}"
            );
            assert_eq!(
                dacode_json::direct::is_ascii(&v),
                v.iter().all(|b| *b < 0x80)
            );
        }
    }
}

/// Everything else stays checked: the ASCII path skips UTF-8 validation,
/// not validation.
#[test]
fn ascii_parser_still_rejects_malformed_json() {
    let bad: &[&str] = &[
        r#"{"a":01}"#,
        r#"{"a":1,}"#,
        r#"{"a":tru}"#,
        r#"{"a":"\q"}"#,
        r#"{"a":1"#,
        "[1,]",
        r#"{"a":1} x"#,
    ];
    for src in bad {
        assert!(
            serde_json::from_str::<JValue>(src).is_err(),
            "fix the test: serde_json accepted {src}"
        );
        assert!(
            dacode_json::direct::from_slice_ascii::<JValue>(src.as_bytes()).is_err(),
            "ASCII parser accepted {src}"
        );
    }
    // Raw control bytes in strings are still rejected, even though they
    // are ASCII.
    assert!(dacode_json::direct::from_slice_ascii::<JValue>(b"[\"a\tb\"]").is_err());
}

#[test]
fn rejects_trailing_and_empty_input() {
    for bad in ["", "   ", "\u{feff}{}"] {
        assert!(
            dacode_json::stream::from_slice::<JValue>(bad.as_bytes()).is_err(),
            "accepted {bad:?}"
        );
        assert!(
            dacode_json::direct::from_slice::<JValue>(bad.as_bytes()).is_err(),
            "direct accepted {bad:?}"
        );
    }
}

/// The index is now reserved for ~50% structural density, not the worst
/// case, so denser documents must make the scanner grow it correctly.
/// `[1,1,1,...]` is about 57% structural; `[[[]]]`-style input is 100%.
#[test]
fn documents_denser_than_the_index_estimate_still_parse() {
    let dense: Vec<(String, usize)> = vec![
        // ~57% structural
        (format!("[{}]", vec!["1"; 200_000].join(",")), 200_000),
        // 100% structural: nothing but brackets and commas
        (format!("[{}]", vec!["[]"; 200_000].join(",")), 200_000),
        // all strings: every quote is indexed, so 4 index entries per element
        (format!("[{}]", vec![r#""a""#; 200_000].join(",")), 200_000),
    ];
    for (src, want) in dense {
        let v: JValue = dacode_json::stream::from_slice(src.as_bytes())
            .unwrap_or_else(|e| panic!("dense input failed: {e}"));
        assert_eq!(v.as_array().map(Vec::len), Some(want));
        let d: JValue = dacode_json::direct::from_slice(src.as_bytes())
            .unwrap_or_else(|e| panic!("direct dense input failed: {e}"));
        assert_eq!(d, v);
        // And it must still agree with serde_json.
        let theirs: JValue = serde_json::from_slice(src.as_bytes()).expect("serde_json");
        assert_eq!(v, theirs);
    }
}

/// `size_hint` is a hint, but a wrong one silently mis-sizes every `Vec`.
#[test]
fn size_hint_matches_the_real_element_count() {
    let cases: &[(&str, usize)] = &[
        ("[]", 0),
        ("[1]", 1),
        ("[1,2,3]", 3),
        (r#"["a","b"]"#, 2),
        (r#"[[1,2],[3]]"#, 2),
        (r#"[{"a":1},{"b":2}]"#, 2),
        // brackets and commas inside strings must not be counted
        (r#"["a,b","c]d","[e"]"#, 3),
        ("[[],[],[]]", 3),
        ("[null,true,false]", 3),
    ];
    for (src, want) in cases {
        // A Vec<IgnoredAny> is built purely through SeqAccess, so if the
        // hint were wrong the length would still be right - compare the
        // parsed length against serde_json as the oracle.
        let ours: Vec<JValue> = dacode_json::stream::from_slice(src.as_bytes())
            .unwrap_or_else(|e| panic!("{src}: {e}"));
        assert_eq!(ours.len(), *want, "{src}");
        let theirs: Vec<JValue> = serde_json::from_slice(src.as_bytes()).expect("serde_json");
        assert_eq!(ours, theirs, "{src}");
    }
}

#[test]
fn deeply_nested_input_is_rejected_not_overflowed() {
    let deep = format!("{}1{}", "[".repeat(4096), "]".repeat(4096));
    // Must not stack overflow. Either verdict is acceptable, a crash is not.
    let _ = dacode_json::stream::from_slice::<JValue>(deep.as_bytes());
}
