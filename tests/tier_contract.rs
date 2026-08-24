//! The four tiers against each other.
//!
//! `docs/stage2/P1_2_JSON_TIERS.md:109-141` promises a common interface
//! "so consumers don't need to know which tier is active". These tests
//! check how true that is.
//!
//! Answer: true for tiers 1–3 on **valid** JSON, and false everywhere else.
//! Tier 0 stubs out every container operation, and on malformed input the
//! tiers disagree with each other and with RFC 8259 in several distinct
//! ways. Each of those is pinned down below rather than glossed over.

use std::collections::BTreeSet;
use vela_json::corpus;
use vela_json::tiers::{
    tier0::Tier0,
    tier1::{Pairs, Tier1},
    tier2::Tier2,
    tier3::Tier3,
    JsonTier, Tier, TypeName,
};

// ---------------------------------------------------------------------
// Scalars: all four tiers share one implementation, so all four must agree
// ---------------------------------------------------------------------

#[track_caller]
fn scalars_agree(src: &[u8]) {
    let strings = [
        Tier0::parse_string(src),
        Tier1::parse_string(src),
        Tier2::parse_string(src),
        Tier3::parse_string(src),
    ];
    assert!(
        strings.iter().all(|s| *s == strings[0]),
        "parse_string disagrees on {:?}: {strings:?}",
        String::from_utf8_lossy(src)
    );

    let numbers = [
        Tier0::parse_number(src),
        Tier1::parse_number(src),
        Tier2::parse_number(src),
        Tier3::parse_number(src),
    ];
    assert!(
        numbers.iter().all(|n| *n == numbers[0]),
        "parse_number disagrees on {:?}: {numbers:?}",
        String::from_utf8_lossy(src)
    );

    let bools = [
        Tier0::parse_bool(src),
        Tier1::parse_bool(src),
        Tier2::parse_bool(src),
        Tier3::parse_bool(src),
    ];
    assert!(bools.iter().all(|b| *b == bools[0]), "parse_bool disagrees");

    let types = [
        Tier0::detect_type(src),
        Tier1::detect_type(src),
        Tier2::detect_type(src),
        Tier3::detect_type(src),
    ];
    assert!(
        types.iter().all(|t| *t == types[0]),
        "detect_type disagrees on {:?}: {types:?}",
        String::from_utf8_lossy(src)
    );

    let valid = [
        Tier0::validate_string(src),
        Tier1::validate_string(src),
        Tier2::validate_string(src),
        Tier3::validate_string(src),
    ];
    assert!(
        valid.iter().all(|v| *v == valid[0]),
        "validate_string disagrees"
    );
}

#[test]
fn every_tier_agrees_on_scalars() {
    for s in [
        &b""[..],
        b"   ",
        b"42",
        b"-7",
        b"0",
        b"3.14",
        b"1e5",
        b"true",
        b"false",
        b"null",
        br#""hello""#,
        br#""a\nb""#,
        br#""a\u0041b""#,
        br#""a\bb""#,
        b"garbage",
        b"tru",
        b"{",
        b"[",
    ] {
        scalars_agree(s);
    }
}

#[test]
fn every_tier_agrees_on_random_scalars() {
    let mut rng = corpus::Rng::new(0x5CA1A5);
    let mut buf = Vec::with_capacity(40);
    for _ in 0..20_000 {
        buf.clear();
        for _ in 0..rng.below(40) {
            buf.push((rng.next_u64() & 0xFF) as u8);
        }
        scalars_agree(&buf);
    }
}

// ---------------------------------------------------------------------
// Containers: tiers 1-3 must agree on valid JSON. Tier 0 must not.
// ---------------------------------------------------------------------

