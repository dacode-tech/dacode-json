//! An owned raw JSON value that splices into output verbatim.
//!
//! This is `dacode-json`'s counterpart of `serde_json`'s `RawValue`: a JSON
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
//! # The trust boundary
//!
//! The positional handoff is *trusted* — the walk validated the span — and
//! travels a crate-private channel ([`TrustedStr`]) that no generic serde
//! route can produce. Every other route into a `RawJson` — map keys, flat
//! string slots, the pool's canonical re-serialisation, a foreign
//! deserializer — lands in the validating `visit_str`/`visit_string`
//! entries and errors cleanly on text that is not JSON.
//!
//! # Example
//!
//! ```
//! use dacode_json::RawJson;
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
//!     dacode_json::to_string(&e)?,
//!     r#"{"name":"edit","args":{"path":"a.rs","line":42}}"#
//! );
//! # Ok::<(), dacode_json::Error>(())
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
pub const TOKEN: &str = "$dacode_json::private::RawJson";

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

    /// Wrap text already known to be one well-formed JSON value, without
    /// validating it again.
    ///
    /// Crate-private on purpose: the public API stays validating-only, so
    /// the crate's no-panics guarantee holds as stated — this function's
    /// `debug_assert` can fire only through an internal bug, never from a
    /// public entry point with caller-chosen text.
    ///
    /// The capture path's constructor. The positional deserializer
    /// ([`crate::direct`]) validates a value while walking past it, then
    /// hands the exact span through [`TrustedStr`]; routing those bytes
    /// through [`from_string`](Self::from_string) would parse them a
    /// second time for a verdict the walk already established — measured
    /// as nearly all of capture's overhead over the ignore floor.
    ///
    /// # Contract
    ///
    /// `json` must be exactly one well-formed JSON value — the guarantee
    /// [`from_string`](Self::from_string) buys by validating. Debug builds
    /// assert it with a real validation, so an internal routing mistake
    /// fails in tests rather than shipping a `RawJson` whose verbatim
    /// splice could corrupt a host document. Release builds skip the
    /// check: that is the point of the constructor.
    ///
    /// The pool path must not use this. A `Doc` from the *faithful* parser
    /// ([`crate::parse_to_pool`]) can re-serialise to text that is not
    /// valid JSON, so [`from_doc`](Self::from_doc) keeps validating — see
    /// its documentation for the bug that established that.
    pub(crate) fn from_validated_str(json: &str) -> Self {
        // `validate` refuses input over `MAX_INPUT_LEN` (its positions are
        // `u32`) — a size limit of the strict parser, not a JSON rule, and
        // the first thing it checks, so asking costs O(1). An oversized
        // span therefore does not break this contract; the assert accepts
        // exactly that one rejection and no other.
        debug_assert!(
            match crate::validate(json.as_bytes()) {
                Ok(()) => true,
                Err(crate::Error::Parse(e)) => {
                    json.len() > crate::scan::MAX_INPUT_LEN
                        && e.kind == crate::strict::ErrorKind::InputTooLarge
                }
                Err(_) => false,
            },
            "from_validated_str: not one well-formed JSON value"
        );
        RawJson(Box::from(json))
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
        // The sentinel struct is the signal: `dacode-json`'s serializer recognises
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

    // The trust boundary, in one place. `dacode-json`'s deserializers
    // special-case `TOKEN` in `deserialize_newtype_struct` and hand the
    // captured text back through these methods:
    //
    // * `visit_newtype_struct` — the capture handoff. The positional
    //   deserializer calls it with [`TrustedStr`], a crate-private
    //   deserializer holding a span its walk just validated. The
    //   extraction re-enters `deserialize_newtype_struct` with
    //   [`StrCapture`], which only `TrustedStr` answers: a foreign
    //   deserializer that reaches this method routes the re-entry to its
    //   own `visit_newtype_struct(StrCapture)` and gets the same clean
    //   error serde's default gave before this channel existed. (A
    //   deserializer that called `visit_newtype_struct` here and then
    //   *forwarded* the re-entry to `deserialize_any` could still reach
    //   the trusted constructor — but that requires behaving differently
    //   across two calls of the same method, which no in-tree or sane
    //   foreign deserializer does. `serde_json`'s sentinel-map design
    //   carries the same class of adversarial residual.)
    // * `visit_str` / `visit_string` — everything else, and they keep
    //   validating. Map keys, flat string slots and any borrowed str a
    //   generic `forward_to_deserialize_any!` routes here arrive
    //   unvalidated; the pool path's canonical re-serialisation arrives
    //   as an owned `String` and must stay checked, because a
    //   faithful-parser `Doc` can re-serialise to invalid JSON.
    fn visit_newtype_struct<D>(self, deserializer: D) -> Result<RawJson, D::Error>
    where
        D: de::Deserializer<'de>,
    {
        let text = deserializer.deserialize_newtype_struct(TOKEN, StrCapture)?;
        Ok(RawJson::from_validated_str(text))
    }

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

/// The capture handoff: a span the positional deserializer validated while
/// walking past it, handed to [`RawJsonVisitor::visit_newtype_struct`].
///
/// `pub(crate)` is the trust boundary: no code outside the crate can
/// construct one, so no generic serde route — map keys, flat string slots,
/// a foreign deserializer — can reach the trusted constructor with text
/// nobody vouched for.
pub(crate) struct TrustedStr<'de>(pub(crate) &'de str);

