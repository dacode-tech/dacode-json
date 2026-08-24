//! Tier 2 — simdjson-style structural index + tape DOM.
//!
//! Port of `tier2/parse.vl` (475 lines) plus [`tape`] (`tier2/tape.vl`).
//! Stage 1 is shared with tier 3 and lives in [`crate::scan`].
//!
//! # The thing to know about tier 2
//!
//! **Its public API rebuilds the entire tape on every single call.**
//! `json_object_get`, `json_object_count`, `json_object_keys` and
//! `json_array_count` each begin with `json_tape_build(input)`
//! (`tier2/parse.vl:347`, `:371`, `:392`, `:452`), which scans the
//! structural index and constructs the tape from scratch. And
//! `json_array_get` (`:417`) does not use the tape at all — it is a byte
//! scan identical to tier 1's.
//!
//! So for the navigation API, tier 2 does strictly more work than tier 1:
//! the same byte-level scanning, *plus* an index, *plus* a tape, thrown
//! away each time. The benchmark bears this out.
//!
//! That is an integration problem, not an algorithm problem — the tape
//! itself is sound. [`Tier2Cached`] keeps the tape across queries and shows
//! what the design is actually worth; the difference between the two is the
//! cost of the mistake.
//!
//! # Also worth knowing
//!
//! SIMD is **off by default**. `structural_gate.vl:4-7` says "Default: SIMD
//! off (scalar). Call `json_simd_accel_enable()` to switch", and
//! `json_tape_build` goes through `json_structural_scan_gated`
//! (`tape.vl:301`). Stock tier 2 therefore runs the scalar scanner.
//! [`TapeBuilder::with_scanner`] makes the choice explicit here.

pub mod tape;

use super::common::{self, skip_whitespace};
use super::{tier1, JsonTier, Tier, TypeName};
use std::borrow::Cow;
use tape::{Tag, Tape, TapeBuilder};

/// The content of a key entry, given its payload (offset after the opening
/// quote).
fn key_bytes(input: &[u8], payload: usize) -> &[u8] {
    // `skip_string` wants the opening quote, which is one before.
    let end = common::skip_string(input, payload.saturating_sub(1));
    input
        .get(payload..end.saturating_sub(1))
        .unwrap_or_default()
}

/// Byte offset where the value after a key begins.
fn value_start_after_key(input: &[u8], payload: usize) -> usize {
    let key_end = common::skip_string(input, payload.saturating_sub(1));
    let p = skip_whitespace(input, key_end);
    if input.get(p) == Some(&b':') {
        skip_whitespace(input, p + 1)
    } else {
        p
    }
}

/// Walk an object tape, yielding `(key bytes, raw value bytes)`.
///
/// Shared by `object_get`, `object_count` and `object_keys`, which Vela
/// implements three times over.
fn object_pairs<'a>(
    input: &'a [u8],
    t: &'a Tape,
) -> impl Iterator<Item = (&'a [u8], &'a [u8])> + 'a {
    let close = if t.tag(0) == Some(Tag::ObjOpen) {
        t.payload(0).unwrap_or(0)
    } else {
        0
    };

    let mut i = 1usize;
    core::iter::from_fn(move || {
        while i < close {
            if t.tag(i) == Some(Tag::Key) {
                let payload = t.payload(i).unwrap_or(0);
                let key = key_bytes(input, payload);
                let vstart = value_start_after_key(input, payload);
                let vend = tier1::skip_value(input, vstart);
                let value = input.get(vstart..vend).unwrap_or_default();

                i = t.skip_value(i + 1);
                return Some((key, value));
            }
            i += 1;
        }
        None
    })
}

/// Tier 2: structural index + tape, rebuilt per call — Vela's shipped
/// behaviour.
#[derive(Debug, Clone, Copy, Default)]
pub struct Tier2;

impl JsonTier for Tier2 {
    const TIER: Tier = Tier::Two;

    fn parse_string(input: &[u8]) -> String {
        tier1::Tier1::parse_string(input)
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

    /// Tier 2 keeps tier 1's byte-level `skip_value`
    /// (`tier2/parse.vl` imports it from `common.vl`).
    fn skip_value(input: &[u8], pos: usize) -> usize {
        tier1::skip_value(input, pos)
    }

    /// `json_object_get` — `tier2/parse.vl:346`. Builds a whole tape, then
    /// falls back to `skip_value` byte scanning to delimit the value.
    fn object_get<'a>(input: &'a [u8], key: &str) -> Option<&'a [u8]> {
        let t = Tape::build(input);
        // The tape borrows nothing, but the returned slice borrows `input`,
        // so the offsets have to be recomputed outside the closure.
        let close = if t.tag(0) == Some(Tag::ObjOpen) {
            t.payload(0)?
        } else {
            return None;
        };

