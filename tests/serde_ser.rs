//! The serializer must produce byte-identical output to `serde_json`
//! (for the escaping policy they share) and round-trip through our own
//! deserializer.

#![cfg(feature = "serde")]
// 3.141592653589793 below is test data, not an attempt at PI.
#![allow(clippy::approx_constant)]

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use dacode_json::corpus;
use dacode_json::ser::{self, Options};
use dacode_json::{de, strict::StrictParser};

/// Byte-for-byte equality with `serde_json::to_string`.
#[track_caller]
fn same<T: Serialize>(v: &T) {
    let theirs = serde_json::to_string(v).expect("serde_json");
    let ours = ser::to_string(v).expect("vela");
    assert_eq!(ours, theirs);
}

#[derive(Debug, Serialize, Deserialize, PartialEq)]
struct Scalars {
    b: bool,
    i: i64,
    u: u32,
    f: f64,
    s: String,
    n: Option<i32>,
}

#[test]
fn scalars_match_serde_json_byte_for_byte() {
    same(&Scalars {
        b: true,
        i: -42,
        u: 7,
        f: 3.5,
        s: "hi".into(),
        n: None,
    });
    same(&Scalars {
        b: false,
        i: i64::MIN,
        u: u32::MAX,
        f: -0.125,
        s: String::new(),
        n: Some(9),
    });
}

#[test]
fn integers_at_the_boundaries() {
    same(&i64::MIN);
    same(&i64::MAX);
    same(&u64::MAX);
    same(&0i64);
    same(&-1i64);
    same(&vec![i8::MIN, -1, 0, 1, i8::MAX]);
    same(&vec![i32::MIN, i32::MAX]);
}

#[test]
fn floats_match() {
    for f in [
        0.0f64,
        -0.0,
        1.0,
        -1.5,
        3.141592653589793,
        1e-300,
        1e300,
        f64::MIN,
        f64::MAX,
        f64::MIN_POSITIVE,
    ] {
        same(&f);
    }
    // JSON has no infinity or NaN; both crates emit null.
    assert_eq!(ser::to_string(&f64::NAN).expect("nan"), "null");
    assert_eq!(ser::to_string(&f64::INFINITY).expect("inf"), "null");
    same(&f64::NAN);
    same(&f64::INFINITY);
}

#[test]
fn escaping_matches_serde_json() {
    // Every control byte, plus the two mandatory escapes.
    for b in 0u8..0x20 {
        let s = format!("a{}b", b as char);
        same(&s);
    }
    same(&"quote\"backslash\\".to_string());
    same(&"tab\tnewline\nreturn\rbell\u{7}".to_string());
    same(&"\u{0}\u{1}\u{1f}".to_string());
    // Non-ASCII passes through unescaped in both.
    same(&"é中文😀".to_string());
    // A long clean run followed by an escape, to exercise the bulk copy.
    same(&format!("{}\n{}", "x".repeat(1000), "y".repeat(1000)));
    // Escape at the very start and very end.
    same(&"\nabc".to_string());
    same(&"abc\n".to_string());
    same(&"\n".to_string());
    same(&String::new());
}

#[test]
fn escapes_at_every_offset_in_a_chunk() {
    // The escaper scans 16 bytes at a time; make sure an escape lands in
    // every lane and every chunk phase.
    for pos in 0..70usize {
        let mut s = "a".repeat(70);
        s.replace_range(pos..pos + 1, "\n");
        same(&s);
    }
}

