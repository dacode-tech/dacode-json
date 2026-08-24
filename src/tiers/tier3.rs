//! Tier 3 — yyjson-style flat node pool, presented through the common
//! tier interface.
//!
//! The implementation lives in [`crate::pool`], [`crate::builder`],
//! [`crate::query`] and [`crate::workspace`]; this module is only the
//! adapter that lets tier 3 be compared against the others generically.
//!
//! # Two things the adapter has to paper over
//!
//! **Tier 3 has no per-call navigation API of the shape the contract
//! describes.** Its real interface is "parse once into a pool, then query
//! it" (`json_pool_parse_ws` + `json_pool_object_get`). The legacy wrappers
//! that do match the contract — `json_object_get(input, key)` and friends
//! at `tier3/parse.vl:687-701` — call `yy_parse(input)` on every
//! invocation, exactly the mistake tier 2 makes. Those wrappers are what
//! [`Tier3`] implements, so the comparison is like-for-like. For what tier
//! 3 is actually for, use [`crate::Workspace`] directly.
//!
//! **Tier 3's number semantics differ from tiers 0–2.** Its scalar parser
//! skips `.`, `e`, `E`, `+`, `-` mid-number and keeps accumulating, so
//! `3.14` becomes `314` (`runtime/json_pool_write.ll:771-784`). Tiers 0–2
//! stop at the first non-digit and give `3`. Both are wrong; they are
//! wrong differently. [`Tier3::parse_number`] uses the tier-0/1 helper so
//! the shared contract stays coherent — the tier-3 behaviour is still
//! reachable through [`crate::scalar::parse_number`] and is pinned by
//! `tests/faithful_semantics.rs`.

use super::common;
use super::{JsonTier, Tier, TypeName};
use crate::query::Doc;
use crate::tag::Type;
use crate::workspace::parse_to_pool;
use std::borrow::Cow;

/// Tier 3: flat pool, rebuilt per call (the legacy wrappers).
#[derive(Debug, Clone, Copy, Default)]
pub struct Tier3;

/// Raw JSON text spanning the value at `pos`.
///
/// Tier 3's pool records string offsets but not the extent of containers,
/// so the legacy wrappers delimit values with `json_skip_value`
/// (`tier3/parse.vl:552` → `pool_val_to_json`). Reproduced with the
/// tier-1 byte scanner, which is what Vela links here.
fn raw_span(input: &[u8], pos: usize) -> Option<&[u8]> {
    let start = common::skip_whitespace(input, pos);
    if start >= input.len() {
        return None;
    }
    let end = super::tier1::skip_value(input, start);
    input.get(start..end)
}

/// Locate the raw text of an object member by walking the input.
///
/// The pool gives us the key positions; the value extent still needs the
/// byte scanner.
fn member_raw<'a>(input: &'a [u8], key: &str) -> Option<&'a [u8]> {
    let pool = parse_to_pool(input);
    let doc = Doc::new(input, &pool);
    let root = doc.root();
    if root.typ() != Type::Object {
        return None;
    }
    // Find the key, then re-derive the value extent from the input so the
    // returned slice borrows `input` rather than the temporary pool.
    let mut found: Option<usize> = None;
    for (k, v) in root.entries() {
        if k == key.as_bytes() {
            found = Some(match v.typ() {
                // For strings the pool payload is the offset after the
                // opening quote.
                Type::String => v.node().payload.saturating_sub(1) as usize,
                _ => {
                    // For everything else, locate by scanning from the key.
                    let kpos = v.index();
                    let _ = kpos;
                    return scan_member(input, key);
                }
            });
            break;
        }
    }
    let start = found?;
    raw_span(input, start)
}

/// Fallback: find a member's raw text with the byte scanner.
fn scan_member<'a>(input: &'a [u8], key: &str) -> Option<&'a [u8]> {
    super::tier1::Tier1::object_get(input, key)
}

impl JsonTier for Tier3 {
    const TIER: Tier = Tier::Three;

    /// `json_extract_string` — `tier3/parse.vl:331`. Same lossy decoder as
    /// tiers 0–2.
    fn parse_string(input: &[u8]) -> String {
        super::tier1::Tier1::parse_string(input)
    }

