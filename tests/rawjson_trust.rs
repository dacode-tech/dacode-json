//! The `RawJson` trust boundary, tested from outside the crate.
//!
//! The capture optimisation once routed its trusted handoff through
//! `Visitor::visit_borrowed_str` — reachable with unvalidated text,
//! because `forward_to_deserialize_any!` maps `deserialize_newtype_struct`
//! to `deserialize_any` to `visit_borrowed_str` with no `TOKEN` match
//! required. `HashMap<RawJson, i64>` from `{"é": 1}` panicked in debug and
//! silently captured `RawJson("é")` in release; a `RawJson` field over a
//! flat `Str` slot did the same. The trusted channel is now the
//! crate-private `raw::TrustedStr`, and every generic route lands in the
//! validating entries instead.
//!
//! Each test here pins one enumerated route: it must error cleanly on
//! non-JSON text, in debug and release alike, and still capture when the
//! text *is* JSON — validating, not blanket-rejecting.

use dacode_json::RawJson;
use serde::Deserialize;
use std::collections::HashMap;

#[derive(Deserialize)]
struct Event {
    #[allow(dead_code)]
    name: String,
    #[allow(dead_code)]
    args: RawJson,
}

/// Map keys, on each deserializer that hands them to serde as a string.
///
/// The direct path hands *simple* keys as bytes (`visit_borrowed_bytes`,
/// which errors for `RawJson` regardless) and non-simple keys as a
/// borrowed str; the pool and stream paths always hand a borrowed str.
/// All of them must validate what they hand.
#[test]
fn map_keys_reach_the_validating_path() {
    // A fine string and terrible JSON: the key text must be validated,
    // never trusted.
    for src in [r#"{"é": 1}"#, r#"{"not json": 1}"#] {
        assert!(
            dacode_json::from_str::<HashMap<RawJson, i64>>(src).is_err(),
            "direct keys must reject {src:?}"
        );
        let mut p = dacode_json::Parser::new();
        let doc = p.parse(src.as_bytes()).expect("strict parses it");
        assert!(
            dacode_json::de::from_doc::<HashMap<RawJson, i64>>(doc).is_err(),
            "pool keys must reject {src:?}"
        );
        let mut idx = dacode_json::stream::Index::default();
        assert!(
            dacode_json::stream::from_slice_with::<HashMap<RawJson, i64>>(&mut idx, src.as_bytes())
                .is_err(),
            "stream keys must reject {src:?}"
        );
    }

    // A key that *is* JSON still captures on the borrowed-str paths, so
    // the fix validates rather than blanket-rejects. The direct path hands
    // this simple key as *bytes* (`visit_borrowed_bytes`) and errors for
    // `RawJson` — its long-standing behaviour, unchanged by the trust fix.
    assert!(
        dacode_json::from_str::<HashMap<RawJson, i64>>(r#"{"42": 1}"#).is_err(),
        "direct simple keys go the bytes route and error"
    );
    let mut p = dacode_json::Parser::new();
    let doc = p.parse(br#"{"42": 1}"#).expect("strict parses it");
    let m: HashMap<RawJson, i64> = dacode_json::de::from_doc(doc).expect("pool key is JSON");
    assert_eq!(m.get(&"42".parse::<RawJson>().unwrap()), Some(&1));
    let mut idx = dacode_json::stream::Index::default();
    let m: HashMap<RawJson, i64> = dacode_json::stream::from_slice_with(&mut idx, br#"{"42": 1}"#)
        .expect("stream key is JSON");
    assert_eq!(m.get(&"42".parse::<RawJson>().unwrap()), Some(&1));
}

/// Flat columnar string slots hold arbitrary text; a `RawJson` field over
/// one must validate it. The `StrList` elements ride the same
/// `BorrowedStr` deserializer the row keys do, so they are covered by the
/// same shape.
#[test]
fn flat_string_slots_reach_the_validating_path() {
    use dacode_json::flat::typed::{de, TypedView, TypedWriter};

    dacode_json::flat_struct! {
        pub struct Row : RowFields { name: str, tags: [str], n: u32 }
    }

    #[derive(Deserialize)]
    struct Out {
        name: RawJson,
        tags: Vec<RawJson>,
        #[allow(dead_code)]
        n: u32,
    }

    let read = |buf: &[u8]| -> Result<Vec<Out>, _> {
        let v = TypedView::<Row>::new(buf).expect("view");
        de::from_all(&v)
    };

    // A `Str` slot that is not JSON: clean error, never a capture.
    let mut w = TypedWriter::<Row>::new();
    w.record()
        .str("hello, not json")
        .str_list(["a", "b"])
        .u32(1);
    assert!(
        read(&w.finish()).is_err(),
        "flat Str slot must reject non-JSON text"
    );

    // A `StrList` element that is not JSON: same deserializer, same rule.
    let mut w = TypedWriter::<Row>::new();
    w.record().str("42").str_list(["ok", "not json"]).u32(1);
    assert!(
        read(&w.finish()).is_err(),
        "flat StrList element must reject non-JSON text"
    );

    // Slots that *are* JSON still capture — validating, not rejecting.
    let mut w = TypedWriter::<Row>::new();
    w.record().str("42").str_list(["1", "true"]).u32(1);
    let rows = read(&w.finish()).expect("valid slots");
    assert_eq!(rows[0].name.get(), "42");
    assert_eq!(rows[0].tags[1].get(), "true");
}

/// A foreign deserializer has no trusted channel: `RawJson` from
/// `serde_json` is a clean error, exactly as it was before the capture
/// optimisation existed.
#[test]
fn foreign_deserializer_has_no_trusted_channel() {
    assert!(serde_json::from_str::<RawJson>("42").is_err());
    assert!(serde_json::from_str::<RawJson>(r#""hello""#).is_err());
    assert!(serde_json::from_str::<Event>(r#"{"name":"x","args":1}"#).is_err());
}