#[track_caller]
fn containers_agree(src: &[u8]) {
    let counts = [
        Tier1::object_count(src),
        Tier2::object_count(src),
        Tier3::object_count(src),
    ];
    assert!(
        counts.iter().all(|c| *c == counts[0]),
        "object_count disagrees on {:?}: {counts:?}",
        String::from_utf8_lossy(src)
    );

    let acounts = [
        Tier1::array_count(src),
        Tier2::array_count(src),
        Tier3::array_count(src),
    ];
    assert!(
        acounts.iter().all(|c| *c == acounts[0]),
        "array_count disagrees on {:?}: {acounts:?}",
        String::from_utf8_lossy(src)
    );

    // Keys: as a set, since tier 3's pool preserves document order but the
    // contract does not promise ordering.
    let keys: Vec<BTreeSet<String>> = [
        Tier1::object_key_list(src),
        Tier2::object_key_list(src),
        Tier3::object_key_list(src),
    ]
    .iter()
    .map(|v| v.iter().map(|c| c.to_string()).collect())
    .collect();
    assert!(
        keys.iter().all(|k| *k == keys[0]),
        "object keys disagree on {:?}: {keys:?}",
        String::from_utf8_lossy(src)
    );

    // And every key must resolve to the same raw value in all three.
    for k in &keys[0] {
        let vals = [
            Tier1::object_get(src, k),
            Tier2::object_get(src, k),
            Tier3::object_get(src, k),
        ];
        assert!(
            vals.iter().all(|v| *v == vals[0]),
            "object_get({k:?}) disagrees on {:?}: {vals:?}",
            String::from_utf8_lossy(src)
        );
    }

    for i in 0..acounts[0].min(64) {
        let vals = [
            Tier1::array_get(src, i),
            Tier2::array_get(src, i),
            Tier3::array_get(src, i),
        ];
        assert!(
            vals.iter().all(|v| *v == vals[0]),
            "array_get({i}) disagrees on {:?}",
            String::from_utf8_lossy(src)
        );
    }
}

#[test]
fn tiers_1_to_3_agree_on_handwritten_documents() {
    for s in [
        &b"{}"[..],
        b"[]",
        b"{ }",
        b"[ ]",
        br#"{"a":1}"#,
        br#"{"a":1,"b":2,"c":3}"#,
        br#"{"a":"x","b":[1,2],"c":{"d":null}}"#,
        b"[1,2,3]",
        br#"[1,"two",[3],{"f":4},null,true,false]"#,
        br#"{"nested":{"deep":{"deeper":[1,[2,[3]]]}}}"#,
        b"  {\n \"a\" : [ 1 , 2 ] ,\t\"b\" : 3 }  ",
        br#"{"":0}"#,
        br#"{"k":"v with , comma"}"#,
        br#"{"esc":"a\"b"}"#,
        b"[[[[[]]]]]",
        br#"[{"a":[{"b":1}]}]"#,
    ] {
        containers_agree(s);
    }
}

#[test]
fn tiers_1_to_3_agree_on_generated_corpora() {
    for (name, json) in corpus::suite(120_000, 77) {
        let src = json.as_bytes();
        let counts = [
            Tier1::array_count(src),
            Tier2::array_count(src),
            Tier3::array_count(src),
        ];
        assert!(
            counts.iter().all(|c| *c == counts[0]),
            "{name}: array_count {counts:?}"
        );
        let ocounts = [
            Tier1::object_count(src),
            Tier2::object_count(src),
            Tier3::object_count(src),
        ];
        assert!(
            ocounts.iter().all(|c| *c == ocounts[0]),
            "{name}: object_count {ocounts:?}"
        );
    }
}

#[test]
fn tiers_1_to_3_agree_on_random_valid_documents() {
    let mut rng = corpus::Rng::new(0x71E45);
    for _ in 0..20_000 {
        containers_agree(corpus::random_value(&mut rng, 4).as_bytes());
    }
}

