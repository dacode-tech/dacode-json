//! An owned raw JSON value that splices into output verbatim.
//!
//! This is `dacodec`'s counterpart of `serde_json`'s `RawValue`: a JSON
//! document carried as text and written back out **byte-for-byte**, with no
//! re-escaping, no re-encoding and no key reordering. It exists for the case
//! where a value parsed out of one document must be embedded in another
//! without the round trip perturbing it.
//!
//! The motivating use is an event-sourced protocol whose events carry opaque
//! JSON (a tool call's `args`, a provider's raw body). Such a value is parsed
//! once, stored, and then re-emitted on every subsequent turn. Provider prompt
//! caching keys on a byte-stable prefix, so re-encoding that reorders keys or
//! reformats numbers would silently defeat the cache. [`RawJson`] keeps the
//! bytes fixed.
//!
//! Gated behind the `raw_value` feature; the default build is unaffected.
//!
//! # Capture fidelity
//!
//! Serialising a [`RawJson`] is always byte-exact: the stored text is copied out
//! verbatim. *Deserialising* one depends on which deserializer runs:
//!
//! - **Positional** ([`crate::from_str`], [`crate::from_slice`],
//!   [`crate::Parser`]) — the common path — captures the value's **exact source
//!   bytes**, whitespace and all, like `serde_json`'s borrowed `RawValue`.
//! - **Pool** ([`crate::de::from_doc`]) indexes object/array nodes by child
//!   position, not byte span, so capture there re-serialises **canonically**
//!   ([`crate::query::Value::to_json`]).
//! - **Stream** ([`crate::stream::from_slice_with`]) does not capture; a
//!   [`RawJson`] field there is a clear deserialization error, never corruption.
//!
//! Both capturing paths are deterministic, so the byte-stability guarantee
//! holds either way: capture once, splice the same bytes forever.
//!
//! # Example
//!
//! ```
//! use dacodec::RawJson;
//! use serde::{Deserialize, Serialize};
//!
//! #[derive(Serialize, Deserialize)]
//! struct Event {
//!     name: String,
//!     args: RawJson,
//! }
//!
//! let e = Event {
//!     name: "edit".into(),
//!     args: r#"{"path":"a.rs","line":42}"#.parse()?,
//! };
//! // `args` is spliced in verbatim — not escaped into a string.
//! assert_eq!(
//!     dacodec::to_string(&e)?,
//!     r#"{"name":"edit","args":{"path":"a.rs","line":42}}"#
//! );
//! # Ok::<(), dacodec::Error>(())
//! ```

use alloc::boxed::Box;
use alloc::string::String;
use core::fmt;
use serde::de::{self, Deserialize, Deserializer, Visitor};
use serde::ser::{Serialize, SerializeStruct, Serializer};

/// Sentinel struct/field name that routes a [`RawJson`] through the
/// serializer's verbatim path and the deserializer's capture path.
///
/// A name no real schema would use, so the special case cannot collide with a
/// genuine struct. Mirrors `serde_json`'s private `RawValue` token.
pub const TOKEN: &str = "$dacodec::private::RawJson";

/// An owned, validated JSON document that serialises by splicing its text
/// verbatim into the output.
///
/// `Clone + Send + Sync + 'static`, so it can live in an event that is stored,
/// broadcast and persisted. See the [module docs](self) for the byte-stability
/// contract.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct RawJson(Box<str>);

impl RawJson {
    /// Wrap an owned `String` of JSON, checking it is one well-formed value.
    ///
    /// Avoids a copy when the text is already owned. For a `&str`, use
    /// [`FromStr`](core::str::FromStr) (`s.parse()`).
    ///
    /// # Errors
    ///
    /// Returns the parse error if `json` is not a single valid JSON value
    /// (trailing content, unbalanced brackets, bad literal). Validation is what
    /// makes the verbatim splice safe: a `RawJson` always holds JSON, so
    /// embedding it cannot corrupt the surrounding document.
    pub fn from_string(json: String) -> crate::Result<Self> {
        crate::validate(json.as_bytes())?;
        Ok(RawJson(json.into_boxed_str()))
    }

    /// Wrap a value parsed from a document, re-serialised canonically.
    ///
    /// The canonical text is **validated**, so this upholds the invariant that a
    /// `RawJson` always holds JSON and its verbatim splice cannot corrupt the
    /// host document. Validation is not redundant: a `Doc` may come from the
    /// *faithful* parser ([`crate::parse_to_pool`]), which does not validate and
    /// can hold a raw control byte or `NaN` that [`Value::to_json`] re-emits
    /// verbatim, yielding invalid JSON. The strict-parsed deserialize paths only
    /// reach here with already-valid input.
    ///
    /// # Errors
    ///
    /// Returns the parse error if the canonical re-serialisation is not valid
    /// JSON — reachable only from a non-validating (`parse_to_pool`) `Doc`.
    pub fn from_doc(doc: crate::query::Doc<'_>) -> crate::Result<Self> {
        Self::from_string(doc.root().to_json())
    }

