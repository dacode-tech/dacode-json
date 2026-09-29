//! Tier 1 — recursive descent over raw bytes. **Vela's default tier.**
//!
//! Port of `tier1/parse.vl` (590 lines). This is what
//! `--define json_tier=...` gives you when you do not set it
//! (`BUILD.bazel:149`).
//!
//! > *"Recursive descent parser with object/array navigation. Returns raw
//! > JSON substrings -- zero-allocation navigation."*
//! > — `tier1/parse.vl:2-3`
//!
//! # How it works
//!
//! There is no index and no DOM. Every query re-scans the document from
//! byte zero, driven by three mutually recursive skip functions:
//!
//! ```text
//! skip_value ─┬→ skip_string   (common.vl:146)
//!             ├→ skip_object ──→ skip_value
//!             ├→ skip_array  ──→ skip_value
//!             └→ skip_number  (common.vl:165)
//! ```
//!
//! `object_get` walks pairs comparing keys; `array_get` walks elements
//! counting. Both are O(n) in document size per call, and looking up *k*
//! fields is O(k·n).
//!
//! The payoff is that it allocates nothing and touches no memory beyond the
//! input, which on small documents beats every indexed approach — building
//! an index you use once is pure overhead. `benches/tiers.rs` shows exactly
//! where the crossover is.
//!
//! # Divergence from Vela
//!
//! Recursion depth is capped ([`MAX_DEPTH`]). Vela's version has no limit
//! and will exhaust the stack on deeply nested input — reachable from
//! untrusted data, so it is not reproduced. On hitting the cap the port
//! stops descending, which mirrors what tier 3 does at its own 256-level
//! limit.

use super::common::{self, skip_number, skip_string, skip_whitespace};
use super::{JsonTier, Tier, TypeName};
use std::borrow::Cow;

/// Recursion limit for [`skip_value`].
///
/// Vela is unbounded here and will blow the stack; 256 matches tier 3's
/// `IDX_STACK_MAX` (`parse_indexed.vl:66`) so the two tiers agree on how
/// much nesting they accept.
pub const MAX_DEPTH: u32 = 256;

/// `json_skip_value` — `tier1/parse.vl:26`.
///
/// Returns the index one past the value beginning at `pos`.
#[must_use]
pub fn skip_value(input: &[u8], pos: usize) -> usize {
    skip_value_depth(input, pos, 0)
}

fn skip_value_depth(input: &[u8], pos: usize, depth: u32) -> usize {
    let len = input.len();
    let p = skip_whitespace(input, pos);
    let Some(&ch) = input.get(p) else { return p };

    match ch {
        b'"' => skip_string(input, p),
        b'{' if depth < MAX_DEPTH => skip_object_depth(input, p, depth + 1),
        b'[' if depth < MAX_DEPTH => skip_array_depth(input, p, depth + 1),
        // At the depth cap, refuse to descend. Vela recurses forever.
        b'{' | b'[' => p + 1,

        // Vela checks only the first byte and that the length fits — it
        // never verifies the remaining characters spell the literal.
        // `tier1/parse.vl:39-43`.
        b't' if p + 4 <= len => p + 4,
        b'f' if p + 5 <= len => p + 5,
        b'n' if p + 4 <= len => p + 4,

        b'-' | b'0'..=b'9' => skip_number(input, p),

        // "Unrecognized or truncated: advance past it to guarantee
        // progress" — `tier1/parse.vl:48-49`.
        _ => p + 1,
    }
}

/// `json_skip_object` — `tier1/parse.vl:53`.
#[must_use]
pub fn skip_object(input: &[u8], pos: usize) -> usize {
    skip_object_depth(input, pos, 0)
}

fn skip_object_depth(input: &[u8], pos: usize, depth: u32) -> usize {
    let len = input.len();
    let mut p = skip_whitespace(input, pos + 1);
    if input.get(p) == Some(&b'}') {
        return p + 1;
    }

    while p < len {
        p = skip_whitespace(input, p);
        if p >= len {
            return p;
        }
        if input.get(p) == Some(&b'"') {
            p = skip_string(input, p);
        } else {
            return p;
        }

        p = skip_whitespace(input, p);
        if input.get(p) != Some(&b':') {
            return p;
        }
        p += 1;

        p = skip_value_depth(input, p, depth);

        p = skip_whitespace(input, p);
        match input.get(p) {
            None => return p,
            Some(b'}') => return p + 1,
            Some(b',') => p += 1,
            Some(_) => {}
        }
    }
    p
}

