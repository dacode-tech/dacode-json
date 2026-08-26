//! `jsonflat` correctness: round-trip fidelity, and refusal to be
//! confused by a corrupt buffer.
//!
//! The second half matters more than the first. A zero-copy format is a
//! parser for attacker-controlled bytes wearing a disguise — the buffer
//! *is* the data structure — so every accessor is fuzzed against random
//! and mutated input and must return `None`, never panic.

use dacodec::flat::{self, Builder, Error, View, HEADER, MAGIC};
use dacodec::strict::StrictParser;
use dacodec::{corpus, Type};

fn encode(json: &str) -> Vec<u8> {
    let mut p = StrictParser::new();
    let doc = p.parse(json.as_bytes()).expect("valid json");
    flat::encode(doc).expect("encode")
}

/// Compare a `jsonflat` view against `serde_json`'s tree.
fn same(r: flat::Ref<'_>, j: &serde_json::Value) -> Result<(), String> {
    match (r.typ(), j) {
        (Type::Null, serde_json::Value::Null) => Ok(()),
        (Type::Bool, serde_json::Value::Bool(b)) => {
            if r.as_bool() == Some(*b) {
                Ok(())
            } else {
                Err(format!("bool {:?} != {b}", r.as_bool()))
            }
        }
        (Type::Number | Type::Float, serde_json::Value::Number(n)) => {
            match (r.as_f64(), n.as_f64()) {
                // 1 ULP of slack: serde_json's default float parser is not
                // correctly rounded, ours is. See
                // `serde_de.rs::float_precision_beats_serde_json_default`.
                (Some(a), Some(b)) => {
                    let ulps = (a.to_bits() as i64).wrapping_sub(b.to_bits() as i64).unsigned_abs();
                    if ulps <= 1 {
                        Ok(())
                    } else {
                        Err(format!("number {a:?} != {b:?} ({ulps} ulps)"))
                    }
                }
                (a, b) => Err(format!("number {a:?} != {b:?}")),
            }
        }
        (Type::String, serde_json::Value::String(s)) => {
            if r.as_str() == Some(s.as_str()) {
                Ok(())
            } else {
                Err(format!("string {:?} != {s:?}", r.as_str()))
            }
        }
        (Type::Array, serde_json::Value::Array(items)) => {
            if r.len() != items.len() {
                return Err(format!("array len {} != {}", r.len(), items.len()));
            }
            // Both by iterator and by index, since `at` is a separate path.
            for (i, ji) in items.iter().enumerate() {
                let e = r.at(i).ok_or_else(|| format!("missing element {i}"))?;
                same(e, ji)?;
            }
            for (e, ji) in r.elements().zip(items.iter()) {
                same(e, ji)?;
            }
            Ok(())
        }
        (Type::Object, serde_json::Value::Object(map)) => {
            if r.len() != map.len() {
                return Err(format!("object len {} != {}", r.len(), map.len()));
            }
            for (k, jv) in map {
                let v = r.get(k).ok_or_else(|| format!("missing key {k:?}"))?;
                same(v, jv)?;
            }
            // And via iteration.
            for (k, v) in r.entries() {
                let jv = map.get(k).ok_or_else(|| format!("extra key {k:?}"))?;
                same(v, jv)?;
            }
            Ok(())
        }
        (t, j) => Err(format!("type mismatch {t:?} vs {j}")),
    }
}

#[track_caller]
fn roundtrip(json: &str) {
    let buf = encode(json);
    let view = View::new(&buf).expect("view");
    view.validate_deep().expect("deep validation");
    let expect: serde_json::Value = serde_json::from_str(json).expect("serde_json");
    if let Err(why) = same(view.root(), &expect) {
        panic!("{why}\n{json}");
    }
}

#[test]
fn scalars_roundtrip() {
    for j in [
        "null", "true", "false", "0", "-1", "42", "3.5", "-0.125", "1e10", r#""""#, r#""abc""#,
    ] {
        roundtrip(j);
    }
}

#[test]
fn containers_roundtrip() {
    for j in [
        "[]",
        "{}",
        "[1,2,3]",
        r#"{"a":1}"#,
        r#"{"a":1,"b":[1,2],"c":{"d":null}}"#,
        "[[[[1]]]]",
        r#"[{"a":[{"b":{}}]}]"#,
        r#"{"z":1,"a":2,"m":3}"#,
    ] {
        roundtrip(j);
    }
}

