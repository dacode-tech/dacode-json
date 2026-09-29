//! Fixed-length serde visitors: tuples, tuple structs, tuple enum
//! variants and `[T; N]`.
//!
//! These are the only serde shapes that stop reading a container *early*.
//! A collection (`Vec`, `BTreeMap`, `Value`) calls `next_element_seed`
//! until it returns `None`; a fixed-length visitor calls it exactly `len`
//! times and returns, never asking for another element. So somebody else
//! has to consume the closer, and `serde_json` makes that the
//! `Deserializer`'s job: `next_element_seed` peeks `]`, and `end_seq` — run
//! after `visit_seq` returns — eats it.
//!
//! Both byte-cursor tiers had that inverted: `next_element_seed` ate the
//! `]` and nothing closed the array for a visitor that stopped early, so
//! control returned to the *enclosing* container with the closer still
//! pending and that container consumed it as its own terminator. Nothing
//! caught it because `tests/serde_de.rs` holds the **pool** tier to
//! `serde_json` (and the pool pre-parses, so its framing is already
//! settled), while `tests/stream.rs` holds `direct` and `stream` to
//! `serde_json` only through `JValue`, which is a collection all the way
//! down and never visits a fixed length. This file is the missing column:
//! the same differential against `serde_json`, over the shapes that stop
//! early.
//!
//! Three defects are pinned here, all of them verdict-level, not cosmetic:
//!
//! 1. A fixed-length visitor left its array unclosed, so
//!    `Vec<(u64, u64)>` over `[[1,2],[3,4]]` read *one* pair — the outer
//!    `Vec` took the inner `]` for its own — and the rest of the document
//!    was reported as trailing characters.
//! 2. A **truncated** array parsed clean: `[1,2` deserialized as
//!    `Ok((1, 2))`. The closer was never required, so a document cut short
//!    mid-array was indistinguishable from a complete one. That is the
//!    case a consumer of a line-oriented store most needs to be an error.
//! 3. `stream`'s `Payload::close` bumped the structural index without
//!    moving `vend`, which is what `from_slice` measures the tail from —
//!    so every enum object variant parsed and was then rejected for the
//!    `}` it had just consumed. Found while writing (1)'s tests; it is a
//!    separate defect and gets its own test below.
//!
//! Offsets and message text are *not* asserted: they are
//! implementation-defined and the tiers already differ from each other on
//! malformed input (see `tests/tier_contract.rs`). The verdict — accept or
//! reject — and the value are.

#![cfg(feature = "serde")]

use serde::Deserialize;

/// Both byte-cursor tiers must reach `serde_json`'s verdict, and its value
/// when the document is accepted.
///
/// `serde_json` is the oracle, which is the same arrangement
/// `tests/serde_de.rs` and `tests/stream.rs` use.
#[track_caller]
fn same<T>(json: &str)
where
    T: for<'a> Deserialize<'a> + PartialEq + std::fmt::Debug,
{
    let want = serde_json::from_str::<T>(json).map_err(|e| e.to_string());
    // The tiers have distinct error types, so both reduce to their text:
    // only the verdict and the value are under test here.
    let tiers = [
        (
            "direct",
            dacode_json::from_slice::<T>(json.as_bytes()).map_err(|e| e.to_string()),
        ),
        (
            "stream",
            dacode_json::stream::from_slice::<T>(json.as_bytes()).map_err(|e| e.to_string()),
        ),
    ];
    for (tier, got) in tiers {
        match (&want, &got) {
            (Ok(w), Ok(g)) => assert_eq!(w, g, "{tier} value mismatch on {json}"),
            (Err(_), Err(_)) => {}
            (Ok(w), Err(e)) => panic!("{tier} rejected an accepted doc {json}: {e} (want {w:?})"),
            (Err(e), Ok(g)) => {
                panic!("{tier} accepted a rejected doc {json}: got {g:?}, serde_json said {e}")
            }
        }
    }
}

/// `same`, plus the value pinned literally so the test cannot pass on two
/// engines agreeing about the wrong thing.
#[track_caller]
fn accepted<T>(json: &str, want: T)
where
    T: for<'a> Deserialize<'a> + PartialEq + std::fmt::Debug,
{
    same::<T>(json);
    assert_eq!(
        serde_json::from_str::<T>(json).expect("oracle"),
        want,
        "oracle disagrees with the pinned value on {json}"
    );
}

#[derive(Debug, Deserialize, PartialEq)]
struct Pair(u64, u64);

#[derive(Debug, Deserialize, PartialEq)]
enum Variant {
    Unit,
    Tup(u64, u64),
    New(u64),
    Struct { a: u64 },
}

#[derive(Debug, Deserialize, PartialEq)]
struct TupleFieldThenSibling {
    a: (u64, u64),
    b: u64,
}

// ---------------------------------------------------------------------
// Accepted: every fixed-length shape, on both tiers
// ---------------------------------------------------------------------

#[test]
fn tuples_deserialize() {
    accepted::<(u64, u64)>("[1,2]", (1, 2));
    accepted::<(u64, String)>("[1,\"x\"]", (1, "x".to_owned()));
    accepted::<(u64, u64)>("[ 1 , 2 ]", (1, 2));
}