    /// See the module note: uses the tier-0/1 semantics, not tier 3's
    /// `3.14 -> 314`.
    fn parse_number(input: &[u8]) -> i64 {
        common::parse_number_leading(input)
    }

    fn parse_bool(input: &[u8]) -> bool {
        common::parse_bool_leading(input)
    }

    fn validate_string(input: &[u8]) -> bool {
        common::validate_string_leading(input)
    }

    fn detect_type(input: &[u8]) -> TypeName {
        common::detect_type_leading(input)
    }

    fn skip_value(input: &[u8], pos: usize) -> usize {
        super::tier1::skip_value(input, pos)
    }

    /// `json_object_get` — `tier3/parse.vl:687`, which is
    /// `json_pool_object_get(input, yy_parse(input), key)`: a full parse
    /// per call.
    fn object_get<'a>(input: &'a [u8], key: &str) -> Option<&'a [u8]> {
        member_raw(input, key)
    }

    /// `json_object_count` — `tier3/parse.vl:690`. The pool stores the pair
    /// count on the object node, so this is O(1) *after* the parse.
    fn object_count(input: &[u8]) -> usize {
        let pool = parse_to_pool(input);
        let doc = Doc::new(input, &pool);
        let root = doc.root();
        if root.typ() == Type::Object {
            root.len()
        } else {
            0
        }
    }

    fn object_keys(input: &[u8]) -> String {
        let pool = parse_to_pool(input);
        let doc = Doc::new(input, &pool);
        let root = doc.root();
        if root.typ() != Type::Object {
            return String::new();
        }
        let mut out = String::new();
        for (i, (k, _)) in root.entries().enumerate() {
            if i > 0 {
                out.push(',');
            }
            out.push_str(&String::from_utf8_lossy(k));
        }
        out
    }

    fn object_key_list(input: &[u8]) -> Vec<Cow<'_, str>> {
        let pool = parse_to_pool(input);
        let doc = Doc::new(input, &pool);
        let root = doc.root();
        if root.typ() != Type::Object {
            return Vec::new();
        }
        root.entries()
            .map(|(k, _)| Cow::Owned(String::from_utf8_lossy(k).into_owned()))
            .collect()
    }

    /// `json_array_get` — `tier3/parse.vl:696`.
    fn array_get(input: &[u8], idx: usize) -> Option<&[u8]> {
        // Same story as `object_get`: the extent comes from the scanner.
        super::tier1::Tier1::array_get(input, idx)
    }

    /// `json_array_count` — `tier3/parse.vl:699`. O(1) after the parse.
    fn array_count(input: &[u8]) -> usize {
        let pool = parse_to_pool(input);
        let doc = Doc::new(input, &pool);
        let root = doc.root();
        if root.typ() == Type::Array {
            root.len()
        } else {
            0
        }
    }

    /// `json_validate` — `tier3/parse.vl:705`. Identical to tier 1's: a
    /// `skip_value` consume-everything check, not real validation.
    fn validate(input: &[u8]) -> bool {
        super::tier1::Tier1::validate(input)
    }

    fn parse_value_at(input: &[u8], pos: usize) -> Option<&[u8]> {
        super::tier1::Tier1::parse_value_at(input, pos)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_come_from_the_pool() {
        let obj = br#"{"a":1,"b":"x","c":[1,2],"d":{"e":null}}"#;
        assert_eq!(Tier3::object_count(obj), 4);
        assert_eq!(Tier3::array_count(br#"[1,"two",[3],{"f":4},null]"#), 5);
        assert_eq!(Tier3::object_keys(obj), "a,b,c,d");
    }

    #[test]
    fn object_get_returns_raw_text() {
        let obj = br#"{"a":1,"b":"x","c":[1,2]}"#;
        assert_eq!(Tier3::object_get(obj, "a"), Some(b"1".as_slice()));
        assert_eq!(Tier3::object_get(obj, "b"), Some(br#""x""#.as_slice()));
        assert_eq!(Tier3::object_get(obj, "c"), Some(b"[1,2]".as_slice()));
        assert_eq!(Tier3::object_get(obj, "nope"), None);
    }
}