/// Every raw slice a tier hands back must itself be a valid JSON value.
#[test]
fn returned_slices_are_well_formed() {
    let mut rng = corpus::Rng::new(0x51CE5);
    for _ in 0..5_000 {
        let doc = corpus::random_value(&mut rng, 4);
        let src = doc.as_bytes();

        for k in Tier1::object_key_list(src) {
            if let Some(v) = Tier1::object_get(src, &k) {
                assert!(
                    serde_json::from_slice::<serde_json::Value>(v).is_ok(),
                    "object_get({k:?}) returned invalid JSON {:?} from {doc}",
                    String::from_utf8_lossy(v)
                );
            }
        }
        for i in 0..Tier1::array_count(src).min(16) {
            if let Some(v) = Tier1::array_get(src, i) {
                assert!(
                    serde_json::from_slice::<serde_json::Value>(v).is_ok(),
                    "array_get({i}) returned invalid JSON {:?} from {doc}",
                    String::from_utf8_lossy(v)
                );
            }
        }
    }
}

/// Cross-check the navigation against `serde_json` rather than only against
/// each other — three tiers agreeing on a wrong answer is still wrong.
#[test]
fn navigation_matches_serde_json() {
    let mut rng = corpus::Rng::new(0x5E12E);
    for _ in 0..10_000 {
        let doc = corpus::random_value(&mut rng, 4);
        let src = doc.as_bytes();
        let Ok(expect) = serde_json::from_str::<serde_json::Value>(&doc) else {
            continue;
        };

        match &expect {
            serde_json::Value::Object(map) => {
                assert_eq!(Tier1::object_count(src), map.len(), "on {doc}");
                assert_eq!(Tier2::object_count(src), map.len(), "on {doc}");
                assert_eq!(Tier3::object_count(src), map.len(), "on {doc}");

                // Walk the tier's own raw keys rather than serde_json's
                // decoded ones. The tiers compare keys byte-for-byte
                // against the document, so `\ud83d\ude00` in the source is
                // a different key from the emoji it decodes to; iterating
                // decoded keys and looking them up raw would compare the
                // wrong things.
                for (raw_key, raw_val) in Pairs::new(src) {
                    let decoded = vela_json::unescape::unescape(raw_key)
                        .expect("key is valid UTF-8 with valid escapes");

                    let want = map
                        .get(decoded.as_ref())
                        .unwrap_or_else(|| panic!("serde_json lacks key {decoded:?} in {doc}"));

                    let got: serde_json::Value =
                        serde_json::from_slice(raw_val).expect("value slice is valid JSON");
                    assert_eq!(&got, want, "key {decoded:?} in {doc}");

                    // And looking the raw key back up must find that value
                    // in all three tiers.
                    let raw_key_str = std::str::from_utf8(raw_key).expect("utf8");
                    for (name, got) in [
                        ("tier1", Tier1::object_get(src, raw_key_str)),
                        ("tier2", Tier2::object_get(src, raw_key_str)),
                        ("tier3", Tier3::object_get(src, raw_key_str)),
                    ] {
                        assert_eq!(
                            got,
                            Some(raw_val),
                            "{name} object_get({raw_key_str:?}) in {doc}"
                        );
                    }
                }
            }
            serde_json::Value::Array(items) => {
                assert_eq!(Tier1::array_count(src), items.len(), "on {doc}");
                assert_eq!(Tier2::array_count(src), items.len(), "on {doc}");
                assert_eq!(Tier3::array_count(src), items.len(), "on {doc}");

                for (i, want) in items.iter().enumerate() {
                    let Some(got) = Tier1::array_get(src, i) else {
                        panic!("tier1 missed index {i} in {doc}");
                    };
                    let got: serde_json::Value =
                        serde_json::from_slice(got).expect("slice is valid JSON");
                    assert_eq!(&got, want, "index {i} in {doc}");
                }
            }
            _ => {}
        }
    }
}

// ---------------------------------------------------------------------
// Where the contract does NOT hold
// ---------------------------------------------------------------------