#[test]
fn fixed_arrays_deserialize() {
    accepted::<[u64; 2]>("[1,2]", [1, 2]);
    accepted::<[u64; 0]>("[]", []);
}

#[test]
fn tuple_structs_deserialize() {
    accepted::<Pair>("[1,2]", Pair(1, 2));
}

#[test]
fn a_fixed_length_visitor_nested_in_a_collection_does_not_steal_its_closer() {
    // Defect 1. Before `end_seq`, the outer `Vec` ended after one pair and
    // `[3,4]]` was reported as trailing characters.
    accepted::<Vec<(u64, u64)>>("[[1,2],[3,4]]", vec![(1, 2), (3, 4)]);
    accepted::<Vec<Vec<(u64, u64)>>>("[[[1,2]],[[3,4]]]", vec![vec![(1, 2)], vec![(3, 4)]]);
}

#[test]
fn a_fixed_length_visitor_does_not_derail_a_following_field() {
    // Defect 1, one level up: the object saw `]` where it expected `,`.
    accepted::<TupleFieldThenSibling>(
        r#"{"a":[1,2],"b":3}"#,
        TupleFieldThenSibling { a: (1, 2), b: 3 },
    );
}

#[test]
fn tuple_enum_variants_deserialize() {
    accepted::<Variant>(r#"{"Tup":[1,2]}"#, Variant::Tup(1, 2));
    accepted::<Vec<Variant>>(r#"[{"Tup":[1,2]}]"#, vec![Variant::Tup(1, 2)]);
}

// ---------------------------------------------------------------------
// Rejected: the closer is required, and so is its exact length
// ---------------------------------------------------------------------

#[test]
fn a_truncated_array_is_an_error_not_a_shorter_value() {
    // Defect 2, and the one that matters most to a consumer reading a
    // line-oriented store: at HEAD both tiers returned `Ok((1, 2))`.
    same::<(u64, u64)>("[1,2");
    same::<Vec<u64>>("[1,2");
    same::<Vec<(u64, u64)>>("[[1,2]");
    assert!(dacode_json::from_slice::<(u64, u64)>(b"[1,2").is_err());
    assert!(dacode_json::stream::from_slice::<(u64, u64)>(b"[1,2").is_err());
}

#[test]
fn a_surplus_element_is_an_error_not_a_silent_truncation() {
    same::<(u64, u64)>("[1,2,3]");
    same::<[u64; 2]>("[1,2,3]");
    same::<Pair>("[1,2,3]");
    same::<[u64; 0]>("[1]");
    assert!(dacode_json::from_slice::<(u64, u64)>(b"[1,2,3]").is_err());
}

#[test]
fn a_short_array_is_an_invalid_length_error() {
    same::<(u64, u64)>("[1]");
    same::<(u64, u64)>("[]");
    same::<Pair>("[1]");
}

#[test]
fn damaged_separators_stay_damaged() {
    same::<(u64, u64)>("[1,2,]");
    same::<(u64, u64)>("[1 2]");
    same::<(u64, u64)>("[,1]");
    same::<Vec<u64>>("[1,2,]");
}

// ---------------------------------------------------------------------
// Defect 3: `stream`'s enum object variants
// ---------------------------------------------------------------------

/// Every payload shape, not just the tuple one.
///
/// `Payload::close` advanced the structural index with `bump` instead of
/// `Cursor::close`, leaving `vend` — the offset `from_slice` measures the
/// trailing-whitespace check from — pointing just past the payload. So the
/// `}` was consumed and then reported as a trailing character. This failed
/// for newtype and struct variants too, which have no array anywhere near
/// them, and it is independent of defects 1 and 2.
#[test]
fn stream_closes_the_enum_object_brace() {
    accepted::<Variant>(r#"{"New":5}"#, Variant::New(5));
    accepted::<Variant>(r#"{"Struct":{"a":5}}"#, Variant::Struct { a: 5 });
    accepted::<Variant>(r#"{"Tup":[1,2]}"#, Variant::Tup(1, 2));
    accepted::<Vec<Variant>>(
        r#"[{"New":1},{"Struct":{"a":2}},{"Tup":[3,4]}]"#,
        vec![
            Variant::New(1),
            Variant::Struct { a: 2 },
            Variant::Tup(3, 4),
        ],
    );
    // A unit variant is spelled as a bare string and never opens the
    // object, so it was already correct; pinned so the fix is not mistaken
    // for what made it work.
    accepted::<Variant>(r#""Unit""#, Variant::Unit);
}

#[test]
fn an_enum_object_with_more_than_one_key_is_still_rejected() {
    // `Payload::close`'s error message is the one thing that keeps
    // `{"A":1,"B":2}` out; closing the brace properly must not loosen it.
    same::<Variant>(r#"{"New":5,"Tup":[1,2]}"#);
    assert!(dacode_json::stream::from_slice::<Variant>(br#"{"New":5,"Tup":[1,2]}"#).is_err());
    assert!(dacode_json::from_slice::<Variant>(br#"{"New":5,"Tup":[1,2]}"#).is_err());
}
