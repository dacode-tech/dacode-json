//! The `serde::Deserializer` over the pool must agree with `serde_json`
//! on every type serde can express.

#![cfg(feature = "serde")]

use dacode_json::corpus;
use dacode_json::de;
use dacode_json::strict::StrictParser;
use serde::Deserialize;
use std::borrow::Cow;
use std::collections::{BTreeMap, HashMap};

/// Round-trip a document through both deserializers and require equality.
#[track_caller]
fn same<T>(json: &str)
where
    T: for<'a> Deserialize<'a> + PartialEq + std::fmt::Debug,
{
    let theirs: T =
        serde_json::from_str(json).unwrap_or_else(|e| panic!("serde_json: {e}\n{json}"));
    let ours: T = de::from_slice(json.as_bytes()).unwrap_or_else(|e| panic!("vela: {e}\n{json}"));
    assert_eq!(ours, theirs, "mismatch on {json}");
}

#[derive(Debug, Deserialize, PartialEq)]
struct Scalars {
    b: bool,
    i: i64,
    u: u32,
    f: f64,
    s: String,
    n: Option<i32>,
}

#[test]
fn scalars() {
    same::<Scalars>(r#"{"b":true,"i":-42,"u":7,"f":3.5,"s":"hi","n":null}"#);
    same::<Scalars>(r#"{"b":false,"i":0,"u":0,"f":-0.125,"s":"","n":9}"#);
}

#[derive(Debug, Deserialize, PartialEq)]
struct Nested {
    id: u64,
    tags: Vec<String>,
    inner: Inner,
    maybe: Option<Inner>,
}

#[derive(Debug, Deserialize, PartialEq)]
struct Inner {
    x: i32,
    y: Vec<i32>,
}

#[test]
fn nested_structures() {
    same::<Nested>(
        r#"{"id":1,"tags":["a","b"],"inner":{"x":1,"y":[1,2,3]},"maybe":{"x":2,"y":[]}}"#,
    );
    same::<Nested>(r#"{"id":0,"tags":[],"inner":{"x":-1,"y":[0]},"maybe":null}"#);
}

#[test]
fn collections() {
    same::<Vec<i64>>("[1,2,3,-4]");
    same::<Vec<Vec<i64>>>("[[1],[2,3],[]]");
    same::<BTreeMap<String, i64>>(r#"{"a":1,"b":2}"#);
    same::<HashMap<String, Vec<bool>>>(r#"{"k":[true,false]}"#);
    same::<(i64, String, bool)>(r#"[1,"x",true]"#);
    same::<Vec<Option<i32>>>("[1,null,3]");
}

#[derive(Debug, Deserialize, PartialEq)]
enum Shape {
    Circle,
    Radius(f64),
    Rect { w: i32, h: i32 },
    Pair(i32, i32),
}

#[test]
fn enums() {
    same::<Shape>(r#""Circle""#);
    same::<Shape>(r#"{"Radius":1.5}"#);
    same::<Shape>(r#"{"Rect":{"w":3,"h":4}}"#);
    same::<Shape>(r#"{"Pair":[1,2]}"#);
    same::<Vec<Shape>>(r#"["Circle",{"Radius":2.0}]"#);
}

#[derive(Debug, Deserialize, PartialEq)]
struct Newtype(i64);

#[derive(Debug, Deserialize, PartialEq)]
struct Unit;

#[test]
fn newtype_and_unit() {
    same::<Newtype>("42");
    same::<Unit>("null");
}

#[test]
fn escapes_are_decoded() {
    same::<String>(r#""a\nb\tc\u0041\ud83d\ude00""#);
    same::<Vec<String>>(r#"["\"","\\","\/"]"#);
    same::<BTreeMap<String, i64>>(r#"{"a\nb":1}"#);
}

#[derive(Debug, Deserialize, PartialEq)]
struct Skipping {
    keep: i64,
}

#[test]
fn unknown_fields_are_skipped_by_subtree_walk() {
    // Skipping is `skip_subtree`, an index walk. Bury the wanted field
    // behind a lot of structure to be sure the walk lands correctly.
    same::<Skipping>(r#"{"junk":{"a":[1,2,{"b":[[[]]]}],"c":"x"},"keep":7,"more":[{"d":null}]}"#);
    same::<Skipping>(r#"{"keep":1,"z":[[[[[[1]]]]]]}"#);
}

// ---------------------------------------------------------------------
// Zero-copy
// ---------------------------------------------------------------------

#[derive(Debug, Deserialize, PartialEq)]
struct Borrowing<'a> {
    #[serde(borrow)]
    clean: Cow<'a, str>,
    #[serde(borrow)]
    dirty: Cow<'a, str>,
}

#[test]
fn strings_borrow_from_the_input_when_unescaped() {
    let json = br#"{"clean":"no escapes here","dirty":"has\nescape"}"#;
    let mut p = StrictParser::new();
    let doc = p.parse(json).expect("valid");
    let got: Borrowing<'_> = de::from_doc(doc).expect("deserialize");

    assert!(
        matches!(got.clean, Cow::Borrowed(_)),
        "unescaped string should borrow from the input, got {:?}",
        got.clean
    );
    assert_eq!(got.clean, "no escapes here");

    assert!(
        matches!(got.dirty, Cow::Owned(_)),
        "escaped string must be owned"
    );
    assert_eq!(got.dirty, "has\nescape");
}

#[test]
fn borrowed_str_fields_work() {
    #[derive(Debug, Deserialize, PartialEq)]
    struct S<'a> {
        name: &'a str,
    }
    let json = br#"{"name":"alpha"}"#;
    let mut p = StrictParser::new();
    let doc = p.parse(json).expect("valid");
    let got: S<'_> = de::from_doc(doc).expect("deserialize");
    assert_eq!(got.name, "alpha");
    // Same allocation as the input.
    assert!(std::ptr::eq(
        got.name.as_ptr(),
        json.as_ptr().wrapping_add(9)
    ));
}

// ---------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------

#[test]
fn type_mismatches_are_errors_not_panics() {
    assert!(de::from_slice::<Scalars>(br#"{"b":1}"#).is_err());
    assert!(de::from_slice::<i64>(br#""not a number""#).is_err());
    assert!(de::from_slice::<Vec<i64>>(br#"{"a":1}"#).is_err());
    assert!(de::from_slice::<Shape>(r#"{"A":1,"B":2}"#.as_bytes()).is_err());
}

#[test]
fn malformed_json_is_rejected() {
    for bad in [&b"{"[..], b"{\"a\":}", b"[1,]", b"3.14.15", b""] {
        assert!(
            de::from_slice::<serde_json::Value>(bad).is_err(),
            "should reject {:?}",
            String::from_utf8_lossy(bad)
        );
    }
}

// ---------------------------------------------------------------------
// Differential against serde_json on generated data
// ---------------------------------------------------------------------

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

#[test]
fn records_corpus_matches_serde_json() {
    let json = corpus::records(2_000, 17);
    let theirs: Vec<Record> = serde_json::from_str(&json).expect("serde_json");
    let ours: Vec<Record> = de::from_slice(json.as_bytes()).expect("vela");
    assert_eq!(ours, theirs);
    assert_eq!(ours.len(), 2_000);
}

/// Compare two `serde_json::Value` trees, tolerating a 1-ULP difference on
/// floats.
///
/// This tolerance is needed because **serde_json is the less accurate of the
/// two**. Its default float parser is not correctly rounded; the crate ships
/// a `float_roundtrip` feature precisely to fix that, off by default. Our
/// strict parser hands the literal to `str::parse::<f64>()`, which is
/// correctly rounded.
///
/// Worked example, `-12715.4527e-19`:
///
/// ```text
/// correctly rounded (Python, str::parse, us) : -0x1.6e7f7b0ed8f08p-50
/// serde_json default                         : -0x1.6e7f7b0ed8f09p-50
/// ```
///
/// See `float_precision_beats_serde_json_default` below, which pins this
/// down rather than papering over it.
fn values_match(a: &serde_json::Value, b: &serde_json::Value) -> bool {
    use serde_json::Value as J;
    match (a, b) {
        (J::Number(x), J::Number(y)) => {
            if x == y {
                return true;
            }
            match (x.as_f64(), y.as_f64()) {
                (Some(x), Some(y)) => {
                    // `.abs()` panics on i64::MIN, which two far-apart bit patterns
                    // can produce. `unsigned_abs` is total.
                    let ulps = (x.to_bits() as i64)
                        .wrapping_sub(y.to_bits() as i64)
                        .unsigned_abs();
                    ulps <= 1
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

#[test]
fn random_documents_match_serde_json_as_value() {
    let mut rng = corpus::Rng::new(0x5E12DE);
    let mut p = StrictParser::new();
    for _ in 0..5_000 {
        let src = corpus::random_value(&mut rng, 4);
        let theirs: serde_json::Value = serde_json::from_str(&src).expect("serde_json");
        let doc = p.parse(src.as_bytes()).expect("strict parse");
        let ours: serde_json::Value = de::from_doc(doc).expect("vela de");
        assert!(
            values_match(&ours, &theirs),
            "on {src}\n ours: {ours}\nserde: {theirs}"
        );
    }
}

/// Our float parsing is correctly rounded; serde_json's default is not.
///
/// Documented as a test so the claim in `values_match` stays honest if
/// either crate changes.
#[test]
fn float_precision_beats_serde_json_default() {
    let mut rng = corpus::Rng::new(0xF10A7);
    let mut p = StrictParser::new();
    let mut differ = 0usize;
    let mut we_are_right = 0usize;

    for _ in 0..20_000 {
        // Enough significant digits to make rounding matter.
        let mantissa = rng.below(1_000_000_000_000_000u64);
        let exp = rng.below(40) as i64 - 20;
        let text = format!("{mantissa}.{}e{exp}", rng.below(100_000_000));

        // `str::parse` is the correctly-rounded reference.
        let Ok(reference) = text.parse::<f64>() else {
            continue;
        };
        if !reference.is_finite() {
            continue;
        }

        let doc = match p.parse(text.as_bytes()) {
            Ok(d) => d,
            Err(_) => continue,
        };
        let Some(ours) = doc.root().as_f64() else {
            continue;
        };
        let theirs: f64 = serde_json::from_str(&text).expect("serde_json parses it");

        assert_eq!(
            ours.to_bits(),
            reference.to_bits(),
            "we deviated from the correctly-rounded value for {text}"
        );
        if theirs.to_bits() != reference.to_bits() {
            differ += 1;
            we_are_right += 1;
        }
    }

    assert_eq!(differ, we_are_right);
    // Not asserting a specific count, only that this is a real effect worth
    // the tolerance in `values_match`.
    assert!(
        differ > 0,
        "expected serde_json's default float parser to deviate at least once"
    );
    println!("serde_json (default) deviated from correctly-rounded on {differ}/20000 literals");
}

/// Type-mismatch messages must read the same as `serde_json`'s, exactly.
///
/// serde's default `Error::invalid_type` formats the offending value with
/// `Display`, and `Unexpected::Float`'s `Display` prints an `f64` with
/// `{}` — which links `core::fmt`'s shortest-round-trip float formatter,
/// 11 848 bytes on `thumbv7em-none-eabihf` for a number in an error
/// string (`docs/SIZE.md`). `crate::errmsg::Unexpected` renders it with
/// `zmij` instead, which is already a dependency and formats into a stack
/// buffer. The text is meant to be unchanged, and this holds it to that.
#[test]
fn type_error_messages_match_serde_json() {
    #[derive(Deserialize, Debug, PartialEq)]
    struct R {
        v: u8,
    }

    // A sanity check that the type is deserializable at all, which also
    // reads the field so it is not dead.
    assert_eq!(
        dacode_json::from_str::<R>(r#"{"v":7}"#).expect("valid"),
        R { v: 7 }
    );

    // `serde_json` appends " at line N column M"; this crate reports a
    // byte offset instead, where it has one. Everything before that must
    // match word for word.
    for src in [
        r#"{"v":"x"}"#,
        r#"{"v":999}"#,
        r#"{"v":-1}"#,
        r#"{"v":[1]}"#,
        r#"{"v":{}}"#,
        r#"{"v":null}"#,
        r#"{"v":true}"#,
        r#"{"w":1}"#,
        // The float arm, which is the one the size fix touched.
        r#"{"v":1.5}"#,
        r#"{"v":-0.0}"#,
        r#"{"v":1e300}"#,
        r#"{"v":1.7976931348623157e308}"#,
    ] {
        let theirs = serde_json::from_str::<R>(src)
            .expect_err("must fail")
            .to_string();
        let ours = dacode_json::from_str::<R>(src)
            .expect_err("must fail")
            .to_string();
        let theirs = theirs.split(" at line ").next().unwrap_or(&theirs);
        assert_eq!(ours, theirs, "{src}");
    }
}
