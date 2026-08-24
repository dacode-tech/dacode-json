//! Helpers shared by every tier — the port of `common.vl`.
//!
//! `common.vl` is compiled regardless of which tier is selected
//! (`common.vl:1-3`), and tiers 0–2 all lean on `json_skip_string` and
//! `json_skip_number` from it.
//!
//! Everything here is byte-oriented and allocation-free.

/// `json_is_whitespace` — `common.vl:32`. Space, tab, LF, CR.
#[inline]
#[must_use]
pub const fn is_whitespace(ch: u8) -> bool {
    matches!(ch, b' ' | b'\t' | b'\n' | b'\r')
}

/// `json_skip_whitespace` — `common.vl:40`.
#[inline]
#[must_use]
pub fn skip_whitespace(input: &[u8], pos: usize) -> usize {
    let mut p = pos;
    while let Some(&b) = input.get(p) {
        if !is_whitespace(b) {
            break;
        }
        p += 1;
    }
    p.min(input.len().max(pos))
}

/// `json_is_valid_escape` — `common.vl:19`. The characters that may follow
/// a backslash.
#[inline]
#[must_use]
pub const fn is_valid_escape(b: u8) -> bool {
    matches!(
        b,
        b'"' | b'\\' | b'/' | b'b' | b'f' | b'n' | b'r' | b't' | b'u'
    )
}

/// `json_skip_string` — `common.vl:146-161`.
///
/// `pos` must point at the opening quote. Returns the index just past the
/// closing quote, or `input.len()` if unterminated.
///
/// Note the `\u` handling: Vela advances 4 extra bytes for the hex digits
/// and then the loop's own `p += 1` covers the `u` itself. It does not
/// check that the four bytes are actually hex.
#[inline]
#[must_use]
pub fn skip_string(input: &[u8], pos: usize) -> usize {
    let len = input.len();
    let mut p = pos + 1;
    while p < len {
        match input.get(p) {
            Some(b'"') => return p + 1,
            Some(b'\\') => {
                p += 1;
                if input.get(p) == Some(&b'u') {
                    p += 4;
                }
            }
            Some(_) => {}
            None => break,
        }
        p += 1;
    }
    // Vela returns `p`, which for unterminated input is >= len.
    p.min(len)
}

/// `json_skip_number` — `common.vl:165-180`.
///
/// Consumes an optional leading `-` then any run of digits, `.`, `e`, `E`,
/// `+`, `-`. It validates no grammar at all: `1.2.3e+-4` is consumed whole.
#[inline]
#[must_use]
pub fn skip_number(input: &[u8], pos: usize) -> usize {
    let len = input.len();
    let mut p = pos;
    if input.get(p) == Some(&b'-') {
        p += 1;
    }
    while p < len {
        match input.get(p) {
            Some(b'0'..=b'9' | b'.' | b'e' | b'E' | b'+' | b'-') => p += 1,
            _ => return p,
        }
    }
    p
}

/// `json_parse_number` — `tier0/parse.vl:75`, identical in tier 1.
///
/// Digits only: stops at the first byte that is not `0`–`9`, so `3.14`
/// gives `3` and `1e5` gives `1`. Wrapping arithmetic, matching Vela's
/// unchecked `result * 10 + (b - 48)`.
#[inline]
#[must_use]
pub fn parse_number_leading(input: &[u8]) -> i64 {
    let mut pos = skip_whitespace(input, 0);
    let Some(&first) = input.get(pos) else {
        return 0;
    };

    let neg = first == b'-';
    if neg {
        pos += 1;
    }

    let mut result: i64 = 0;
    while let Some(&b) = input.get(pos) {
        if b.is_ascii_digit() {
            result = result
                .wrapping_mul(10)
                .wrapping_add(i64::from(b - b'0'));
            pos += 1;
        } else {
            break;
        }
    }

    if neg {
        result.wrapping_neg()
    } else {
        result
    }
}

/// `json_parse_bool` — `tier0/parse.vl:101`.
///
/// Only recognises `true`. `false` and garbage both yield `false`.
#[inline]
#[must_use]
pub fn parse_bool_leading(input: &[u8]) -> bool {
    let pos = skip_whitespace(input, 0);
    input.get(pos..pos + 4) == Some(b"true".as_slice())
}

/// `json_validate_string` — `tier0/parse.vl:110`, identical in tier 1.
#[must_use]
pub fn validate_string_leading(input: &[u8]) -> bool {
    let len = input.len();
    let mut pos = skip_whitespace(input, 0);
    if input.get(pos) != Some(&b'"') {
        return false;
    }
    pos += 1;

    while pos < len {
        let Some(&b) = input.get(pos) else { return false };
        if b == b'"' {
            return true;
        }
        if b == b'\\' {
            pos += 1;
            let Some(&esc) = input.get(pos) else {
                return false;
            };
            if esc == b'u' {
                // Vela writes `if pos + 4 >= len { return false }`, so a
                // `\uXXXX` ending exactly at the last byte is rejected.
                // Reproduced, including the off-by-one.
                if pos + 4 >= len {
                    return false;
                }
                pos += 4;
            } else if !is_valid_escape(esc) {
                return false;
            }
        } else if b < 0x20 {
            return false;
        }
        pos += 1;
    }
    false
}