/// `json_skip_array` — `tier1/parse.vl:84`.
#[must_use]
pub fn skip_array(input: &[u8], pos: usize) -> usize {
    skip_array_depth(input, pos, 0)
}

fn skip_array_depth(input: &[u8], pos: usize, depth: u32) -> usize {
    let len = input.len();
    let mut p = skip_whitespace(input, pos + 1);
    if input.get(p) == Some(&b']') {
        return p + 1;
    }

    while p < len {
        let before = p;
        p = skip_value_depth(input, p, depth);
        p = skip_whitespace(input, p);
        match input.get(p) {
            None => return p,
            Some(b']') => return p + 1,
            Some(b',') => p += 1,
            Some(_) => {}
        }
        // Vela's loop relies on skip_value always advancing. Guard anyway:
        // a malformed document must not hang the process.
        if p <= before {
            return p + 1;
        }
    }
    p
}

/// Iterator over an object's `(raw key, raw value)` pairs.
///
/// Not in Vela — its three object functions each re-implement the same walk
/// (`tier1/parse.vl:253`, `:287`, `:317`). Factoring it out removes the
/// triplication without changing behaviour.
#[derive(Debug, Clone)]
pub struct Pairs<'a> {
    input: &'a [u8],
    p: usize,
    done: bool,
}

impl<'a> Pairs<'a> {
    #[must_use]
    pub fn new(input: &'a [u8]) -> Self {
        let p = skip_whitespace(input, 0);
        if input.get(p) == Some(&b'{') {
            Pairs {
                input,
                p: p + 1,
                done: false,
            }
        } else {
            Pairs {
                input,
                p: 0,
                done: true,
            }
        }
    }
}

impl<'a> Iterator for Pairs<'a> {
    /// `(key content without quotes, raw value text)`
    type Item = (&'a [u8], &'a [u8]);

    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }
        let input = self.input;

        self.p = skip_whitespace(input, self.p);
        match input.get(self.p) {
            Some(b'"') => {}
            // `}`, end of input, or anything unexpected ends the walk.
            _ => {
                self.done = true;
                return None;
            }
        }

        let key_start = self.p + 1;
        let key_end = skip_string(input, self.p);
        // key_end is one past the closing quote.
        let key = input.get(key_start..key_end.saturating_sub(1))?;
        self.p = key_end;

        self.p = skip_whitespace(input, self.p);
        if input.get(self.p) != Some(&b':') {
            self.done = true;
            return None;
        }
        self.p += 1;

        let val_start = skip_whitespace(input, self.p);
        let val_end = skip_value(input, val_start);
        let value = input.get(val_start..val_end)?;

        self.p = skip_whitespace(input, val_end);
        if input.get(self.p) == Some(&b',') {
            self.p += 1;
        }

        if val_end <= val_start {
            self.done = true;
        }
        Some((key, value))
    }
}

/// Iterator over an array's raw element texts.
#[derive(Debug, Clone)]
pub struct Elements<'a> {
    input: &'a [u8],
    p: usize,
    done: bool,
}

impl<'a> Elements<'a> {
    #[must_use]
    pub fn new(input: &'a [u8]) -> Self {
        let p = skip_whitespace(input, 0);
        if input.get(p) == Some(&b'[') {
            let p = skip_whitespace(input, p + 1);
            // Vela special-cases the empty array before the loop.
            let done = input.get(p) == Some(&b']');
            Elements { input, p, done }
        } else {
            Elements {
                input,
                p: 0,
                done: true,
            }
        }
    }
}

impl<'a> Iterator for Elements<'a> {
    type Item = &'a [u8];

    fn next(&mut self) -> Option<&'a [u8]> {
        if self.done {
            return None;
        }
        let input = self.input;