#[test]
fn solidus_option_reproduces_vela() {
    // serde_json does not escape '/'; Vela's common.vl:74 does.
    assert_eq!(ser::to_string(&"a/b").expect("ok"), r#""a/b""#);

    let mut out = Vec::new();
    ser::to_writer_with(
        &mut out,
        &"a/b",
        Options {
            escape_solidus: true,
        },
    )
    .expect("ok");
    assert_eq!(String::from_utf8(out).expect("utf8"), r#""a\/b""#);
}

#[derive(Debug, Serialize, Deserialize, PartialEq)]
struct Nested {
    id: u64,
    tags: Vec<String>,
    inner: Inner,
    maybe: Option<Inner>,
    map: BTreeMap<String, i64>,
}

#[derive(Debug, Serialize, Deserialize, PartialEq)]
struct Inner {
    x: i32,
    y: Vec<i32>,
}

#[test]
fn nested_structures_match() {
    let v = Nested {
        id: 1,
        tags: vec!["a".into(), "b\"c".into()],
        inner: Inner {
            x: -1,
            y: vec![1, 2, 3],
        },
        maybe: None,
        map: [("k".to_string(), 1i64), ("k2".to_string(), 2)]
            .into_iter()
            .collect(),
    };
    same(&v);
}

#[test]
fn empty_containers_match() {
    same(&Vec::<i32>::new());
    same(&BTreeMap::<String, i32>::new());
    same(&(Vec::<i32>::new(), BTreeMap::<String, i32>::new()));
}

#[derive(Debug, Serialize, Deserialize, PartialEq)]
enum Shape {
    Circle,
    Radius(f64),
    Rect { w: i32, h: i32 },
    Pair(i32, i32),
}

#[test]
fn enums_match() {
    same(&Shape::Circle);
    same(&Shape::Radius(1.5));
    same(&Shape::Rect { w: 3, h: 4 });
    same(&Shape::Pair(1, 2));
    same(&vec![Shape::Circle, Shape::Radius(2.0)]);
}

#[test]
fn tuples_and_newtypes_match() {
    #[derive(Serialize)]
    struct NT(i64);
    #[derive(Serialize)]
    struct TS(i64, String);
    same(&(1i32, "x", true));
    same(&NT(7));
    same(&TS(1, "a".into()));
}

#[test]
fn integer_keys_are_quoted_like_serde_json() {
    let m: BTreeMap<i32, &str> = [(-1, "a"), (2, "b")].into_iter().collect();
    same(&m);
    let m: BTreeMap<u64, &str> = [(0, "a"), (u64::MAX, "b")].into_iter().collect();
    same(&m);
}

#[test]
fn non_string_keys_are_rejected() {
    // JSON objects cannot have float or bool keys.
    let m: BTreeMap<String, i32> = BTreeMap::new();
    assert!(ser::to_string(&m).is_ok());

    #[derive(Serialize, PartialEq, Eq, PartialOrd, Ord)]
    struct BadKey(bool);
    let m: BTreeMap<BadKey, i32> = [(BadKey(true), 1)].into_iter().collect();
    assert!(ser::to_string(&m).is_err());
}

// ---------------------------------------------------------------------
// Round-trip through our own parser
// ---------------------------------------------------------------------

#[test]
fn roundtrip_through_our_own_parser() {
    let v = Nested {
        id: 42,
        tags: vec!["a\nb".into(), "é😀".into(), String::new()],
        inner: Inner {
            x: i32::MIN,
            y: vec![],
        },
        maybe: Some(Inner { x: 0, y: vec![7] }),
        map: [("a\"b".to_string(), -1i64)].into_iter().collect(),
    };

    let text = ser::to_vec(&v).expect("serialize");
    let back: Nested = de::from_slice(&text).expect("deserialize");
    assert_eq!(back, v);
}

#[test]
fn roundtrip_records_corpus() {
    #[derive(Debug, Serialize, Deserialize, PartialEq)]
    struct Record {
        id: u64,
        name: String,
        age: u32,
        active: bool,
        city: String,
        score: i64,
        tags: Vec<String>,
    }

    let json = corpus::records(1_000, 23);
    let parsed: Vec<Record> = de::from_slice(json.as_bytes()).expect("parse");

    // Our output must be byte-identical to serde_json's...
    let ours = ser::to_vec(&parsed).expect("serialize");
    let theirs = serde_json::to_vec(&parsed).expect("serde_json");
    assert_eq!(ours, theirs);

    // ...and must parse back to the same values.
    let back: Vec<Record> = de::from_slice(&ours).expect("reparse");
    assert_eq!(back, parsed);
}

#[test]
fn random_values_roundtrip_and_match_serde_json() {
    let mut rng = corpus::Rng::new(0x5E121);
    let mut p = StrictParser::new();

    for _ in 0..5_000 {
        let src = corpus::random_value(&mut rng, 4);
        // Parse with serde_json so both sides serialize the same tree.
        let v: serde_json::Value = serde_json::from_str(&src).expect("valid");

        let ours = ser::to_string(&v).expect("vela ser");
        let theirs = serde_json::to_string(&v).expect("serde_json ser");
        assert_eq!(ours, theirs, "on {src}");

        // And our output must be accepted by our own strict parser.
        p.validate(ours.as_bytes())
            .unwrap_or_else(|e| panic!("our output is invalid JSON: {e}\n{ours}"));
    }
}

#[test]
fn output_is_always_valid_json() {
    // Fuzz strings specifically: escaping is where a serializer breaks.
    let mut rng = corpus::Rng::new(0xE5CA9E);
    let mut p = StrictParser::new();
    let mut s = String::new();

    for _ in 0..20_000 {
        s.clear();
        for _ in 0..rng.below(40) {
            // Bias hard towards bytes that need escaping.
            let c = match rng.below(6) {
                0 => char::from(rng.below(0x20) as u8),
                1 => '"',
                2 => '\\',
                3 => '/',
                4 => 'é',
                _ => char::from(b'a' + rng.below(26) as u8),
            };
            s.push(c);
        }
        let out = ser::to_string(&s).expect("serialize");
        p.validate(out.as_bytes())
            .unwrap_or_else(|e| panic!("invalid output {out:?}: {e}"));
        assert_eq!(out, serde_json::to_string(&s).expect("serde_json"));

        let back: String = de::from_slice(out.as_bytes()).expect("reparse");
        assert_eq!(back, s);
    }
}