/// Tier 0's container functions are stubs. A consumer written against the
/// "common interface" gets silently wrong answers, with no way to tell from
/// the return type.
#[test]
fn tier0_stubs_out_the_container_contract() {
    // These are associated consts, so the assertions are compile-time
    // facts; that is the point -- the contract is not uniform and the type
    // system now says so.
    const _: () = assert!(!Tier0::HANDLES_CONTAINERS);
    const _: () = assert!(Tier1::HANDLES_CONTAINERS);
    const _: () = assert!(Tier2::HANDLES_CONTAINERS);
    const _: () = assert!(Tier3::HANDLES_CONTAINERS);

    let obj = br#"{"a":1,"b":2}"#;
    let arr = b"[1,2,3]";

    assert_eq!(Tier0::object_count(obj), 0);
    assert_eq!(Tier1::object_count(obj), 2);

    assert_eq!(Tier0::array_count(arr), 0);
    assert_eq!(Tier1::array_count(arr), 3);

    assert_eq!(Tier0::object_get(obj, "a"), None);
    assert_eq!(Tier1::object_get(obj, "a"), Some(b"1".as_slice()));

    // Worst of the lot: `skip_value` makes no progress, so a caller that
    // loops on it hangs.
    assert_eq!(Tier0::skip_value(obj, 0), 0);
    assert!(Tier1::skip_value(obj, 0) > 0);
}

/// `json_validate` is not a validator in any tier.
///
/// Tier 1 and tier 3 share an implementation; tier 2 adds a bracket-balance
/// check on top, which catches *some* of what tier 1 misses but not most of
/// it. None of them is RFC 8259.
#[test]
fn validate_differs_between_tiers_and_from_rfc8259() {
    // Truncated containers: tier 2's bracket check catches these, tiers 1
    // and 3 do not.
    for src in [&b"{"[..], b"[", b"[1,2", br#"{"a":1"#] {
        assert!(
            Tier1::validate(src),
            "tier1 accepts {:?}",
            String::from_utf8_lossy(src)
        );
        assert!(
            Tier3::validate(src),
            "tier3 accepts {:?}",
            String::from_utf8_lossy(src)
        );
        assert!(
            !Tier2::validate(src),
            "tier2's bracket check should reject {:?}",
            String::from_utf8_lossy(src)
        );
        assert!(serde_json::from_slice::<serde_json::Value>(src).is_err());
    }

    // Bad literals and stray commas slip past all three.
    for src in [&b"txxx"[..], b"[1,]", b"[,]"] {
        for (name, ok) in [
            ("tier1", Tier1::validate(src)),
            ("tier2", Tier2::validate(src)),
            ("tier3", Tier3::validate(src)),
        ] {
            assert!(
                ok,
                "{name} unexpectedly rejected {:?}",
                String::from_utf8_lossy(src)
            );
        }
        assert!(serde_json::from_slice::<serde_json::Value>(src).is_err());
    }
}