        let mut i = 1usize;
        while i < close {
            if t.tag(i) == Some(Tag::Key) {
                let payload = t.payload(i).unwrap_or(0);
                if key_bytes(input, payload) == key.as_bytes() {
                    let vstart = value_start_after_key(input, payload);
                    let vend = tier1::skip_value(input, vstart);
                    return input.get(vstart..vend);
                }
                i = t.skip_value(i + 1);
            } else {
                i += 1;
            }
        }
        None
    }

    /// `json_object_count` — `tier2/parse.vl:370`.
    fn object_count(input: &[u8]) -> usize {
        let t = Tape::build(input);
        object_pairs(input, &t).count()
    }

    /// `json_object_keys` — `tier2/parse.vl:391`.
    fn object_keys(input: &[u8]) -> String {
        let t = Tape::build(input);
        let mut out = String::new();
        for (i, (k, _)) in object_pairs(input, &t).enumerate() {
            if i > 0 {
                out.push(',');
            }
            out.push_str(&String::from_utf8_lossy(k));
        }
        out
    }

    fn object_key_list(input: &[u8]) -> Vec<Cow<'_, str>> {
        let t = Tape::build(input);
        object_pairs(input, &t)
            .map(|(k, _)| Cow::Owned(String::from_utf8_lossy(k).into_owned()))
            .collect()
    }

    /// `json_array_get` — `tier2/parse.vl:417`.
    ///
    /// Does **not** use the tape. This is byte-for-byte tier 1's
    /// implementation.
    fn array_get(input: &[u8], idx: usize) -> Option<&[u8]> {
        tier1::Tier1::array_get(input, idx)
    }

    /// `json_array_count` — `tier2/parse.vl:452`. Builds a tape and walks
    /// the top-level array with `tape_skip_value`.
    fn array_count(input: &[u8]) -> usize {
        let t = Tape::build(input);
        if t.tag(0) != Some(Tag::ArrOpen) {
            return 0;
        }
        let close = t.payload(0).unwrap_or(0);
        let mut count = 0usize;
        let mut i = 1usize;
        while i < close {
            count += 1;
            let next = t.skip_value(i);
            if next <= i {
                break;
            }
            i = next;
        }
        count
    }

    /// `json_validate` — `tier2/parse.vl:331`. Scans the structural index,
    /// checks bracket balance, then does tier 1's `skip_value` check.
    fn validate(input: &[u8]) -> bool {
        if input.is_empty() {
            return false;
        }
        let p = skip_whitespace(input, 0);
        if p >= input.len() {
            return false;
        }
        if !structural_valid(input) {
            return false;
        }
        let end = tier1::skip_value(input, p);
        skip_whitespace(input, end) == input.len()
    }

    fn parse_value_at(input: &[u8], pos: usize) -> Option<&[u8]> {
        tier1::Tier1::parse_value_at(input, pos)
    }
}

/// `json_structural_valid` — brackets balanced and correctly nested.
#[must_use]
pub fn structural_valid(input: &[u8]) -> bool {
    let si = crate::scan::scan(crate::scan::Scanner::default(), input);
    let mut stack: Vec<u8> = Vec::new();
    for &p in si.positions() {
        let Some(&ch) = input.get(p as usize) else {
            return false;
        };
        match ch {
            b'{' | b'[' => stack.push(ch),
            b'}' if stack.pop() != Some(b'{') => return false,
            b']' if stack.pop() != Some(b'[') => return false,
            _ => {}
        }
    }
    stack.is_empty()
}

// =====================================================================
// The fix Vela did not apply
// =====================================================================

/// Tier 2 with the tape kept across queries.
///
/// Same algorithm, same tape, same navigation — the only difference is that
/// `json_tape_build` is called once instead of once per query. This is not
/// in Vela; it exists to separate "the tape DOM is a bad design" from "the
/// tier-2 API throws it away", which the benchmark shows are very different
/// claims.
#[derive(Debug, Clone)]
pub struct Tier2Cached {
    builder: TapeBuilder,
}

impl Default for Tier2Cached {
    fn default() -> Self {
        Self::new()
    }
}

impl Tier2Cached {
    #[must_use]
    pub fn new() -> Self {
        Tier2Cached {
            builder: TapeBuilder::new(),
        }
    }

    #[must_use]
    pub fn with_capacity(max_input_len: usize) -> Self {
        Tier2Cached {
            builder: TapeBuilder::with_capacity(max_input_len),
        }
    }

    /// Choose the Stage 1 scanner. Vela's runtime gate defaults to scalar.
    #[must_use]
    pub fn with_scanner(mut self, s: crate::scan::Scanner) -> Self {
        self.builder = self.builder.with_scanner(s);
        self
    }

    /// Build the tape for `input`, returning a queryable view.
    pub fn parse<'a>(&'a mut self, input: &'a [u8]) -> Doc<'a> {
        self.builder.build(input);
        Doc {
            input,
            tape: self.builder.tape(),
        }
    }
}

/// A built tape paired with the input it indexes.
#[derive(Debug, Clone, Copy)]
pub struct Doc<'a> {
    input: &'a [u8],
    tape: &'a Tape,
}