/// `json_detect_type` — `tier0/parse.vl:140`, identical in tier 1 and 2.
///
/// Classifies on the first non-whitespace byte, with a full literal compare
/// only for `true`/`false`/`null`.
#[must_use]
pub fn detect_type_leading(input: &[u8]) -> super::TypeName {
    use super::TypeName as T;

    let pos = skip_whitespace(input, 0);
    let Some(&b) = input.get(pos) else {
        return T::Unknown;
    };

    match b {
        b'"' => T::String,
        b'{' => T::Object,
        b'[' => T::Array,
        b't' if input.get(pos..pos + 4) == Some(b"true".as_slice()) => T::Bool,
        b'f' if input.get(pos..pos + 5) == Some(b"false".as_slice()) => T::Bool,
        b'n' if input.get(pos..pos + 4) == Some(b"null".as_slice()) => T::Null,
        b'-' | b'0'..=b'9' => T::Number,
        _ => T::Unknown,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skip_string_handles_escapes() {
        assert_eq!(skip_string(br#""ab""#, 0), 4);
        assert_eq!(skip_string(br#""a\"b""#, 0), 6);
        assert_eq!(skip_string(br#""a\\""#, 0), 5);
        assert_eq!(skip_string(br#""\u0041""#, 0), 8);
        // Unterminated clamps to len rather than running off the end.
        assert_eq!(skip_string(br#""abc"#, 0), 4);
    }

    #[test]
    fn skip_number_is_permissive() {
        assert_eq!(skip_number(b"123", 0), 3);
        assert_eq!(skip_number(b"-1.5e+3", 0), 7);
        assert_eq!(skip_number(b"1,2", 0), 1);
        // No grammar validation whatsoever.
        assert_eq!(skip_number(b"1.2.3e+-4", 0), 9);
    }

    #[test]
    fn parse_number_stops_at_non_digit() {
        assert_eq!(parse_number_leading(b"42"), 42);
        assert_eq!(parse_number_leading(b"-7"), -7);
        assert_eq!(parse_number_leading(b"  99"), 99);
        assert_eq!(parse_number_leading(b"42abc"), 42);
        assert_eq!(parse_number_leading(b""), 0);
        assert_eq!(parse_number_leading(b"abc"), 0);
        // Documented divergence from tier 3, which skips '.' and keeps
        // accumulating (3.14 -> 314). Tiers 0/1 stop instead.
        assert_eq!(parse_number_leading(b"3.14"), 3);
    }

    #[test]
    fn parse_bool_only_knows_true() {
        assert!(parse_bool_leading(b"true"));
        assert!(parse_bool_leading(b"  true"));
        assert!(parse_bool_leading(b"true rest"));
        assert!(!parse_bool_leading(b"false"));
        assert!(!parse_bool_leading(b"tru"));
        assert!(!parse_bool_leading(b""));
    }

    #[test]
    fn validate_string_matches_vela() {
        assert!(validate_string_leading(br#""hello""#));
        assert!(validate_string_leading(br#""""#));
        assert!(validate_string_leading(br#""a\nb""#));
        assert!(!validate_string_leading(b""));
        assert!(!validate_string_leading(b"hello"));
        assert!(!validate_string_leading(br#""unterminated"#));
        assert!(!validate_string_leading(br#""\x""#));
        assert!(!validate_string_leading(br#"""#));
    }

    #[test]
    fn detect_type_classifies() {
        use super::super::TypeName as T;
        assert_eq!(detect_type_leading(br#""hi""#), T::String);
        assert_eq!(detect_type_leading(b"42"), T::Number);
        assert_eq!(detect_type_leading(b"-1"), T::Number);
        assert_eq!(detect_type_leading(b"true"), T::Bool);
        assert_eq!(detect_type_leading(b"false"), T::Bool);
        assert_eq!(detect_type_leading(b"null"), T::Null);
        assert_eq!(detect_type_leading(b"{"), T::Object);
        assert_eq!(detect_type_leading(b"[1, 2]"), T::Array);
        assert_eq!(detect_type_leading(b""), T::Unknown);
        assert_eq!(detect_type_leading(b"  42"), T::Number);
        // Partial literals are unknown, not bool.
        assert_eq!(detect_type_leading(b"tru"), T::Unknown);
    }
}
