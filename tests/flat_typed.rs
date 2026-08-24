//! Typed (schema-driven) jsonflat layout.
//!
//! The safety-critical part is that the two layouts cannot be confused: a
//! dynamic buffer read as typed, or a buffer written for a different
//! schema, must be a clean error and never a silent misread. That is the
//! whole argument for choosing the layout with a type instead of a Cargo
//! feature, so it gets the most tests.

use vela_json::corpus;
use vela_json::flat::typed::{FlatSchema, TypedView, TypedWriter};
use vela_json::flat::{self, Error};
use vela_json::flat_struct;
use vela_json::strict::StrictParser;

flat_struct! {
    /// The records corpus row.
    pub struct Record : RecordFields {
        id: u64,
        age: u32,
        active: bool,
        score: i64,
        name: str,
        city: str,
        tags: [str],
    }
}

// Same fields, one renamed — must not be readable as `Record`.
flat_struct! {
    pub struct RecordRenamed : RecordRenamedFields {
        id: u64,
        age: u32,
        active: bool,
        score: i64,
        nickname: str,
        city: str,
        tags: [str],
    }
}

// Same names, one retyped.
flat_struct! {
    pub struct RecordRetyped : RecordRetypedFields {
        id: u64,
        age: u32,
        active: bool,
        score: i64,
        name: [str],
        city: str,
        tags: [str],
    }
}

// Fewer fields.
flat_struct! {
    pub struct Short : ShortFields {
        id: u64,
        name: str,
    }
}

fn sample() -> Vec<u8> {
    let mut w = TypedWriter::<Record>::new();
    w.record()
        .u64(1)
        .u32(30)
        .bool(true)
        .i64(-5)
        .str("alpha")
        .str("london")
        .str_list(["x", "y", "z"]);
    w.record()
        .u64(2)
        .u32(0)
        .bool(false)
        .i64(i64::MIN)
        .str("")
        .str("paris")
        .str_list::<[&str; 0]>([]);
    w.finish()
}

#[test]
fn roundtrip_all_field_kinds() {
    let buf = sample();
    let v = TypedView::<Record>::new(&buf).expect("valid");
    assert_eq!(v.len(), 2);

    assert_eq!(v.id(0), Some(1));
    assert_eq!(v.age(0), Some(30));
    assert_eq!(v.active(0), Some(true));
    assert_eq!(v.score(0), Some(-5));
    assert_eq!(v.name(0), Some("alpha"));
    assert_eq!(v.city(0), Some("london"));
    assert_eq!(v.tags(0).len(), 3);
    assert_eq!(v.tags(0).get(0), Some("x"));
    assert_eq!(v.tags(0).get(2), Some("z"));
    assert_eq!(v.tags(0).get(3), None);
    assert_eq!(v.tags(0).into_iter().collect::<Vec<_>>(), ["x", "y", "z"]);

    assert_eq!(v.id(1), Some(2));
    assert_eq!(v.active(1), Some(false));
    assert_eq!(v.score(1), Some(i64::MIN));
    assert_eq!(v.name(1), Some(""));
    assert_eq!(v.city(1), Some("paris"));
    assert!(v.tags(1).is_empty());

    // Out of range is None, not a panic.
    assert_eq!(v.id(2), None);
    assert_eq!(v.name(99), None);
    assert!(v.tags(99).is_empty());
}

#[test]
fn strings_borrow_from_the_buffer() {
    let buf = sample();
    let v = TypedView::<Record>::new(&buf).expect("valid");
    let s = v.name(0).expect("name");
    // The &str must point inside `buf`, not at a copy.
    let lo = buf.as_ptr() as usize;
    let hi = lo + buf.len();
    let p = s.as_ptr() as usize;
    assert!((lo..hi).contains(&p), "string was copied out of the buffer");
}

#[test]
fn f64_and_i32_fields() {
    flat_struct! {
        pub struct Nums : NumsFields {
            f: f64,
            i: i32,
        }
    }
    let mut w = TypedWriter::<Nums>::new();
    w.record().f64(3.5).i32(-7);
    w.record().f64(f64::MIN).i32(i32::MAX);
    let buf = w.finish();
    let v = TypedView::<Nums>::new(&buf).expect("valid");
    assert_eq!(v.f(0), Some(3.5));
    assert_eq!(v.i(0), Some(-7));
    assert_eq!(v.f(1), Some(f64::MIN));
    assert_eq!(v.i(1), Some(i32::MAX));
}

#[test]
fn empty_buffer_is_valid() {
    let buf = TypedWriter::<Record>::new().finish();
    let v = TypedView::<Record>::new(&buf).expect("valid");
    assert_eq!(v.len(), 0);
    assert!(v.is_empty());
    assert_eq!(v.id(0), None);
}

