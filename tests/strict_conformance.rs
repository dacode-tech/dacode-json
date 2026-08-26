//! RFC 8259 conformance for the strict parser, plus a differential test
//! against `serde_json`.
//!
//! Accept/reject cases are drawn from the JSONTestSuite `y_`/`n_` naming
//! convention (Nicolas Seriot, "Parsing JSON is a Minefield").

// 3.14 is test data, not an attempt at PI.
#![allow(clippy::approx_constant)]

use dacodec::strict::{self, ErrorKind, StrictParser};
use dacodec::{corpus, Type};

#[track_caller]
fn accept(src: &str) {
    if let Err(e) = strict::validate(src.as_bytes()) {
        panic!("should accept {src:?}: {e}");
    }
    // Anything we accept, serde_json must accept too.
    assert!(
        serde_json::from_str::<serde_json::Value>(src).is_ok(),
        "we accepted {src:?} but serde_json rejects it"
    );
}

#[track_caller]
fn reject(src: &str) {
    if strict::validate(src.as_bytes()).is_ok() {
        panic!("should reject {src:?}");
    }
    assert!(
        serde_json::from_str::<serde_json::Value>(src).is_err(),
        "we rejected {src:?} but serde_json accepts it"
    );
}

#[test]
fn y_structures() {
    for s in [
        "{}",
        "[]",
        "[ ]",
        "{ }",
        r#"{"a":1}"#,
        r#"{"a":1,"b":2}"#,
        "[1,2,3]",
        "[[[[[]]]]]",
        r#"{"a":{"b":{"c":[]}}}"#,
        r#"[{"a":[{"b":1}]}]"#,
        "[null,true,false]",
        r#"{"":0}"#,
        "\t\r\n [1] \t\r\n ",
    ] {
        accept(s);
    }
}

#[test]
fn y_numbers() {
    for s in [
        "0", "-0", "1", "-1", "42", "1.0", "-1.5", "1e3", "1E3", "1e+3", "1e-3", "1.5e10",
        "-0.0e-0", "123456789012345678901234567890", "1e308",
    ] {
        accept(s);
    }
}

#[test]
fn y_strings() {
    for s in [
        r#""""#,
        r#""a""#,
        r#""\"""#,
        r#""\\""#,
        r#""\/""#,
        r#""\b\f\n\r\t""#,
        r#""\u0000""#,
        r#""\uFFFF""#,
        r#""\ud83d\ude00""#,
        "\"é\"",
        "\"中文\"",
        r#""{[,:]}""#,
    ] {
        accept(s);
    }
}

#[test]
fn n_structures() {
    for s in [
        "",
        " ",
        "{",
        "}",
        "[",
        "]",
        "[,]",
        "[1,]",
        "{,}",
        r#"{"a":1,}"#,
        r#"{"a"}"#,
        r#"{"a":}"#,
        r#"{:1}"#,
        r#"{a:1}"#,
        "[1 2]",
        r#"{"a":1"b":2}"#,
        "[}",
        "{]",
        r#"{"a":1]"#,
        "[1,2}",
        "[[]",
        "[]]",
        r#"{"a":1}{"b":2}"#,
        "[1][2]",
        "1 2",
        "nulll",
        "tru",
        "fals",
        "NULL",
        "True",
        "'single'",
        r#"{"a": 1,, "b": 2}"#,
    ] {
        reject(s);
    }
}

#[test]
fn n_numbers() {
    for s in [
        "01", "-01", "00", "1.", ".1", "-", "+1", "1e", "1e+", "1.e3", "0x1", "1..2", "--1",
        "1e1e1",
    ] {
        reject(s);
    }
}

#[test]
fn n_strings() {
    for s in [
        r#""unterminated"#,
        r#""\x""#,
        r#""\u00""#,
        r#""\uZZZZ""#,
        "\"raw\nnewline\"",
        "\"raw\ttab\"",
    ] {
        reject(s);
    }
}