impl<'de> de::Deserializer<'de> for TrustedStr<'de> {
    type Error = crate::stream::Error;

    /// The one accepting method: what the capture handoff calls.
    fn deserialize_newtype_struct<V: Visitor<'de>>(
        self,
        _name: &'static str,
        visitor: V,
    ) -> Result<V::Value, Self::Error> {
        visitor.visit_borrowed_str(self.0)
    }

    /// Everything else fails closed, so a mis-routed call cannot turn into
    /// an accepting path.
    fn deserialize_any<V: Visitor<'de>>(self, _visitor: V) -> Result<V::Value, Self::Error> {
        Err(crate::stream::Error::from_parts(
            0,
            "expected the raw value handoff",
        ))
    }

    serde::forward_to_deserialize_any! {
        bool i8 i16 i32 i64 i128 u8 u16 u32 u64 u128 f32 f64 char str string
        bytes byte_buf option unit unit_struct seq tuple tuple_struct map
        struct enum identifier ignored_any
    }
}

/// Extracts the span from [`TrustedStr`].
///
/// Accepts only `visit_borrowed_str`, which only `TrustedStr`'s handoff
/// calls; every other method falls to serde's erroring defaults, so a
/// foreign deserializer routed here by mistake fails closed.
struct StrCapture;

impl<'de> Visitor<'de> for StrCapture {
    type Value = &'de str;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("the trusted raw value handoff")
    }

    fn visit_borrowed_str<E>(self, v: &'de str) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        Ok(v)
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
        assert!(
            res.is_err(),
            "stream path must reject RawJson, not corrupt it"
        );
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

    // --- the trusted constructor ----------------------------------------

    #[test]
    fn trusted_constructor_is_byte_identical_to_the_validating_path() {
        // Representative shapes: composites, scalars, escapes, non-ASCII,
        // every number spelling, whitespace-heavy formatting. The trusted
        // path must hand back exactly what the validating path does.
        for src in [
            r#"{"a":1,"b":[true,null,"x"]}"#,
            "{ \"a\" : [ 1 , 2 ] }",
            r#""é 😀 \n""#,
            "[1,-0,1.5,1e2,0.1,12345678901234567890]",
            "42",
            "true",
            "null",
            "[]",
            "{}",
            r#""""#,
        ] {
            let trusted = RawJson::from_validated_str(src);
            let validating = src
                .parse::<RawJson>()
                .unwrap_or_else(|e| panic!("{src}: {e}"));
            assert_eq!(trusted, validating, "on {src}");
            assert_eq!(trusted.get(), src, "on {src}");
        }
    }

    #[test]
    #[cfg(debug_assertions)]
    #[should_panic(expected = "from_validated_str")]
    fn trusted_constructor_asserts_its_contract_in_debug() {
        // The contract is "caller already validated"; the debug assert is
        // what pins it. `#[cfg(debug_assertions)]` because the assert is
        // compiled out in release, where a `#[should_panic]` test would
        // fail for the wrong reason — the repo otherwise has no
        // `should_panic` tests, and this is why.
        let _ = RawJson::from_validated_str("{not json");
    }

    #[test]
    fn capture_rejects_numbers_that_validate_rejects() {
        // The audit behind the trusted capture: the walk's grammar must be
        // at least as strict as `crate::validate`'s, or a captured span
        // would break `from_validated_str`'s contract. Integer literals
        // long enough to overflow f64 (>= 310 digits) were the one case
        // where it was not — `validate_number` skipped them on the theory
        // that only floats overflow.
        let wire = format!(r#"{{"name":"x","args":[{}]}}"#, "9".repeat(400));
        assert!(
            crate::from_str::<Event>(&wire).is_err(),
            "a 400-digit integer overflows f64 and must not capture"
        );
        // 20 digits fit `u64` and stay finite as f64: accepted, and the
        // capture is byte-exact.
        let wire = format!(r#"{{"name":"x","args":[{}]}}"#, "9".repeat(20));
        let e = crate::from_str::<Event>(&wire).unwrap();
        assert_eq!(e.args.get(), &format!("[{}]", "9".repeat(20)));
    }

    #[test]
    fn visit_str_still_validates_untrusted_text() {
        // `visit_borrowed_str` is the trusted handoff; `visit_str` remains
        // the validating entry for a str nobody vouched for — a future
        // capture wiring, or a foreign deserializer that special-cases
        // `TOKEN`. It must not inherit the trust.
        use serde::de::Visitor as _;
        let ok: Result<RawJson, crate::de::Error> = RawJsonVisitor.visit_str(r#"{"a":1}"#);
        assert_eq!(ok.expect("valid").get(), r#"{"a":1}"#);
        let bad: Result<RawJson, crate::de::Error> = RawJsonVisitor.visit_str("{not json");
        assert!(bad.is_err());
    }

    #[test]
    fn visit_borrowed_str_still_validates_untrusted_text() {
        // The borrowed hook is what `forward_to_deserialize_any!` routes
        // map keys and flat string slots through. There is deliberately no
        // override here, so serde's default forwards to the validating
        // `visit_str` — the round-1 bug was trusting this hook.
        use serde::de::Visitor as _;
        let ok: Result<RawJson, crate::de::Error> = RawJsonVisitor.visit_borrowed_str(r#"{"a":1}"#);
        assert_eq!(ok.expect("valid").get(), r#"{"a":1}"#);
        let bad: Result<RawJson, crate::de::Error> = RawJsonVisitor.visit_borrowed_str("é");
        assert!(bad.is_err());
    }

    #[test]
    fn capture_depth_is_pinned_against_validate() {
        // The audit's depth dimension, as a test. The walk's MAX_DEPTH=128
        // is stricter than validate's STACK_MAX=256, so every span the walk
        // accepts is accepted by validate — the direction the trusted
        // constructor's debug-assert depends on. A captured span, nested
        // one object deep, tops out at 128 internal levels.
        for depth in [1usize, 63, 127, 128] {
            let span = format!("{}1{}", "[".repeat(depth), "]".repeat(depth));
            let wire = format!(r#"{{"name":"x","args":{span}}}"#);
            let e: Event = crate::from_str(&wire).unwrap_or_else(|e| panic!("depth {depth}: {e}"));
            assert_eq!(e.args.get(), span.as_str(), "depth {depth}");
        }
        // The stricter direction: deep nests are valid JSON that `validate`
        // accepts but the walk refuses, so capture never sees them. Bare
        // documents ride one level past the composite check (the innermost
        // scalar skips it), so the boundary is 130, not 129; 257 is past
        // validate's own limit.
        for depth in [130usize, 200, 256] {
            let doc = format!("{}1{}", "[".repeat(depth), "]".repeat(depth));
            assert!(
                crate::validate(doc.as_bytes()).is_ok(),
                "depth {depth}: validate accepts"
            );
            assert!(
                crate::from_str::<serde::de::IgnoredAny>(&doc).is_err(),
                "depth {depth}: the walk is stricter"
            );
        }
        let too_deep = format!("{}1{}", "[".repeat(257), "]".repeat(257));
        assert!(crate::validate(too_deep.as_bytes()).is_err());
    }

    #[test]
    fn capture_path_exercises_the_trusted_constructor_in_debug() {
        // In a debug build every capture below runs the constructor's
        // `debug_assert` — a real `crate::validate` over the captured span —
        // so the whole existing capture test set doubles as a check that
        // the walk's validation implies `validate`'s acceptance. This test
        // says so out loud, and adds the shapes the others do not: deep
        // nesting near the walk's limit, and escaped non-ASCII strings.
        let deep = format!("{}1{}", "[".repeat(100), "]".repeat(100));
        let src = format!(r#"{{"name":"x","args":{deep}}}"#);
        let e: Event = crate::from_str(&src).unwrap();
        assert_eq!(e.args.get(), deep.as_str());
        let e: Event = crate::from_str(r#"{"name":"x","args":"é \n é"}"#).unwrap();
        assert_eq!(e.args.get(), r#""é \n é""#);
    }
}