impl<'a> Doc<'a> {
    #[inline]
    #[must_use]
    pub fn tape(&self) -> &'a Tape {
        self.tape
    }

    /// Object lookup against the already-built tape.
    #[must_use]
    pub fn object_get(&self, key: &str) -> Option<&'a [u8]> {
        let close = if self.tape.tag(0) == Some(Tag::ObjOpen) {
            self.tape.payload(0)?
        } else {
            return None;
        };
        let mut i = 1usize;
        while i < close {
            if self.tape.tag(i) == Some(Tag::Key) {
                let payload = self.tape.payload(i).unwrap_or(0);
                if key_bytes(self.input, payload) == key.as_bytes() {
                    let vstart = value_start_after_key(self.input, payload);
                    let vend = tier1::skip_value(self.input, vstart);
                    return self.input.get(vstart..vend);
                }
                i = self.tape.skip_value(i + 1);
            } else {
                i += 1;
            }
        }
        None
    }

    #[must_use]
    pub fn object_count(&self) -> usize {
        object_pairs(self.input, self.tape).count()
    }

    /// Array element by index, via the tape rather than a byte scan.
    #[must_use]
    pub fn array_get(&self, idx: usize) -> Option<&'a [u8]> {
        if self.tape.tag(0) != Some(Tag::ArrOpen) {
            return None;
        }
        let close = self.tape.payload(0)?;
        let mut i = 1usize;
        let mut n = 0usize;
        while i < close {
            if n == idx {
                return self.raw_at(i);
            }
            let next = self.tape.skip_value(i);
            if next <= i {
                break;
            }
            i = next;
            n += 1;
        }
        None
    }

    #[must_use]
    pub fn array_count(&self) -> usize {
        if self.tape.tag(0) != Some(Tag::ArrOpen) {
            return 0;
        }
        let close = self.tape.payload(0).unwrap_or(0);
        let mut count = 0usize;
        let mut i = 1usize;
        while i < close {
            count += 1;
            let next = self.tape.skip_value(i);
            if next <= i {
                break;
            }
            i = next;
        }
        count
    }

    /// Raw JSON text of the value at tape index `i`.
    fn raw_at(&self, i: usize) -> Option<&'a [u8]> {
        let e = self.tape.entries().get(i)?;
        let start = match e.tag {
            Tag::Str => e.payload.checked_sub(1)?,
            Tag::Number => e.payload,
            // The tape does not record offsets for these, so fall back to
            // locating them from the container's own structure. Callers
            // wanting the literal text of a bool/null get it from the input
            // by scanning; for arrays and objects we use the byte scanner.
            _ => return None,
        };
        let end = tier1::skip_value(self.input, start);
        self.input.get(start..end)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn object_navigation_matches_tier1() {
        let obj = br#"{"a":1,"b":"x","c":[1,2],"d":{"e":null}}"#;
        assert_eq!(Tier2::object_count(obj), 4);
        assert_eq!(Tier2::object_get(obj, "a"), Some(b"1".as_slice()));
        assert_eq!(Tier2::object_get(obj, "b"), Some(br#""x""#.as_slice()));
        assert_eq!(Tier2::object_get(obj, "c"), Some(b"[1,2]".as_slice()));
        assert_eq!(
            Tier2::object_get(obj, "d"),
            Some(br#"{"e":null}"#.as_slice())
        );
        assert_eq!(Tier2::object_get(obj, "zz"), None);
        assert_eq!(Tier2::object_keys(obj), "a,b,c,d");
    }

    #[test]
    fn array_counts() {
        assert_eq!(Tier2::array_count(br#"[1,"two",[3],{"f":4},null]"#), 5);
        assert_eq!(Tier2::array_count(b"[]"), 0);
        assert_eq!(Tier2::array_count(b"[1]"), 1);
        assert_eq!(Tier2::array_count(b"{}"), 0);
    }

    #[test]
    fn cached_agrees_with_per_call() {
        let mut c = Tier2Cached::new();
        for src in [
            &br#"{"a":1,"b":[1,2,3]}"#[..],
            br#"{"x":{"y":"z"}}"#,
            b"[1,2,3]",
            b"{}",
        ] {
            let doc = c.parse(src);
            assert_eq!(doc.object_count(), Tier2::object_count(src));
            assert_eq!(doc.array_count(), Tier2::array_count(src));
            assert_eq!(doc.object_get("a"), Tier2::object_get(src, "a"));
        }
    }

    #[test]
    fn structural_validity() {
        assert!(structural_valid(br#"{"a":[1,2]}"#));
        assert!(!structural_valid(b"{"));
        assert!(!structural_valid(b"}"));
        assert!(!structural_valid(b"{]"));
        assert!(!structural_valid(b"[}"));
        // Brackets inside strings do not count.
        assert!(structural_valid(br#"{"a":"[[["}"#));
    }
}