        self.p = skip_whitespace(input, self.p);
        match input.get(self.p) {
            None | Some(b']') => {
                self.done = true;
                return None;
            }
            Some(_) => {}
        }

        let start = self.p;
        let end = skip_value(input, start);
        let value = input.get(start..end)?;

        self.p = skip_whitespace(input, end);
        if input.get(self.p) == Some(&b',') {
            self.p += 1;
        }

        if end <= start {
            self.done = true;
        }
        Some(value)
    }
}

/// Tier 1: recursive descent, raw-slice navigation.
#[derive(Debug, Clone, Copy, Default)]
pub struct Tier1;

impl JsonTier for Tier1 {
    const TIER: Tier = Tier::One;

    /// `json_parse_string` — `tier1/parse.vl:102`, delegating to
    /// `json_extract_string` (`:111`). Same lossy decoding as tier 0.
    fn parse_string(input: &[u8]) -> String {
        let pos = skip_whitespace(input, 0);
        if input.get(pos) != Some(&b'"') {
            return String::new();
        }
        let end = skip_string(input, pos);
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

    fn skip_value(input: &[u8], pos: usize) -> usize {
        skip_value(input, pos)
    }

    /// `json_object_get` — `tier1/parse.vl:253`. Linear scan, raw byte
    /// comparison of keys (so an escaped key never matches an unescaped
    /// query, same as tier 3).
    fn object_get<'a>(input: &'a [u8], key: &str) -> Option<&'a [u8]> {
        Pairs::new(input)
            .find(|(k, _)| *k == key.as_bytes())
            .map(|(_, v)| v)
    }

    fn object_count(input: &[u8]) -> usize {
        Pairs::new(input).count()
    }

    fn object_keys(input: &[u8]) -> String {
        let mut out = String::new();
        for (i, (k, _)) in Pairs::new(input).enumerate() {
            if i > 0 {
                out.push(',');
            }
            out.push_str(&String::from_utf8_lossy(k));
        }
        out
    }

    fn object_key_list(input: &[u8]) -> Vec<Cow<'_, str>> {
        Pairs::new(input)
            .map(|(k, _)| String::from_utf8_lossy(k))
            .collect()
    }

    /// `json_array_get` — `tier1/parse.vl:355`. O(n): walks and skips every
    /// preceding element.
    fn array_get(input: &[u8], idx: usize) -> Option<&[u8]> {
        Elements::new(input).nth(idx)
    }

    fn array_count(input: &[u8]) -> usize {
        Elements::new(input).count()
    }

    /// `json_validate` — `tier1/parse.vl:404`.
    ///
    /// "Validation" here means only that `skip_value` consumes exactly the
    /// whole input. Since `skip_value` accepts `t`+3 bytes as `true`, this
    /// accepts `txxx` and `[1,2` variants that RFC 8259 does not. Compare
    /// [`crate::strict`].
    fn validate(input: &[u8]) -> bool {
        if input.is_empty() {
            return false;
        }
        let p = skip_whitespace(input, 0);
        if p >= input.len() {
            return false;
        }
        let end = skip_value(input, p);
        skip_whitespace(input, end) == input.len()
    }

    /// `json_parse_value_at` — `tier1/parse.vl:418`.
    fn parse_value_at(input: &[u8], pos: usize) -> Option<&[u8]> {
        let p = skip_whitespace(input, pos);
        if p >= input.len() {
            return None;
        }
        let end = skip_value(input, p);
        if end == p {
            return None;
        }
        input.get(p..end)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn object_navigation() {
        let obj = br#"{"a":1,"b":"x","c":[1,2],"d":{"e":null}}"#;
        assert_eq!(Tier1::object_count(obj), 4);
        assert_eq!(Tier1::object_get(obj, "a"), Some(b"1".as_slice()));
        assert_eq!(Tier1::object_get(obj, "b"), Some(br#""x""#.as_slice()));
        assert_eq!(Tier1::object_get(obj, "c"), Some(b"[1,2]".as_slice()));
        assert_eq!(
            Tier1::object_get(obj, "d"),
            Some(br#"{"e":null}"#.as_slice())
        );
        assert_eq!(Tier1::object_get(obj, "zz"), None);
        assert_eq!(Tier1::object_keys(obj), "a,b,c,d");
    }

    #[test]
    fn array_navigation() {
        let arr = br#"[1,"two",[3],{"f":4},null]"#;
        assert_eq!(Tier1::array_count(arr), 5);
        assert_eq!(Tier1::array_get(arr, 0), Some(b"1".as_slice()));
        assert_eq!(Tier1::array_get(arr, 1), Some(br#""two""#.as_slice()));
        assert_eq!(Tier1::array_get(arr, 2), Some(b"[3]".as_slice()));
        assert_eq!(Tier1::array_get(arr, 3), Some(br#"{"f":4}"#.as_slice()));
        assert_eq!(Tier1::array_get(arr, 4), Some(b"null".as_slice()));
        assert_eq!(Tier1::array_get(arr, 5), None);
    }

    #[test]
    fn empty_containers() {
        assert_eq!(Tier1::object_count(b"{}"), 0);
        assert_eq!(Tier1::array_count(b"[]"), 0);
        assert_eq!(Tier1::object_count(b"{ }"), 0);
        assert_eq!(Tier1::array_count(b"[ ]"), 0);
        assert_eq!(Tier1::object_keys(b"{}"), "");
    }

    #[test]
    fn whitespace_everywhere() {
        let obj = b"  {\n \"a\" : [ 1 , 2 ] ,\t\"b\" : 3 }  ";
        assert_eq!(Tier1::object_count(obj), 2);
        assert_eq!(Tier1::object_get(obj, "b"), Some(b"3".as_slice()));
        assert_eq!(
            Tier1::object_get(obj, "a").map(|v| v.len()),
            Some("[ 1 , 2 ]".len())
        );
    }

    #[test]
    fn validate_accepts_valid_json() {
        for s in [
            &b"{}"[..],
            b"[]",
            b"1",
            b"true",
            b"null",
            br#""s""#,
            br#"{"a":[1,{"b":2}]}"#,
            b"  [1, 2]  ",
        ] {
            assert!(Tier1::validate(s), "{:?}", String::from_utf8_lossy(s));
        }
    }

    /// `json_validate` (`tier1/parse.vl:404`) is documented as "Validate
    /// entire JSON input (recursive). Returns true if valid RFC 8259".
    /// It is nothing of the sort.
    ///
    /// The skip functions return the end-of-input position when they run
    /// off the end (`skip_object` falls out of its loop and returns `p`,
    /// `tier1/parse.vl:80`), and `validate` only asks whether that position
    /// equals `len`. Truncated input therefore always "validates", because
    /// running out of bytes lands exactly on `len`.
    #[test]
    fn validate_accepts_almost_anything() {
        // Every one of these is rejected by serde_json.
        for bad in [
            &b"{"[..],
            b"}",
            b"[",
            b"]",
            b"[1,2",
            br#"{"a":1"#,
            br#"{"a""#,
            b"txxx", // only the first byte of a literal is checked
            b"nxxx",
            b"fxxxx",
            b"[,]",
            b"[1,]",
        ] {
            assert!(
                Tier1::validate(bad),
                "expected the port to reproduce Vela accepting {:?}",
                String::from_utf8_lossy(bad)
            );
            assert!(
                serde_json::from_slice::<serde_json::Value>(bad).is_err(),
                "{:?} should be invalid JSON",
                String::from_utf8_lossy(bad)
            );
        }

        // The handful it does reject, it rejects for incidental reasons:
        // the literal does not fit in the remaining bytes, or a leading
        // byte cannot start any value.
        assert!(!Tier1::validate(b""));
        assert!(!Tier1::validate(b"tru"));
        assert!(!Tier1::validate(b"{,}"));
    }

    #[test]
    fn deep_nesting_does_not_overflow_the_stack() {
        // Vela recurses without a limit here and would crash.
        let deep = format!("{}1{}", "[".repeat(100_000), "]".repeat(100_000));
        let _ = Tier1::validate(deep.as_bytes());
        let _ = Tier1::array_count(deep.as_bytes());
    }
}
