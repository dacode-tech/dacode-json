//! JSON string unescaping.
//!
//! Vela tier 3 never unescapes: string and key nodes carry raw
//! `(offset, len)` slices. The only unescaper in the tree,
//! `json_extract_string` (`tier3/parse.vl:331-377`), is lossy — `\b` and
//! `\f` decode to a space and `\uXXXX` decodes to `"?"`.
//!
//! This module provides both: [`unescape`] is correct, and
//! [`unescape_vela_lossy`] reproduces the Vela behaviour so the port can be
//! differentially tested against it.

use std::borrow::Cow;

/// Decode JSON escapes, including `\uXXXX` and surrogate pairs.
///
/// Returns `None` if the input is not valid UTF-8 or contains a malformed
/// escape. Borrows when there is no backslash to process.
#[must_use]
pub fn unescape(raw: &[u8]) -> Option<Cow<'_, str>> {
    if !raw.contains(&b'\\') {
        return std::str::from_utf8(raw).ok().map(Cow::Borrowed);
    }

    let mut out = String::with_capacity(raw.len());
    let mut i = 0usize;

    while i < raw.len() {
        let &b = raw.get(i)?;
        if b != b'\\' {
            // Copy the whole run of literal bytes at once.
            let start = i;
            while raw.get(i).is_some_and(|&c| c != b'\\') {
                i += 1;
            }
            out.push_str(std::str::from_utf8(raw.get(start..i)?).ok()?);
            continue;
        }

        i += 1;
        let &esc = raw.get(i)?;
        i += 1;
        match esc {
            b'"' => out.push('"'),
            b'\\' => out.push('\\'),
            b'/' => out.push('/'),
            b'b' => out.push('\u{8}'),
            b'f' => out.push('\u{c}'),
            b'n' => out.push('\n'),
            b'r' => out.push('\r'),
            b't' => out.push('\t'),
            b'u' => {
                let hi = hex4(raw.get(i..i + 4)?)?;
                i += 4;
                let ch = if (0xD800..0xDC00).contains(&hi) {
                    // High surrogate — a low surrogate must follow.
                    if raw.get(i..i + 2) != Some(b"\\u".as_slice()) {
                        return None;
                    }
                    let lo = hex4(raw.get(i + 2..i + 6)?)?;
                    if !(0xDC00..0xE000).contains(&lo) {
                        return None;
                    }
                    i += 6;
                    let cp = 0x1_0000u32
                        + ((u32::from(hi) - 0xD800) << 10)
                        + (u32::from(lo) - 0xDC00);
                    char::from_u32(cp)?
                } else {
                    char::from_u32(u32::from(hi))?
                };
                out.push(ch);
            }
            _ => return None,
        }
    }

    Some(Cow::Owned(out))
}

#[inline]
fn hex4(bytes: &[u8]) -> Option<u16> {
    let mut v = 0u16;
    for &b in bytes.iter() {
        let d = match b {
            b'0'..=b'9' => b - b'0',
            b'a'..=b'f' => b - b'a' + 10,
            b'A'..=b'F' => b - b'A' + 10,
            _ => return None,
        };
        v = v.checked_mul(16)?.checked_add(u16::from(d))?;
    }
    Some(v)
}

/// Reproduce `json_extract_string` (`tier3/parse.vl:331-377`) byte for byte,
/// lossy `\uXXXX` handling and all.
///
/// Kept for differential testing; do not use it for anything real.
#[must_use]
pub fn unescape_vela_lossy(raw: &[u8]) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut i = 0usize;
    while i < raw.len() {
        let Some(&b) = raw.get(i) else { break };
        if b != b'\\' {
            out.push(b as char);
            i += 1;
            continue;
        }
        i += 1;
        let Some(&esc) = raw.get(i) else { break };
        i += 1;
        match esc {
            b'"' => out.push('"'),
            b'\\' => out.push('\\'),
            b'/' => out.push('/'),
            b'n' => out.push('\n'),
            b'r' => out.push('\r'),
            b't' => out.push('\t'),
            // Vela decodes both of these to a space.
            b'b' | b'f' => out.push(' '),
            // Vela emits a literal '?' and skips the four hex digits.
            b'u' => {
                out.push('?');
                i += 4;
            }
            // Unknown escapes append nothing.
            _ => {}
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn borrows_when_clean() {
        let c = unescape(b"hello").expect("valid");
        assert!(matches!(c, Cow::Borrowed("hello")));
    }

    #[test]
    fn simple_escapes() {
        let c = unescape(br#"a\nb\tc\\d\"e\/f"#).expect("valid");
        assert_eq!(c, "a\nb\tc\\d\"e/f");
    }

    #[test]
    fn unicode_escapes() {
        assert_eq!(unescape(br"\u0041").expect("valid"), "A");
        assert_eq!(unescape(br"\u00e9").expect("valid"), "é");
        assert_eq!(unescape(br"\u4e2d\u6587").expect("valid"), "中文");
    }

    #[test]
    fn surrogate_pairs() {
        assert_eq!(unescape(br"\ud83d\ude00").expect("valid"), "😀");
    }

    #[test]
    fn rejects_lone_surrogate_and_bad_hex() {
        assert!(unescape(br"\ud83d").is_none());
        assert!(unescape(br"\uZZZZ").is_none());
        assert!(unescape(br"\q").is_none());
    }

    #[test]
    fn vela_lossy_quirks() {
        assert_eq!(unescape_vela_lossy(br"\b\f"), "  ");
        assert_eq!(unescape_vela_lossy(br"\u0041"), "?");
        assert_eq!(unescape_vela_lossy(br"\q"), "");
    }
}