    /// The JSON text.
    #[must_use]
    pub fn get(&self) -> &str {
        &self.0
    }

    /// The JSON text as bytes.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        self.0.as_bytes()
    }
}

impl fmt::Debug for RawJson {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Show the JSON itself, not a re-escaped string of it.
        f.write_str(&self.0)
    }
}

impl fmt::Display for RawJson {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl AsRef<str> for RawJson {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl core::str::FromStr for RawJson {
    type Err = crate::Error;

    fn from_str(s: &str) -> crate::Result<Self> {
        // `String::from`, not `s.to_owned()`: `ToOwned` is not in the `no_std`
        // prelude, and `raw_value` implies `alloc` but not `std`.
        Self::from_string(String::from(s))
    }
}

impl Serialize for RawJson {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        // The sentinel struct is the signal: `dacodec`'s serializer recognises
        // `TOKEN` and writes the single field's text verbatim instead of as a
        // quoted string. A foreign serializer falls back to emitting the
        // sentinel object, exactly as `serde_json` does.
        let mut s = serializer.serialize_struct(TOKEN, 1)?;
        s.serialize_field(TOKEN, &self.0)?;
        s.end()
    }
}

impl<'de> Deserialize<'de> for RawJson {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_newtype_struct(TOKEN, RawJsonVisitor)
    }
}

struct RawJsonVisitor;