#[test]
fn escapes_are_decoded_at_build_time() {
    let buf = encode(r#"{"k\u0065y":"a\nb\ud83d\ude00"}"#);
    let view = View::new(&buf).expect("view");
    // Key was written unescaped, so lookup uses the decoded form.
    let v = view.root().get("key").expect("decoded key");
    assert_eq!(v.as_str(), Some("a\nb😀"));
    // And the escaped form is *not* a key any more.
    assert!(view.root().get(r"k\u0065y").is_none());
}

#[test]
fn integers_inline_and_tabled() {
    // i32 range goes inline; beyond it uses the numbers table.
    let json = format!(
        "[{},{},{},{},{}]",
        i32::MIN,
        i32::MAX,
        i64::from(i32::MAX) + 1,
        i64::MIN,
        i64::MAX
    );
    roundtrip(&json);

    let buf = encode(&json);
    let view = View::new(&buf).expect("view");
    let r = view.root();
    assert_eq!(r.at(0).and_then(|v| v.as_i64()), Some(i64::from(i32::MIN)));
    assert_eq!(r.at(2).and_then(|v| v.as_i64()), Some(i64::from(i32::MAX) + 1));
    assert_eq!(r.at(3).and_then(|v| v.as_i64()), Some(i64::MIN));
    assert_eq!(r.at(4).and_then(|v| v.as_i64()), Some(i64::MAX));
}

#[test]
fn generated_corpora_roundtrip() {
    for (name, json) in corpus::suite(200_000, 55) {
        let buf = encode(&json);
        let view = View::new(&buf).expect("view");
        view.validate_deep().unwrap_or_else(|e| panic!("{name}: {e}"));
        let expect: serde_json::Value = serde_json::from_str(&json).expect("valid");
        if let Err(why) = same(view.root(), &expect) {
            panic!("{name}: {why}");
        }
    }
}

#[test]
fn random_documents_roundtrip() {
    let mut rng = corpus::Rng::new(0xF1A7);
    for _ in 0..5_000 {
        roundtrip(&corpus::random_value(&mut rng, 4));
    }
}

#[test]
fn unsorted_mode_still_finds_keys() {
    let json = r#"{"z":1,"a":2,"m":3}"#;
    let mut p = StrictParser::new();
    let doc = p.parse(json.as_bytes()).expect("valid");
    let buf = Builder::new().sort_keys(false).build(doc).expect("build");
    let view = View::new(&buf).expect("view");
    assert!(!view.keys_sorted());

    // Document order preserved.
    let keys: Vec<&str> = view.root().entries().map(|(k, _)| k).collect();
    assert_eq!(keys, vec!["z", "a", "m"]);
    // Linear lookup still works.
    assert_eq!(view.root().get("m").and_then(|v| v.as_i64()), Some(3));
    assert!(view.root().get("nope").is_none());
}

#[test]
fn sorted_mode_orders_keys() {
    let buf = encode(r#"{"z":1,"a":2,"m":3}"#);
    let view = View::new(&buf).expect("view");
    assert!(view.keys_sorted());
    let keys: Vec<&str> = view.root().entries().map(|(k, _)| k).collect();
    assert_eq!(keys, vec!["a", "m", "z"]);
    for (k, want) in [("a", 2), ("m", 3), ("z", 1)] {
        assert_eq!(view.root().get(k).and_then(|v| v.as_i64()), Some(want));
    }
    assert!(view.root().get("b").is_none());
    assert!(view.root().get("").is_none());
    assert!(view.root().get("zz").is_none());
}

#[test]
fn binary_search_finds_every_key_in_a_wide_object() {
    let mut json = String::from("{");
    for i in 0..500 {
        if i > 0 {
            json.push(',');
        }
        json.push_str(&format!(r#""field_{i:04}":{i}"#));
    }
    json.push('}');

    let buf = encode(&json);
    let view = View::new(&buf).expect("view");
    for i in 0..500i64 {
        let k = format!("field_{i:04}");
        assert_eq!(
            view.root().get(&k).and_then(|v| v.as_i64()),
            Some(i),
            "key {k}"
        );
    }
    assert!(view.root().get("field_9999").is_none());
}

// ---------------------------------------------------------------------
// Hostile input
// ---------------------------------------------------------------------

#[test]
fn header_validation_rejects_junk() {
    assert_eq!(View::new(&[]).unwrap_err(), Error::TooShort);
    assert_eq!(View::new(&[0u8; 8]).unwrap_err(), Error::TooShort);
    assert_eq!(View::new(&[0u8; HEADER]).unwrap_err(), Error::BadMagic);

    let mut buf = encode(r#"{"a":1}"#);

    let mut bad = buf.clone();
    bad[0] = b'X';
    assert_eq!(View::new(&bad).unwrap_err(), Error::BadMagic);

    let mut bad = buf.clone();
    bad[4] = 99;
    assert!(matches!(
        View::new(&bad).unwrap_err(),
        Error::UnsupportedVersion(_)
    ));

    // Truncation must be caught by the length field.
    buf.pop();
    assert_eq!(View::new(&buf).unwrap_err(), Error::LengthMismatch);
}

#[test]
fn every_single_byte_mutation_is_safe() {
    // The buffer *is* the data structure, so a flipped byte is an attack.
    // Nothing here may panic; wrong answers are acceptable, crashes are not.
    let good = encode(r#"{"a":[1,2,{"b":"x"}],"c":true,"d":1.5,"e":null}"#);

    for i in 0..good.len() {
        for delta in [1u8, 0x7F, 0xFF] {
            let mut buf = good.clone();
            buf[i] = buf[i].wrapping_add(delta);

            let Ok(view) = View::new(&buf) else { continue };
            // Deep validation must also not panic.
            let _ = view.validate_deep();
            exercise(view);
        }
    }
}

#[test]
fn random_buffers_are_safe() {
    let mut rng = corpus::Rng::new(0xBADF1A7);
    let good = encode(r#"{"a":[1,2,3],"b":"str"}"#);

    for _ in 0..20_000 {
        let mut buf = good.clone();
        // Corrupt a handful of bytes, keeping the magic so `View::new`
        // usually succeeds and the accessors get exercised.
        for _ in 0..rng.below(6) {
            let i = 4 + rng.below((buf.len() - 4) as u64) as usize;
            buf[i] = (rng.next_u64() & 0xFF) as u8;
        }
        let Ok(view) = View::new(&buf) else { continue };
        let _ = view.validate_deep();
        exercise(view);
    }

    // Also: buffers that are pure noise with a valid magic.
    for _ in 0..20_000 {
        let len = HEADER + rng.below(200) as usize;
        let mut buf = vec![0u8; len];
        for b in buf.iter_mut() {
            *b = (rng.next_u64() & 0xFF) as u8;
        }
        buf[0..4].copy_from_slice(&MAGIC.to_le_bytes());
        let Ok(view) = View::new(&buf) else { continue };
        let _ = view.validate_deep();
        exercise(view);
    }
}

/// Touch every accessor, recursively, with a depth cap.
fn exercise(view: View<'_>) {
    fn walk(r: flat::Ref<'_>, depth: u32) {
        if depth > 6 {
            return;
        }
        let _ = r.typ();
        let _ = r.is_null();
        let _ = r.as_bool();
        let _ = r.as_i64();
        let _ = r.as_f64();
        let _ = r.as_str();
        let _ = r.as_bytes();
        let _ = r.len();
        let _ = r.get("a");
        let _ = r.get("");
        let _ = r.at(0);
        let _ = r.at(usize::MAX);
        for (i, e) in r.elements().enumerate() {
            if i > 32 {
                break;
            }
            walk(e, depth + 1);
        }
        for (i, (_k, v)) in r.entries().enumerate() {
            if i > 32 {
                break;
            }
            walk(v, depth + 1);
        }
    }
    walk(view.root(), 0);
}

#[test]
fn deep_validation_catches_dangling_references() {
    let mut buf = encode(r#"{"a":[1,2,3]}"#);
    // Header layout: node_count at offset 8. Claim more nodes than exist.
    let n = u32::from_le_bytes([buf[8], buf[9], buf[10], buf[11]]);
    buf[8..12].copy_from_slice(&(n + 1000).to_le_bytes());
    // The length check should catch this first.
    assert!(View::new(&buf).is_err());
}

#[test]
fn size_is_reported_honestly() {
    // Not an assertion about a target, just a guard that the format has
    // not silently ballooned. See docs/ZEROCOPY.md for the real numbers.
    let json = corpus::records(1_000, 3);
    let buf = encode(&json);
    let ratio = buf.len() as f64 / json.len() as f64;
    assert!(
        (0.5..3.0).contains(&ratio),
        "jsonflat is {ratio:.2}x the JSON size, which is outside the expected band"
    );
}
