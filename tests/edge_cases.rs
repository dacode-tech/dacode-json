//! Edge cases, organised along the same lines as yyjson's and simdjson's
//! own unit tests.
//!
//! `tests/conformance_suite.rs` runs their *data*. This covers the
//! categories their *code* tests, which no corpus of `.json` files reaches:
//! API-level behaviour, boundary values, and the encodings a reader has to
//! reject rather than misinterpret.
//!
//! Sections mirror `yyjson/test/test_json_reader.c`
//! (`test_json_encoding`, `test_json_whitespace`, `test_json_incremental`)
//! and `simdjson/tests/dom/{integer_tests,numberparsingcheck}.cpp`.
//!
//! These are written from the categories those suites cover, not
//! mechanically translated — both are table-driven C macros whose
//! assertions do not carry over meaningfully.

use std::fs;
use std::path::Path;
use vela_json::strict::{self, ErrorKind, StrictParser};
use vela_json::{de, ser};

fn testdata(sub: &str) -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata").join(sub)
}

// =====================================================================
// Encoding — yyjson test_json_encoding
// =====================================================================

/// RFC 8259 §8.1: JSON text shall be encoded in UTF-8. A UTF-16 or UTF-32
/// document must be rejected, not silently misread as UTF-8 noise.
#[test]
fn only_utf8_is_accepted() {
    let dir = testdata("test_encoding");
    let mut checked = 0usize;

    for entry in fs::read_dir(&dir).expect("test_encoding").flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let bytes = fs::read(entry.path()).expect("read");
        let ours = strict::validate(&bytes).is_ok();
        checked += 1;

        if name.starts_with("utf8") && !name.contains("bom") {
            assert!(ours, "{name}: plain UTF-8 must be accepted");
        } else {
            // Everything else is UTF-16/32, or UTF-8 with a BOM. RFC 8259
            // §8.1 forbids a BOM too.
            assert!(!ours, "{name}: must be rejected, it is not bare UTF-8");
        }

        // Whatever we decide, serde_json must decide the same.
        assert_eq!(
            ours,
            serde_json::from_slice::<serde_json::Value>(&bytes).is_ok(),
            "{name}: disagreed with serde_json"
        );
    }
    assert_eq!(checked, 10, "expected 10 encoding files");
}