// ---------------------------------------------------------------------
// Layout and schema confusion — the whole point of the design
// ---------------------------------------------------------------------

#[test]
fn dynamic_buffer_is_rejected_by_typed_reader() {
    let mut p = StrictParser::new();
    let doc = p.parse(br#"{"a":1}"#).expect("valid");
    let dynamic = flat::encode(doc).expect("encode");

    assert_eq!(
        TypedView::<Record>::new(&dynamic).unwrap_err(),
        Error::LayoutMismatch,
        "a dynamic buffer must not be readable through a schema"
    );
}

#[test]
fn typed_buffer_is_rejected_by_dynamic_reader() {
    let typed = sample();
    assert_eq!(
        flat::View::new(&typed).unwrap_err(),
        Error::LayoutMismatch,
        "a typed buffer must not be readable as self-describing"
    );
}

#[test]
fn renaming_a_field_changes_the_schema_id() {
    assert_ne!(Record::SCHEMA_ID, RecordRenamed::SCHEMA_ID);
    let buf = sample();
    assert_eq!(
        TypedView::<RecordRenamed>::new(&buf).unwrap_err(),
        Error::SchemaMismatch
    );
}

#[test]
fn retyping_a_field_changes_the_schema_id() {
    assert_ne!(Record::SCHEMA_ID, RecordRetyped::SCHEMA_ID);
    let buf = sample();
    assert_eq!(
        TypedView::<RecordRetyped>::new(&buf).unwrap_err(),
        Error::SchemaMismatch
    );
}

#[test]
fn changing_the_field_count_is_rejected() {
    assert_ne!(Record::SCHEMA_ID, Short::SCHEMA_ID);
    let buf = sample();
    assert_eq!(
        TypedView::<Short>::new(&buf).unwrap_err(),
        Error::SchemaMismatch
    );
}

#[test]
fn reordering_fields_changes_the_schema_id() {
    flat_struct! {
        pub struct A : AFields { x: u64, y: str }
    }
    flat_struct! {
        pub struct B : BFields { y: str, x: u64 }
    }
    assert_ne!(A::SCHEMA_ID, B::SCHEMA_ID);
}

/// The hash must separate field names, or `("ab","c")` and `("a","bc")`
/// would collide.
#[test]
fn adjacent_names_do_not_collide() {
    flat_struct! {
        pub struct P : PFields { ab: u64, c: u64 }
    }
    flat_struct! {
        pub struct Q : QFields { a: u64, bc: u64 }
    }
    assert_ne!(P::SCHEMA_ID, Q::SCHEMA_ID);
}

#[test]
fn header_corruption_is_rejected() {
    let good = sample();

    assert_eq!(TypedView::<Record>::new(&[]).unwrap_err(), Error::TooShort);

    let mut bad = good.clone();
    bad[0] = b'X';
    assert_eq!(TypedView::<Record>::new(&bad).unwrap_err(), Error::BadMagic);

    let mut bad = good.clone();
    bad[4] = 99;
    assert!(matches!(
        TypedView::<Record>::new(&bad).unwrap_err(),
        Error::UnsupportedVersion(_)
    ));

    let mut bad = good.clone();
    bad.pop();
    assert_eq!(
        TypedView::<Record>::new(&bad).unwrap_err(),
        Error::LengthMismatch
    );
}

#[test]
fn every_single_byte_mutation_is_safe() {
    // The buffer is the data structure, so a flipped byte is an attack.
    // Wrong answers are fine; crashes are not.
    let good = sample();
    for i in 0..good.len() {
        for delta in [1u8, 0x7F, 0xFF] {
            let mut buf = good.clone();
            buf[i] = buf[i].wrapping_add(delta);
            let Ok(v) = TypedView::<Record>::new(&buf) else {
                continue;
            };
            for r in 0..v.len().min(64) {
                let _ = v.id(r);
                let _ = v.age(r);
                let _ = v.active(r);
                let _ = v.score(r);
                let _ = v.name(r);
                let _ = v.city(r);
                let list = v.tags(r);
                let _ = list.len();
                for k in 0..list.len().min(64) {
                    let _ = list.get(k);
                }
            }
        }
    }
}

#[test]
fn random_buffers_are_safe() {
    let mut rng = corpus::Rng::new(0x7_9DED);
    let good = sample();
    for _ in 0..20_000 {
        let mut buf = good.clone();
        for _ in 0..rng.below(6) {
            let i = 4 + rng.below((buf.len() - 4) as u64) as usize;
            buf[i] = (rng.next_u64() & 0xFF) as u8;
        }
        let Ok(v) = TypedView::<Record>::new(&buf) else {
            continue;
        };
        for r in 0..v.len().min(32) {
            let _ = v.name(r);
            let _ = v.tags(r).into_iter().count();
        }
    }
}

// ---------------------------------------------------------------------
// Interning
// ---------------------------------------------------------------------

#[test]
fn interning_deduplicates_repeated_strings() {
    let build = |intern: bool| {
        let mut w = TypedWriter::<Short>::new().intern(intern);
        for i in 0..500u64 {
            w.record().u64(i).str("the-same-string-every-time");
        }
        w.finish()
    };
    let with = build(true);
    let without = build(false);

    // The fixed record area is 500 * 2 slots * 8 B = 8000 bytes either way,
    // so compare the saving against the blob rather than the total: 499
    // duplicate copies of a 26-byte string should disappear.
    const S: usize = "the-same-string-every-time".len();
    let saved = without.len() - with.len();
    assert_eq!(
        saved,
        499 * S,
        "expected to drop 499 duplicate copies of a {S}-byte string \
         (total {} -> {})",
        without.len(),
        with.len()
    );
    // Both must still read correctly.
    for buf in [&with, &without] {
        let v = TypedView::<Short>::new(buf).expect("valid");
        assert_eq!(v.len(), 500);
        assert_eq!(v.name(499), Some("the-same-string-every-time"));
        assert_eq!(v.id(499), Some(499));
    }
}

// ---------------------------------------------------------------------
// Against the real corpus
// ---------------------------------------------------------------------

/// Build the typed buffer from the same corpus the other benchmarks use and
/// check every value against `serde_json`.
#[test]
fn records_corpus_roundtrips() {
    let json = corpus::records(2_000, 41);
    let parsed: serde_json::Value = serde_json::from_str(&json).expect("valid");
    let rows = parsed.as_array().expect("array");

    let mut w = TypedWriter::<Record>::new();
    for r in rows {
        let tags: Vec<&str> = r["tags"]
            .as_array()
            .map(|a| a.iter().filter_map(|t| t.as_str()).collect())
            .unwrap_or_default();
        w.record()
            .u64(r["id"].as_u64().unwrap_or(0))
            .u32(r["age"].as_u64().unwrap_or(0) as u32)
            .bool(r["active"].as_bool().unwrap_or(false))
            .i64(r["score"].as_i64().unwrap_or(0))
            .str(r["name"].as_str().unwrap_or(""))
            .str(r["city"].as_str().unwrap_or(""))
            .str_list(tags);
    }
    let buf = w.finish();

    let v = TypedView::<Record>::new(&buf).expect("valid");
    assert_eq!(v.len(), rows.len());
    for (i, r) in rows.iter().enumerate() {
        assert_eq!(v.id(i), r["id"].as_u64(), "id {i}");
        assert_eq!(v.active(i), r["active"].as_bool(), "active {i}");
        assert_eq!(v.score(i), r["score"].as_i64(), "score {i}");
        assert_eq!(v.name(i), r["name"].as_str(), "name {i}");
        assert_eq!(v.city(i), r["city"].as_str(), "city {i}");
        let want: Vec<&str> = r["tags"]
            .as_array()
            .map(|a| a.iter().filter_map(|t| t.as_str()).collect())
            .unwrap_or_default();
        assert_eq!(v.tags(i).into_iter().collect::<Vec<_>>(), want, "tags {i}");
    }
}

/// The size claim: typed must be materially smaller than dynamic, because
/// it does not store key strings.
#[test]
fn typed_is_smaller_than_dynamic() {
    let json = corpus::records(5_000, 41);

    let mut p = StrictParser::new();
    let dynamic = flat::encode(p.parse(json.as_bytes()).expect("valid")).expect("encode");

    let parsed: serde_json::Value = serde_json::from_str(&json).expect("valid");
    let mut w = TypedWriter::<Record>::new();
    for r in parsed.as_array().into_iter().flatten() {
        let tags: Vec<&str> = r["tags"]
            .as_array()
            .map(|a| a.iter().filter_map(|t| t.as_str()).collect())
            .unwrap_or_default();
        w.record()
            .u64(r["id"].as_u64().unwrap_or(0))
            .u32(r["age"].as_u64().unwrap_or(0) as u32)
            .bool(r["active"].as_bool().unwrap_or(false))
            .i64(r["score"].as_i64().unwrap_or(0))
            .str(r["name"].as_str().unwrap_or(""))
            .str(r["city"].as_str().unwrap_or(""))
            .str_list(tags);
    }
    let typed = w.finish();

    println!(
        "  json {} | dynamic {} ({:.2}x) | typed {} ({:.2}x)",
        json.len(),
        dynamic.len(),
        dynamic.len() as f64 / json.len() as f64,
        typed.len(),
        typed.len() as f64 / json.len() as f64
    );
    assert!(
        typed.len() < dynamic.len(),
        "typed {} should beat dynamic {}",
        typed.len(),
        dynamic.len()
    );
}