/// Quantify it: how often does each tier's `validate` agree with a real
/// JSON parser?
#[test]
fn validate_accuracy_against_serde_json() {
    let mut rng = corpus::Rng::new(0xACC0);
    let mut agree = [0usize; 4];
    let mut total = 0usize;

    for _ in 0..20_000 {
        // Mutate valid documents so the corpus is mostly-near-valid, which
        // is the interesting regime.
        let mut src = corpus::random_value(&mut rng, 3).into_bytes();
        if !src.is_empty() && rng.below(2) == 0 {
            let i = rng.below(src.len() as u64) as usize;
            match rng.below(3) {
                0 => {
                    src.remove(i);
                }
                1 => src.truncate(i),
                _ => {
                    if let Some(b) = src.get_mut(i) {
                        *b = *rng.pick(br#"{}[]",:0a"#.as_slice()).unwrap_or(&b'x');
                    }
                }
            }
        }

        let truth = serde_json::from_slice::<serde_json::Value>(&src).is_ok();
        total += 1;
        for (i, got) in [
            Tier0::validate(&src),
            Tier1::validate(&src),
            Tier2::validate(&src),
            Tier3::validate(&src),
        ]
        .into_iter()
        .enumerate()
        {
            if got == truth {
                if let Some(a) = agree.get_mut(i) {
                    *a += 1;
                }
            }
        }
    }

    for (i, t) in Tier::ALL.iter().enumerate() {
        let pct = 100.0 * agree.get(i).copied().unwrap_or(0) as f64 / total as f64;
        println!("  {:14} agrees with serde_json on {pct:5.1}% of {total}", t.label());
    }

    // Tier 0 always says false, so it is right only on the invalid ones.
    // No tier should be *worse* than that floor.
    let floor = agree.first().copied().unwrap_or(0);
    for (i, t) in Tier::ALL.iter().enumerate().skip(1) {
        assert!(
            agree.get(i).copied().unwrap_or(0) >= floor,
            "{} is less accurate than tier 0's constant false",
            t.label()
        );
    }
}

// ---------------------------------------------------------------------
// Robustness
// ---------------------------------------------------------------------

#[test]
fn no_tier_panics_on_arbitrary_bytes() {
    let mut rng = corpus::Rng::new(0xF0_2200);
    let mut buf = Vec::with_capacity(256);

    for _ in 0..20_000 {
        buf.clear();
        for _ in 0..rng.below(256) {
            buf.push((rng.next_u64() & 0xFF) as u8);
        }
        exercise::<Tier0>(&buf);
        exercise::<Tier1>(&buf);
        exercise::<Tier2>(&buf);
        exercise::<Tier3>(&buf);
    }
}

#[test]
fn no_tier_panics_on_truncated_documents() {
    let doc = corpus::records(30, 9);
    for n in 0..doc.len() {
        let src = doc.as_bytes().get(..n).unwrap_or_default();
        exercise::<Tier0>(src);
        exercise::<Tier1>(src);
        exercise::<Tier2>(src);
        exercise::<Tier3>(src);
    }
}

#[test]
fn no_tier_panics_on_structural_soup() {
    for src in [
        b"[".repeat(2_000),
        b"]".repeat(2_000),
        b"{".repeat(2_000),
        b"}".repeat(2_000),
        b"\"".repeat(2_000),
        b"\\".repeat(2_000),
        b",".repeat(2_000),
        b":".repeat(2_000),
        [b"[".repeat(1_000), b"]".repeat(1_000)].concat(),
        [br#"{"a":"#.repeat(1_000).to_vec(), b"1".to_vec()].concat(),
    ] {
        exercise::<Tier0>(&src);
        exercise::<Tier1>(&src);
        exercise::<Tier2>(&src);
        exercise::<Tier3>(&src);
    }
}

/// Touch every method of a tier.
fn exercise<T: JsonTier>(src: &[u8]) {
    let _ = T::parse_string(src);
    let _ = T::parse_number(src);
    let _ = T::parse_bool(src);
    let _ = T::validate_string(src);
    let _ = T::detect_type(src);
    let _ = T::skip_value(src, 0);
    let _ = T::object_get(src, "a");
    let _ = T::object_get(src, "");
    let _ = T::object_count(src);
    let _ = T::object_keys(src);
    let _ = T::object_key_list(src);
    let _ = T::array_get(src, 0);
    let _ = T::array_get(src, usize::MAX);
    let _ = T::array_count(src);
    let _ = T::validate(src);
    let _ = T::parse_value_at(src, 0);
}

#[test]
fn detect_type_never_lies_about_valid_json() {
    let mut rng = corpus::Rng::new(0xD37EC7);
    for _ in 0..10_000 {
        let doc = corpus::random_value(&mut rng, 3);
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&doc) else {
            continue;
        };
        let want = match v {
            serde_json::Value::Null => TypeName::Null,
            serde_json::Value::Bool(_) => TypeName::Bool,
            serde_json::Value::Number(_) => TypeName::Number,
            serde_json::Value::String(_) => TypeName::String,
            serde_json::Value::Array(_) => TypeName::Array,
            serde_json::Value::Object(_) => TypeName::Object,
        };
        assert_eq!(Tier1::detect_type(doc.as_bytes()), want, "on {doc}");
    }
}