#[test]
fn utf8_bom_is_rejected() {
    // EF BB BF then a valid document.
    let mut buf = vec![0xEF, 0xBB, 0xBF];
    buf.extend_from_slice(br#"{"a":1}"#);
    assert!(strict::validate(&buf).is_err(), "a BOM is not JSON whitespace");
    // Without it, the same bytes are fine.
    assert!(strict::validate(&buf[3..]).is_ok());
}

#[test]
fn invalid_utf8_in_strings_is_rejected() {
    for bad in [
        &b"[\"\xff\"]"[..],           // lone 0xFF
        b"[\"\xc0\x80\"]",            // overlong NUL
        b"[\"\xed\xa0\x80\"]",        // UTF-8-encoded surrogate
        b"[\"\xe2\x28\xa1\"]",        // invalid continuation
        b"[\"\xf8\xa1\xa1\xa1\xa1\"]", // 5-byte sequence
        b"[\"abc\xc3\"]",             // truncated 2-byte sequence
    ] {
        assert!(
            strict::validate(bad).is_err(),
            "should reject invalid UTF-8: {bad:?}"
        );
        assert!(serde_json::from_slice::<serde_json::Value>(bad).is_err());
    }
}

#[test]
fn valid_multibyte_utf8_is_accepted() {
    for good in [
        r#"["é"]"#,
        r#"["中文"]"#,
        r#"["😀"]"#,
        r#"["\u00e9"]"#,
        r#"["\ud83d\ude00"]"#,
        r#"{"é":"中"}"#,
    ] {
        strict::validate(good.as_bytes())
            .unwrap_or_else(|e| panic!("should accept {good}: {e}"));
    }
}

// =====================================================================
// Whitespace — yyjson test_json_whitespace
// =====================================================================

#[test]
fn only_the_four_whitespace_characters_are_allowed() {
    // RFC 8259 §2: space, tab, LF, CR. Nothing else.
    for ws in [" ", "\t", "\n", "\r", " \t\r\n "] {
        let doc = format!("{ws}[{ws}1{ws},{ws}2{ws}]{ws}");
        strict::validate(doc.as_bytes())
            .unwrap_or_else(|e| panic!("should accept {doc:?}: {e}"));
    }
    // Form feed, vertical tab, NUL, NBSP and Unicode spaces are not.
    for bad in ["\x0c", "\x0b", "\0", "\u{a0}", "\u{2028}", "\u{feff}"] {
        let doc = format!("{bad}[1]");
        assert!(
            strict::validate(doc.as_bytes()).is_err(),
            "{bad:?} is not JSON whitespace"
        );
    }
}

#[test]
fn whitespace_only_input_is_rejected() {
    for s in ["", " ", "\t\n\r", "   \n  "] {
        assert_eq!(
            strict::validate(s.as_bytes()).map_err(|e| e.kind),
            Err(ErrorKind::Empty),
            "on {s:?}"
        );
    }
}

// =====================================================================
// Truncation — yyjson test_json_incremental
// =====================================================================

/// Every proper prefix of a valid document must be rejected, and none may
/// panic. This is the property tier 1's `json_validate` fails outright
/// (`docs/TIERS.md`), so it is worth pinning for the strict parser.
#[test]
fn every_prefix_of_a_valid_document_is_rejected() {
    let docs = [
        r#"{"a":1,"b":[1,2,{"c":null}],"d":"e"}"#,
        r#"[1,2.5,true,false,null,"s",{"k":[]}]"#,
        r#"{"nested":{"deep":{"deeper":[1,[2,[3]]]}}}"#,
        r#"["\u0041\ud83d\ude00\n\t"]"#,
    ];
    for doc in docs {
        let bytes = doc.as_bytes();
        assert!(strict::validate(bytes).is_ok(), "{doc} should be valid");
        for n in 0..bytes.len() {
            let prefix = &bytes[..n];
            assert!(
                strict::validate(prefix).is_err(),
                "prefix of length {n} was accepted: {:?}",
                String::from_utf8_lossy(prefix)
            );
        }
    }
}

#[test]
fn trailing_content_is_rejected() {
    for s in [
        r#"{} {}"#, r#"[1][2]"#, "1 2", "null null", r#""a" "b""#,
        r#"{"a":1}x"#, "[1],", "{}\0",
    ] {
        assert!(
            strict::validate(s.as_bytes()).is_err(),
            "should reject trailing content: {s:?}"
        );
    }
}

// =====================================================================
// Numbers — simdjson integer_tests / numberparsingcheck
// =====================================================================

#[test]
fn integer_boundaries_are_exact() {
    let mut p = StrictParser::new();
    for v in [
        0i64, 1, -1, 42, -42,
        i64::MAX, i64::MIN,
        i64::MAX - 1, i64::MIN + 1,
        i32::MAX as i64, i32::MIN as i64,
        (1i64 << 53), -(1i64 << 53),      // f64 exact-integer limit
        (1i64 << 53) + 1,                 // first integer f64 cannot hold
    ] {
        let lit = v.to_string();
        let doc = p.parse(lit.as_bytes()).unwrap_or_else(|e| panic!("{lit}: {e}"));
        assert_eq!(doc.root().as_i64(), Some(v), "on {lit}");
    }
}

#[test]
fn values_beyond_i64_become_floats() {
    let mut p = StrictParser::new();
    for lit in [
        "9223372036854775808",             // i64::MAX + 1
        "18446744073709551615",            // u64::MAX
        "-9223372036854775809",            // i64::MIN - 1
        "123456789012345678901234567890",
    ] {
        let doc = p.parse(lit.as_bytes()).unwrap_or_else(|e| panic!("{lit}: {e}"));
        assert_eq!(doc.root().as_i64(), None, "{lit} should not be an i64");
        let ours = doc.root().as_f64().unwrap_or_else(|| panic!("{lit} not a float"));
        // Must be correctly rounded.
        assert_eq!(ours.to_bits(), lit.parse::<f64>().expect("ref").to_bits(), "on {lit}");
    }
}

#[test]
fn float_boundaries() {
    let mut p = StrictParser::new();
    for lit in [
        "0.0", "-0.0", "1.0", "-1.5", "3.141592653589793",
        "1e308", "-1e308", "5e-324",       // f64::MAX-ish, smallest subnormal
        "2.2250738585072014e-308",         // smallest normal
        "1.7976931348623157e308",          // f64::MAX
        "0e0", "0E0", "0.0e0", "1e+3", "1E-3",
    ] {
        let doc = p.parse(lit.as_bytes()).unwrap_or_else(|e| panic!("{lit}: {e}"));
        let ours = doc.root().as_f64().unwrap_or_else(|| panic!("{lit} not numeric"));
        assert_eq!(
            ours.to_bits(),
            lit.parse::<f64>().expect("ref").to_bits(),
            "on {lit}: not correctly rounded"
        );
    }
}

#[test]
fn overflow_to_infinity_is_rejected() {
    // A literal that cannot be represented is an error, not `inf`.
    for lit in ["1e309", "-1e309", "1e99999", "1e1000"] {
        assert!(
            strict::validate(lit.as_bytes()).is_err(),
            "{lit} overflows f64 and should be rejected"
        );
        assert!(serde_json::from_str::<serde_json::Value>(lit).is_err());
    }
}

#[test]
fn negative_zero_keeps_its_sign() {
    let mut p = StrictParser::new();
    let doc = p.parse(b"-0").expect("valid");
    let v = doc.root().as_f64().expect("numeric");
    assert!(v.is_sign_negative(), "-0 lost its sign");
    assert_eq!(v, 0.0);
    // And +0 does not acquire one.
    let doc = p.parse(b"0").expect("valid");
    assert_eq!(doc.root().as_i64(), Some(0));
}

#[test]
fn malformed_number_grammar() {
    for lit in [
        "01", "-01", "00", "1.", ".1", "-", "+1", "1e", "1e+", "1.e3",
        "0x10", "1..2", "--1", "1e1e1", "Infinity", "-Infinity", "NaN",
        "1_000", "0b101", ".", "-.5", "1.2.3",
    ] {
        assert!(
            strict::validate(lit.as_bytes()).is_err(),
            "should reject {lit:?}"
        );
    }
}

// =====================================================================
// Strings — yyjson test_string
// =====================================================================

#[test]
fn all_two_character_escapes() {
    let mut p = StrictParser::new();
    let cases: &[(&str, &str)] = &[
        (r#""\"""#, "\""),
        (r#""\\""#, "\\"),
        (r#""\/""#, "/"),
        (r#""\b""#, "\u{8}"),
        (r#""\f""#, "\u{c}"),
        (r#""\n""#, "\n"),
        (r#""\r""#, "\r"),
        (r#""\t""#, "\t"),
    ];
    for (json, want) in cases {
        let doc = p.parse(json.as_bytes()).unwrap_or_else(|e| panic!("{json}: {e}"));
        assert_eq!(doc.root().as_str().as_deref(), Some(*want), "on {json}");
    }
}

#[test]
fn invalid_escapes_are_rejected() {
    for s in [
        r#""\x""#, r#""\a""#, r#""\v""#, r#""\0""#, r#""\ ""#,
        r#""\u""#, r#""\u0""#, r#""\u00""#, r#""\u000""#, r#""\uZZZZ""#,
        r#""\u00G0""#, r#""\"#,
    ] {
        assert!(strict::validate(s.as_bytes()).is_err(), "should reject {s}");
    }
}

#[test]
fn surrogate_pairs() {
    let mut p = StrictParser::new();
    // Well-formed pairs decode.
    for (json, want) in [
        (r#""\ud83d\ude00""#, "😀"),
        (r#""\uD83D\uDE00""#, "😀"),
        (r#""\ud800\udc00""#, "\u{10000}"),
        (r#""\udbff\udfff""#, "\u{10FFFF}"),
    ] {
        let doc = p.parse(json.as_bytes()).unwrap_or_else(|e| panic!("{json}: {e}"));
        assert_eq!(doc.root().as_str().as_deref(), Some(want), "on {json}");
    }
    // Lone or mispaired surrogates are not.
    for bad in [
        r#""\ud800""#,          // high, unpaired
        r#""\udc00""#,          // low, unpaired
        r#""\ud800\ud800""#,    // high followed by high
        r#""\udc00\udc00""#,    // low followed by low
        r#""\ud800x""#,         // high followed by a normal character
        r#""\ud800\u0041""#,    // high followed by a non-surrogate escape
    ] {
        assert!(strict::validate(bad.as_bytes()).is_err(), "should reject {bad}");
    }
}

#[test]
fn raw_control_characters_are_rejected() {
    // RFC 8259 §7: characters below 0x20 must be escaped.
    for c in 0u8..0x20 {
        let doc = format!("\"a{}b\"", c as char);
        assert!(
            strict::validate(doc.as_bytes()).is_err(),
            "raw control byte {c:#04x} must be rejected"
        );
    }
    // 0x20 and above are fine.
    assert!(strict::validate(b"\"a b\"").is_ok());
}

#[test]
fn escaped_control_characters_are_accepted() {
    let mut p = StrictParser::new();
    for c in 0u8..0x20 {
        let doc = format!(r#""a\u{c:04x}b""#);
        let parsed = p.parse(doc.as_bytes()).unwrap_or_else(|e| panic!("{doc}: {e}"));
        let s = parsed.root().as_str().expect("string");
        assert_eq!(s.chars().count(), 3, "on {doc}");
        assert_eq!(s.chars().nth(1), Some(c as char), "on {doc}");
    }
}

#[test]
fn embedded_nul_survives_a_round_trip() {
    // A NUL inside a string is legal when escaped, and must not truncate.
    let json = r#"{"a\u0000b":"c\u0000d"}"#;
    let v: serde_json::Value = de::from_slice(json.as_bytes()).expect("parse");
    let obj = v.as_object().expect("object");
    assert_eq!(obj.len(), 1);
    let (k, val) = obj.iter().next().expect("entry");
    assert_eq!(k.as_bytes(), b"a\0b");
    assert_eq!(val.as_str().map(str::as_bytes), Some(&b"c\0d"[..]));

    let out = ser::to_string(&v).expect("serialize");
    assert_eq!(out, serde_json::to_string(&v).expect("serde_json"));
    let back: serde_json::Value = de::from_slice(out.as_bytes()).expect("reparse");
    assert_eq!(back, v);
}

#[test]
fn empty_and_long_strings() {
    let mut p = StrictParser::new();
    assert_eq!(p.parse(br#""""#).expect("valid").root().as_str().as_deref(), Some(""));

    // Long enough to cross every SIMD chunk boundary in the scanner and
    // the escaper.
    for n in [15usize, 16, 17, 31, 32, 33, 63, 64, 65, 1000] {
        let s = "x".repeat(n);
        let json = format!("\"{s}\"");
        let doc = p.parse(json.as_bytes()).unwrap_or_else(|e| panic!("len {n}: {e}"));
        assert_eq!(doc.root().as_str().as_deref(), Some(s.as_str()), "len {n}");
    }
}

// =====================================================================
// Structure
// =====================================================================

#[test]
fn empty_and_nested_containers() {
    for s in [
        "{}", "[]", "[[]]", "[{}]", "{\"a\":{}}", "{\"a\":[]}",
        "[[],[]]", "[{},{}]", "[[[[[[[[[[]]]]]]]]]]",
    ] {
        strict::validate(s.as_bytes()).unwrap_or_else(|e| panic!("{s}: {e}"));
    }
}

#[test]
fn duplicate_and_empty_keys() {
    let mut p = StrictParser::new();
    // RFC 8259 permits duplicates; the pool keeps both, like a multimap.
    let doc = p.parse(br#"{"a":1,"a":2}"#).expect("valid");
    assert_eq!(doc.root().len(), 2, "both pairs should be retained");
    // First match wins on lookup, matching document order.
    assert_eq!(doc.root().get("a").and_then(|v| v.as_i64()), Some(1));

    // serde_json's map keeps the last; that is its choice, not an error.
    let v: serde_json::Value = serde_json::from_str(r#"{"a":1,"a":2}"#).expect("valid");
    assert_eq!(v.as_object().map(serde_json::Map::len), Some(1));

    // Empty keys are legal.
    let doc = p.parse(br#"{"":0,"a":1}"#).expect("valid");
    assert_eq!(doc.root().len(), 2);
    assert_eq!(doc.root().get("").and_then(|v| v.as_i64()), Some(0));
}

#[test]
fn deep_nesting_is_bounded_not_crashing() {
    // Past the depth limit must be a clean error, never a stack overflow.
    // STACK_MAX is the number of levels permitted, so 256 is the last
    // accepted depth and 257 is the first rejected one.
    for depth in [100usize, 255, 256, 257, 1000, 100_000] {
        let doc = format!("{}1{}", "[".repeat(depth), "]".repeat(depth));
        let r = strict::validate(doc.as_bytes());
        if depth <= vela_json::pool::STACK_MAX {
            assert!(r.is_ok(), "depth {depth} should be accepted: {r:?}");
        } else {
            assert_eq!(
                r.map_err(|e| e.kind),
                Err(ErrorKind::DepthLimitExceeded),
                "depth {depth} should hit the limit"
            );
        }
    }
}

#[test]
fn unbalanced_brackets_of_every_shape() {
    for s in [
        "[", "]", "{", "}", "[}", "{]", "[{]}", "{[}]",
        "[[]", "[]]", "{\"a\":1", "{\"a\":1]}", "[1,2}",
    ] {
        assert!(strict::validate(s.as_bytes()).is_err(), "should reject {s:?}");
    }
}

// =====================================================================
// Round-trip — yyjson test_roundtrip
// =====================================================================

#[test]
fn roundtrip_is_a_fixed_point() {
    let docs = [
        r#"{"a":1,"b":[1,2,3],"c":{"d":null},"e":true,"f":-1.5}"#,
        r#"["\u0000\u001f\"\\","é中😀"]"#,
        r#"[0,-0.0,1e308,5e-324,9223372036854775807]"#,
        r#"{"":"","a":{"b":{"c":[[[]]]}}}"#,
    ];
    for doc in docs {
        let v: serde_json::Value = de::from_slice(doc.as_bytes())
            .unwrap_or_else(|e| panic!("{doc}: {e}"));
        let once = ser::to_string(&v).unwrap_or_else(|e| panic!("{doc}: {e}"));
        let back: serde_json::Value = de::from_slice(once.as_bytes())
            .unwrap_or_else(|e| panic!("reparse {once}: {e}"));
        let twice = ser::to_string(&back).expect("re-serialize");
        assert_eq!(once, twice, "not idempotent for {doc}");
        // And byte-identical to serde_json's rendering of the same tree.
        assert_eq!(once, serde_json::to_string(&v).expect("serde_json"), "on {doc}");
    }
}