impl<'de> Visitor<'de> for RawJsonVisitor {
    type Value = RawJson;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a JSON value to capture verbatim")
    }

    // `dacodec`'s deserializer special-cases `TOKEN` in
    // `deserialize_newtype_struct` and hands the canonical text back through
    // `visit_string`/`visit_str`.
    fn visit_str<E>(self, v: &str) -> Result<RawJson, E>
    where
        E: de::Error,
    {
        v.parse().map_err(de::Error::custom)
    }

    fn visit_string<E>(self, v: String) -> Result<RawJson, E>
    where
        E: de::Error,
    {
        RawJson::from_string(v).map_err(de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::{Deserialize, Serialize};

    #[derive(Serialize, Deserialize, PartialEq, Debug)]
    struct Event {
        name: String,
        args: RawJson,
    }

    #[test]
    fn splices_verbatim_inside_a_struct() {
        let e = Event {
            name: "edit".into(),
            args: r#"{"path":"a.rs","line":42}"#.parse().unwrap(),
        };
        let json = crate::to_string(&e).unwrap();
        assert_eq!(json, r#"{"name":"edit","args":{"path":"a.rs","line":42}}"#);
    }

    #[test]
    fn preserves_inner_formatting_on_serialize() {
        // Verbatim means the stored bytes come out unchanged, whitespace and all.
        let raw: RawJson = "{ \"a\" : [ 1, 2 ] }".parse().unwrap();
        assert_eq!(crate::to_string(&raw).unwrap(), "{ \"a\" : [ 1, 2 ] }");
    }

    #[test]
    fn round_trips_through_a_struct() {
        let e = Event {
            name: "edit".into(),
            args: r#"{"path":"a.rs"}"#.parse().unwrap(),
        };
        let json = crate::to_string(&e).unwrap();
        let back: Event = crate::from_str(&json).unwrap();
        assert_eq!(back.name, "edit");
        assert_eq!(back.args.get(), r#"{"path":"a.rs"}"#);
    }

    #[test]
    fn deserialize_is_byte_exact_on_the_positional_path() {
        // `from_str`/`from_slice`/`Parser` use the positional deserializer,
        // which captures the value's exact source bytes — whitespace and all.
        let e: Event = crate::from_str(r#"{"name":"x","args":{ "a" : 1 }}"#).unwrap();
        assert_eq!(e.args.get(), "{ \"a\" : 1 }");
        // Splicing the captured bytes back is byte-stable turn over turn, which
        // is the property a provider's prompt-cache prefix depends on.
        let once = crate::to_string(&e).unwrap();
        let twice = crate::to_string(&crate::from_str::<Event>(&once).unwrap()).unwrap();
        assert_eq!(once, twice);
    }

    #[test]
    fn deserialize_canonicalises_on_the_pool_path() {
        // The pool deserializer (`from_doc`) indexes composites by child
        // position, not byte span, so capture there re-serialises canonically.
        // Still deterministic, so still byte-stable across re-splices.
        let mut p = crate::Parser::new();
        let doc = p.parse(br#"{"name":"x","args":{ "a" : 1 }}"#).unwrap();
        let e: Event = crate::de::from_doc(doc).unwrap();
        assert_eq!(e.args.get(), r#"{"a":1}"#);
    }

    #[test]
    fn from_doc_captures_canonical_text() {
        // `from_doc` re-serialises the subtree canonically (whitespace dropped).
        let input = br#"{ "a" : 1 }"#;
        let pool = crate::parse_to_pool(input);
        let raw = RawJson::from_doc(crate::Doc::new(input, &pool)).unwrap();
        assert_eq!(raw.get(), r#"{"a":1}"#);
    }

    #[test]
    fn from_doc_rejects_invalid_pool_from_the_faithful_parser() {
        // Regression: `parse_to_pool` is the *faithful* parser — it does not
        // validate, so a string holding a raw control byte parses fine, and
        // `to_json` re-emits that byte verbatim, which is not valid JSON.
        // `from_doc` must reject it rather than build a `RawJson` whose verbatim
        // splice would corrupt the host document.
        let input = b"\"\x01\""; // a JSON string containing a raw 0x01 control byte
        let pool = crate::parse_to_pool(input);
        let doc = crate::Doc::new(input, &pool);
        assert!(RawJson::from_doc(doc).is_err());
    }

    #[test]
    fn scalars_and_arrays_capture_too() {
        for (src, want) in [
            ("42", "42"),
            ("true", "true"),
            ("null", "null"),
            ("\"hi\"", "\"hi\""),
            ("[1,2,3]", "[1,2,3]"),
        ] {
            let raw: RawJson = src.parse().unwrap();
            assert_eq!(raw.get(), src);
            let back: RawJson = crate::from_str(src).unwrap();
            assert_eq!(back.get(), want, "capture of {src}");
        }
    }

    #[test]
    fn rejects_invalid_json() {
        assert!("{".parse::<RawJson>().is_err());
        assert!("1 2".parse::<RawJson>().is_err()); // trailing content
        assert!("".parse::<RawJson>().is_err());
    }

    #[test]
    fn is_send_sync_static() {
        fn assert_traits<T: Send + Sync + 'static>() {}
        assert_traits::<RawJson>();
    }

    // --- byte-exact capture boundaries (positional path) -----------------

    #[test]
    fn capture_excludes_surrounding_whitespace_keeps_inner() {
        // Leading ws is skipped before `start`; the walk stops at the value's
        // closing byte, so trailing ws before the next `,` is excluded. Inner
        // formatting is preserved verbatim. The unknown `"z"` field after it
        // also proves capture composes with normal field skipping.
        let e: Event = crate::from_str(r#"{"name":"x","args":{ "a" : 1 } ,"z":1}"#).unwrap();
        assert_eq!(e.args.get(), r#"{ "a" : 1 }"#);
    }

    #[test]
    fn capture_is_string_aware_braces_inside_strings() {
        // A naive span scan would stop at the `}` inside the string; the walk
        // must be quote-aware. Value is `{"s":"}{"}`.
        let e: Event = crate::from_str(r#"{"name":"x","args":{"s":"}{"}}"#).unwrap();
        assert_eq!(e.args.get(), r#"{"s":"}{"}"#);
    }

    #[test]
    fn capture_handles_escaped_quotes() {
        // Value is `{"q":"\"}"}`: a string holding an escaped quote then `}`.
        // The scanner must not end the string at the escaped quote.
        let e: Event = crate::from_str(r#"{"name":"x","args":{"q":"\"}"}}"#).unwrap();
        assert_eq!(e.args.get(), r#"{"q":"\"}"}"#);
    }

    #[test]
    fn capture_handles_nested_arrays_and_objects() {
        let e: Event = crate::from_str(r#"{"name":"x","args":[1,{"y":[2,3]}]}"#).unwrap();
        assert_eq!(e.args.get(), r#"[1,{"y":[2,3]}]"#);
    }

    #[test]
    fn stream_deserializer_errors_cleanly_not_corruptly() {
        // The stream deserializer does not capture raw spans; a `RawJson` field
        // there must be a clean error (serde's default `visit_newtype_struct` ->
        // `invalid_type`), never a panic or a silently wrong value. Documented
        // in the module header and docs/RAWJSON.md §4.
        let res = crate::stream::from_slice::<Event>(br#"{"name":"x","args":{"a":1}}"#);
        assert!(res.is_err(), "stream path must reject RawJson, not corrupt it");
    }

    #[test]
    fn recapture_is_stable_for_whitespace_heavy_value() {
        // The property a prompt-cache prefix depends on: capture once, splice,
        // re-parse, re-splice — identical bytes every turn, original inner
        // formatting preserved (not minified, not reformatted).
        let src = r#"{"name":"x","args":{ "a" : [ 1 , 2 ] } }"#;
        let e: Event = crate::from_str(src).unwrap();
        assert_eq!(e.args.get(), r#"{ "a" : [ 1 , 2 ] }"#);
        let once = crate::to_string(&e).unwrap();
        let twice = crate::to_string(&crate::from_str::<Event>(&once).unwrap()).unwrap();
        assert_eq!(once, twice);
    }
}
