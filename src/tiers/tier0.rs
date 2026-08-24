//! Tier 0 — scalar detection only.
//!
//! Port of `tier0/parse.vl` (289 lines). Selected with
//! `--define json_tier=0`.
//!
//! > *"Minimal parser: string, number, bool, null detection and extraction.
//! > No object/array parsing. Tier 1+ functions return stubs."*
//! > — `tier0/parse.vl:2-3`
//!
//! Every container operation is a stub returning `""` / `0` / `false`
//! (`tier0/parse.vl:171-197`). This is not an oversight; it is the tier's
//! stated scope. It exists as a minimum-footprint backend for code that
//! only ever pulls scalars out of tiny fixed-shape payloads.
//!
//! # Why it is worth porting anyway
//!
//! Two reasons. It establishes the floor for the benchmark — how fast is
//! the *simplest possible thing* — and it is the only tier whose
//! `json_parse_string` is reachable in isolation, which makes it the
//! cleanest place to measure Vela's O(n²) string building against a Rust
//! `String`.
//!
//! # The O(n²) string builder
//!
//! `tier0/parse.vl:68` appends one character at a time with
//! `result = "{result}{ch}"`, reallocating the whole string on every byte.
//! The port uses `String::push`, which is amortised O(1). This is a
//! deliberate divergence — reproducing the quadratic behaviour would make
//! the benchmark a measurement of Vela's string type rather than of the
//! parsing algorithm. The decoded *output* is byte-identical, quirks
//! included.

use super::common;
use super::{JsonTier, Tier, TypeName};
use std::borrow::Cow;

/// Tier 0: scalars only.
#[derive(Debug, Clone, Copy, Default)]
pub struct Tier0;

impl JsonTier for Tier0 {
    const TIER: Tier = Tier::Zero;

    /// Tier 0 cannot parse containers at all.
    const HANDLES_CONTAINERS: bool = false;

    /// `json_parse_string` — `tier0/parse.vl:19`.
    ///
    /// Lossy exactly as Vela is: `\b` and `\f` become a space, `\uXXXX`
    /// becomes `?`, unknown escapes contribute nothing.
    fn parse_string(input: &[u8]) -> String {
        let pos = common::skip_whitespace(input, 0);
        if input.get(pos) != Some(&b'"') {
            return String::new();
        }
        // Content runs to the closing quote, or to end of input if
        // unterminated — Vela returns what it has rather than failing.
        let end = common::skip_string(input, pos);
        let content_end = if input.get(end.wrapping_sub(1)) == Some(&b'"') {
            end - 1
        } else {
            end
        };
        let raw = input.get(pos + 1..content_end).unwrap_or_default();
        crate::unescape::unescape_vela_lossy(raw)
    }

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

    // --- stubs: `tier0/parse.vl:171-197` ---

    /// Stub. Vela returns `pos` unchanged, so a caller looping on it would
    /// spin forever.
    fn skip_value(_input: &[u8], pos: usize) -> usize {
        pos
    }

    /// Stub — always `None`.
    fn object_get<'a>(_input: &'a [u8], _key: &str) -> Option<&'a [u8]> {
        None
    }

    /// Stub — always 0.
    fn object_count(_input: &[u8]) -> usize {
        0
    }

    /// Stub — always empty.
    fn object_keys(_input: &[u8]) -> String {
        String::new()
    }

    /// Stub — always empty.
    fn object_key_list(_input: &[u8]) -> Vec<Cow<'_, str>> {
        Vec::new()
    }

    /// Stub — always `None`.
    fn array_get(_input: &[u8], _idx: usize) -> Option<&[u8]> {
        None
    }

    /// Stub — always 0.
    fn array_count(_input: &[u8]) -> usize {
        0
    }

    /// Stub — always `false`, even for valid JSON.
    fn validate(_input: &[u8]) -> bool {
        false
    }

    /// Not present in tier 0 at all; the contract requires the symbol.
    fn parse_value_at(_input: &[u8], _pos: usize) -> Option<&[u8]> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_string_matches_vela_tests() {
        // From `tier0/parse.vl:201-225`.
        assert_eq!(Tier0::parse_string(br#""hello""#), "hello");
        assert_eq!(Tier0::parse_string(br#""""#), "");
        assert_eq!(Tier0::parse_string(br#"  "world""#), "world");
        assert_eq!(Tier0::parse_string(br#""a\"b""#), "a\"b");
        assert_eq!(Tier0::parse_string(br#""line1\nline2""#), "line1\nline2");
        assert_eq!(Tier0::parse_string(br#""a\tb""#), "a\tb");
        assert_eq!(Tier0::parse_string(br#""path\\file""#), "path\\file");
        assert_eq!(Tier0::parse_string(b""), "");
        assert_eq!(Tier0::parse_string(b"hello"), "");
        assert_eq!(Tier0::parse_string(b"   "), "");
    }

    #[test]
    fn parse_string_is_lossy_like_vela() {
        assert_eq!(Tier0::parse_string(br#""a\bb""#), "a b");
        assert_eq!(Tier0::parse_string(br#""a\fb""#), "a b");
        assert_eq!(Tier0::parse_string(br#""\u0041""#), "?");
        // Unknown escapes append nothing.
        assert_eq!(Tier0::parse_string(br#""a\qb""#), "ab");
    }

    #[test]
    fn containers_are_stubs() {
        let obj = br#"{"a":1,"b":2}"#;
        assert_eq!(Tier0::object_count(obj), 0);
        assert_eq!(Tier0::object_get(obj, "a"), None);
        assert_eq!(Tier0::object_keys(obj), "");
        assert_eq!(Tier0::array_count(b"[1,2,3]"), 0);
        assert_eq!(Tier0::array_get(b"[1,2,3]", 0), None);
        // Even valid JSON fails to validate.
        assert!(!Tier0::validate(obj));
        // And skip_value makes no progress.
        assert_eq!(Tier0::skip_value(obj, 0), 0);
    }
}