#[test]
fn error_kinds_and_offsets() {
    let cases: &[(&str, ErrorKind)] = &[
        ("", ErrorKind::Empty),
        ("   ", ErrorKind::Empty),
        (r#"{"a":1]"#, ErrorKind::MismatchedBracket),
        (r#"{"a":1}x"#, ErrorKind::TrailingContent),
        (r#"{"a" 1}"#, ErrorKind::ExpectedColon),
        (r#"{1:2}"#, ErrorKind::ExpectedKey),
        (r#"{"a":1,}"#, ErrorKind::ExpectedKey),
        ("[1,]", ErrorKind::ExpectedValue),
        ("01", ErrorKind::InvalidNumber),
        ("nulll", ErrorKind::TrailingContent),
        ("tru", ErrorKind::InvalidLiteral),
        (r#"{"a":1"#, ErrorKind::UnexpectedEof),
        (r#""\x""#, ErrorKind::InvalidString),
    ];

    for (src, kind) in cases {
        match strict::validate(src.as_bytes()) {
            Ok(()) => panic!("{src:?} should have failed"),
            Err(e) => assert_eq!(e.kind, *kind, "{src:?} -> {e}"),
        }
    }
}

#[test]
fn error_offset_points_at_the_problem() {
    let e = strict::validate(br#"{"a":1,"b":}"#).expect_err("must fail");
    assert_eq!(e.offset, 11, "should point at the '}}': {e}");
}

#[test]
fn depth_limit_is_reported_not_silent() {
    let mut src = String::new();
    for _ in 0..300 {
        src.push('[');
    }
    for _ in 0..300 {
        src.push(']');
    }
    let e = strict::validate(src.as_bytes()).expect_err("must fail");
    assert_eq!(e.kind, ErrorKind::DepthLimitExceeded);
}

#[test]
fn numbers_are_real_numbers() {
    let mut p = StrictParser::new();
    let doc = p
        .parse(br#"{"i":42,"neg":-7,"pi":3.14,"e":1e3,"big":123456789012345678901234567890}"#)
        .expect("valid");
    let r = doc.root();

    assert_eq!(r.get("i").and_then(|v| v.as_i64()), Some(42));
    assert_eq!(r.get("neg").and_then(|v| v.as_i64()), Some(-7));

    // These are the ones the faithful parser mangles into 314 and 13.
    assert_eq!(r.get("pi").map(|v| v.typ()), Some(Type::Float));
    assert_eq!(r.get("pi").and_then(|v| v.as_f64()), Some(3.14));
    assert_eq!(r.get("e").and_then(|v| v.as_f64()), Some(1000.0));

    // i64 overflow degrades to f64, like serde_json without
    // `arbitrary_precision`.
    assert_eq!(r.get("big").map(|v| v.typ()), Some(Type::Float));
    assert_eq!(r.get("big").and_then(|v| v.as_f64()), Some(1.2345678901234568e29));
}

#[test]
fn strings_still_borrow() {
    let mut p = StrictParser::new();
    let doc = p.parse(br#"{"clean":"no escapes","dirty":"a\nb"}"#).expect("valid");
    let r = doc.root();

    assert!(matches!(
        r.get("clean").and_then(|v| v.as_str()),
        Some(std::borrow::Cow::Borrowed("no escapes"))
    ));
    assert_eq!(r.get("dirty").and_then(|v| v.as_str()).as_deref(), Some("a\nb"));
}

// ---------------------------------------------------------------------
// Differential testing against serde_json
// ---------------------------------------------------------------------

/// Compare the strict parser's tree with `serde_json::Value`, node by node.
fn same(v: &dacodec::Value<'_>, j: &serde_json::Value) -> Result<(), String> {
    match (v.typ(), j) {
        (Type::Null, serde_json::Value::Null) => Ok(()),
        (Type::Bool, serde_json::Value::Bool(b)) => {
            if v.as_bool() == Some(*b) {
                Ok(())
            } else {
                Err(format!("bool {:?} != {b}", v.as_bool()))
            }
        }
        (Type::Number | Type::Float, serde_json::Value::Number(n)) => {
            let (Some(a), Some(b)) = (v.as_f64(), n.as_f64()) else {
                return Err(format!("number {n} not comparable"));
            };
            if a == b || (a - b).abs() <= f64::EPSILON * a.abs().max(b.abs()) {
                Ok(())
            } else {
                Err(format!("number {a} != {b}"))
            }
        }
        (Type::String, serde_json::Value::String(s)) => match v.as_str() {
            Some(got) if got == s.as_str() => Ok(()),
            other => Err(format!("string {other:?} != {s:?}")),
        },
        (Type::Array, serde_json::Value::Array(items)) => {
            if v.len() != items.len() {
                return Err(format!("array len {} != {}", v.len(), items.len()));
            }
            for (e, ji) in v.elements().zip(items.iter()) {
                same(&e, ji)?;
            }
            Ok(())
        }
        (Type::Object, serde_json::Value::Object(map)) => {
            if v.len() != map.len() {
                return Err(format!("object len {} != {}", v.len(), map.len()));
            }
            for (k, val) in v.entries() {
                let Some(key) = dacodec::unescape::unescape(k) else {
                    return Err(format!("bad key {:?}", String::from_utf8_lossy(k)));
                };
                let Some(jv) = map.get(key.as_ref()) else {
                    return Err(format!("missing key {key:?}"));
                };
                same(&val, jv)?;
            }
            Ok(())
        }
        (t, j) => Err(format!("type mismatch: {t:?} vs {j}")),
    }
}

#[test]
fn differential_against_serde_json_random() {
    let mut rng = corpus::Rng::new(0xC0FFEE);
    let mut p = StrictParser::new();

    for i in 0..20_000 {
        let src = corpus::random_value(&mut rng, 4);
        let expect: serde_json::Value =
            serde_json::from_str(&src).unwrap_or_else(|e| panic!("generator bug at {i}: {e}\n{src}"));

        let doc = p
            .parse(src.as_bytes())
            .unwrap_or_else(|e| panic!("strict rejected valid JSON: {e}\n{src}"));

        if let Err(why) = same(&doc.root(), &expect) {
            panic!("mismatch: {why}\n{src}");
        }
    }
}

#[test]
fn differential_against_serde_json_corpora() {
    let mut p = StrictParser::new();
    for (name, src) in corpus::suite(200_000, 4242) {
        let expect: serde_json::Value = serde_json::from_str(&src).expect("corpus is valid");
        let doc = p.parse(src.as_bytes()).unwrap_or_else(|e| panic!("{name}: {e}"));
        if let Err(why) = same(&doc.root(), &expect) {
            panic!("{name}: {why}");
        }
    }
}

/// Accept/reject must agree with serde_json on random byte soup, not just on
/// documents we generated.
#[test]
fn differential_accept_reject_on_mutations() {
    let mut rng = corpus::Rng::new(0xBADC0DE);
    let mut p = StrictParser::new();
    let mut agreements = 0usize;
    let mut rejects = 0usize;

    for _ in 0..20_000 {
        let mut src = corpus::random_value(&mut rng, 3).into_bytes();

        // Mutate: delete, duplicate or replace a byte.
        if !src.is_empty() {
            let i = rng.below(src.len() as u64) as usize;
            match rng.below(3) {
                0 => {
                    src.remove(i);
                }
                1 => {
                    let b = src.get(i).copied().unwrap_or(b'x');
                    src.insert(i, b);
                }
                _ => {
                    let alphabet = br#"{}[]",:\ 0aeN"#;
                    if let Some(slot) = src.get_mut(i) {
                        *slot = rng.pick(alphabet.as_slice()).copied().unwrap_or(b'x');
                    }
                }
            }
        }

        let ours = p.validate(&src).is_ok();
        let theirs = serde_json::from_slice::<serde_json::Value>(&src).is_ok();

        assert_eq!(
            ours,
            theirs,
            "disagree (ours={ours}, serde={theirs}) on {:?}",
            String::from_utf8_lossy(&src)
        );
        agreements += 1;
        if !ours {
            rejects += 1;
        }
    }

    assert_eq!(agreements, 20_000);
    assert!(rejects > 1000, "mutations barely broke anything: {rejects}");
}
