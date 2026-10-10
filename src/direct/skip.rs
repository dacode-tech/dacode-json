//! The validating skip behind `deserialize_ignored_any` and raw capture.
//!
//! serde's route for an ignored container is `deserialize_any` with the
//! `IgnoredAny` visitor — full `Visitor`/`MapAccess`/`SeqAccess`
//! machinery, `next_entry`/`next_value_seed` per child. That indirection
//! is what this module replaces: the same grammar, walked directly, with
//! no serde dispatch and no allocation.
//!
//! The measured split of the speedup — how much was the accessors, how
//! much the number double-scan, what residual remains and why — lives
//! in `benches/rawjson.rs`'s docs, the one place it is kept current.
//! The mechanism the numbers explain: every skipped number was scanned
//! twice, `scalar_end` to find its end and then `number_syntax` to
//! check it, which [`skip_number`]'s fast path folds into one pass.
//!
//! The walk is also the raw-capture walk. `deserialize_newtype_struct`'s
//! `raw_value` arm reuses it to find — and validate — a `RawJson` span,
//! which then travels the crate-private trusted channel without a second
//! parse (see `crate::raw`): every check below is therefore load-bearing
//! for the trust contract, not just for skipped fields. The differential
//! pinning the walk's agreement with `crate::validate` is
//! `tests/ignored_skip.rs`.

use super::{err, Cursor, Result, MAX_DEPTH};
use crate::stream::{expect_lit, validate_number};

/// Walk one value without materialising it, validating exactly what
/// `deserialize_any` would.
///
/// Scalars make the same checks the scalar arms of
/// `deserialize_ignored_any` always made: `scan_string` plus
/// `unescape_checked` for strings, `validate_number` for numbers,
/// `expect_lit` for literals. Containers walk their structure directly —
/// matched brackets, string keys, `:`/`,` placement, no trailing commas —
/// and recurse with `depth + 1`, erroring "nesting too deep" past
/// [`MAX_DEPTH`] where `deserialize_any`'s entry check does. Only
/// containers count depth, so a document whose innermost *scalar* sits
/// past the limit still walks, exactly as before.
pub(super) fn skip_value(c: &mut Cursor<'_>, depth: usize) -> Result<()> {
    c.skip_ws();
    let at = c.pos;
    match c.peek() {
        b'{' => {
            if depth > MAX_DEPTH {
                return Err(err(at, "nesting too deep"));
            }
            c.pos += 1;
            let mut first = true;
            loop {
                c.skip_ws();
                if c.eat(b'}') {
                    return Ok(());
                }
                let b = if first {
                    first = false;
                    c.peek()
                } else {
                    if !c.eat(b',') {
                        return Err(err(c.pos, "expected ',' or '}' in object"));
                    }
                    c.skip_ws();
                    // `{"a":1,}` is not valid JSON.
                    let b = c.peek();
                    if b == b'}' {
                        return Err(err(c.pos, "trailing comma in object"));
                    }
                    b
                };
                skip_key(c, b)?;
                skip_value(c, depth + 1)?;
            }
        }
        b'[' => {
            if depth > MAX_DEPTH {
                return Err(err(at, "nesting too deep"));
            }
            c.pos += 1;
            let mut first = true;
            loop {
                c.skip_ws();
                if c.eat(b']') {
                    return Ok(());
                }
                if !first {
                    if !c.eat(b',') {
                        return Err(err(c.pos, "expected ',' or ']' in array"));
                    }
                    c.skip_ws();
                    if c.peek() == b']' {
                        return Err(err(c.pos, "trailing comma in array"));
                    }
                }
                first = false;
                skip_value(c, depth + 1)?;
            }
        }
        b'"' => {
            let (raw, simple) = c.scan_string()?;
            if !simple && crate::unescape::unescape_checked(raw).is_none() {
                return Err(err(at, "invalid escape or UTF-8 in string"));
            }
            Ok(())
        }
        b't' => skip_lit(c, at, b"true"),
        b'f' => skip_lit(c, at, b"false"),
        b'n' => skip_lit(c, at, b"null"),
        _ => skip_number(c, at),
    }
}

/// One object member's `"key":` — the `:` consumed, then the key's
/// escapes checked, in `ObjectAccess::next_key_seed`'s order, so a key
/// that is both badly escaped and missing its `:` reports the same
/// error either route would. `b` is the byte at the cursor, already
/// peeked by the caller.
///
/// A *simple* key needs no UTF-8 check: `scan_string`'s `simple` means no
/// escape, no control byte and no byte at or above 0x80, so the key is
/// ASCII. Everything else goes through `unescape_checked`, which checks
/// escapes and UTF-8 together.
fn skip_key(c: &mut Cursor<'_>, b: u8) -> Result<()> {
    let at = c.pos;
    if b != b'"' {
        return Err(err(at, "expected an object key"));
    }
    let (raw, simple) = c.scan_string()?;
    c.skip_ws();
    if !c.eat(b':') {
        return Err(err(c.pos, "expected ':' after object key"));
    }
    if !simple && crate::unescape::unescape_checked(raw).is_none() {
        return Err(err(at, "invalid escape or UTF-8 in key"));
    }
    Ok(())
}

/// One `true`/`false`/`null` literal, validated and consumed.
fn skip_lit(c: &mut Cursor<'_>, at: usize, lit: &[u8]) -> Result<()> {
    let e = c.scalar_end();
    expect_lit(c.input, at, e, lit)?;
    c.pos = e;
    Ok(())
}

/// One number, validated and consumed.
///
/// The fast path accepts only an unsigned integer — no sign, no leading
/// zero, at most 20 digits — ending at a delimiter: no fraction or
/// exponent to check, and too short for f64 to overflow, so
/// [`validate_number`] would accept it without the range check. Every
/// other shape, every error included, takes the general path, so
/// verdicts, messages and offsets stay identical to it. The fast path is
/// what the skip is measured on: the general path scans every number
/// twice, once by `scalar_end` and once by `number_syntax`, and on the
/// capture corpus that was the largest single cost after serde's
/// container machinery was removed.
fn skip_number(c: &mut Cursor<'_>, at: usize) -> Result<()> {
    let mut i = c.pos;
    if c.peek() == b'0' {
        i += 1;
    } else {
        while matches!(c.input.get(i), Some(d) if d.is_ascii_digit()) {
            i += 1;
        }
    }
    let digits = i - c.pos;
    let fast = digits > 0
        && digits <= 20
        && matches!(
            c.input.get(i),
            None | Some(b',' | b'}' | b']' | b' ' | b'\t' | b'\n' | b'\r')
        );
    if !fast {
        let e = c.scalar_end();
        validate_number(c.input, at, e)?;
        c.pos = e;
        return Ok(());
    }
    c.pos = i;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stream::Error;

    /// Skip `src` from position 0, returning where the walk stopped.
    fn skip(src: &[u8]) -> Result<usize> {
        let mut c = Cursor {
            input: src,
            pos: 0,
            ascii: false,
        };
        skip_value(&mut c, 0).map(|()| c.pos)
    }

    /// The walk consumes exactly one value — nothing before it, nothing
    /// after — which is what raw capture slices its span from.
    #[test]
    fn consumes_exactly_one_value() {
        for (src, end) in [
            ("{}x", 2),
            ("[]x", 2),
            ("[1,2,3]x", 7),
            (r#"{"a":1,"b":[2]}x"#, 15),
            (r#""s"x"#, 3),
            ("  [ 1 ] x", 7),
            ("null x", 4),
        ] {
            assert_eq!(skip(src.as_bytes()), Ok(end), "on {src:?}");
        }
        // A scalar runs to the next delimiter, so junk glued to it is the
        // scalar's error, not a clean stop: the number and literal
        // validators reject it, as the scalar arms always did.
        for src in ["42x", "truex"] {
            assert!(skip(src.as_bytes()).is_err(), "on {src:?}");
        }
    }

    /// The container errors the accessor machinery used to raise, at the
    /// same bytes with the same text — `tests/error_alloc.rs` pins one of
    /// these through the public API, this pins the rest at the source.
    /// The last two rows pin the check ORDER: a doubly-malformed key
    /// reports the `:` first, and a bad escape with a valid `:` reports
    /// the escape at the key's start, exactly as
    /// `ObjectAccess::next_key_seed` does.
    #[test]
    fn container_errors_keep_their_offset_and_message() {
        for (src, at, msg) in [
            ("{,}", 1, "expected an object key"),
            ("{\"a\" 1}", 5, "expected ':' after object key"),
            ("{\"a\":1 \"b\":2}", 7, "expected ',' or '}' in object"),
            ("{\"a\":1,}", 7, "trailing comma in object"),
            ("[1 2]", 3, "expected ',' or ']' in array"),
            ("[1,]", 3, "trailing comma in array"),
            ("{", 1, "expected an object key"),
            ("[", 1, "expected a number"),
            ("{\"\\uDAA\"}", 8, "expected ':' after object key"),
            ("{\"\\uDAA\":1}", 1, "invalid escape or UTF-8 in key"),
        ] {
            let got = skip(src.as_bytes()).unwrap_err();
            assert_eq!(got, Error::from_parts(at, msg), "on {src:?}: got {got}");
        }
    }

    /// The depth boundary is where `deserialize_any`'s entry check puts
    /// it: a container at depth 129 errors, at 128 it walks. Bare
    /// documents ride one level further because only containers count —
    /// the innermost scalar skips the check — so `[`x129 parses and
    /// `[`x130 does not, as `src/raw.rs`'s depth tests pin.
    #[test]
    fn depth_boundary_matches_deserialize_any() {
        for (depth, ok) in [(129usize, true), (130, false)] {
            let doc = format!("{}1{}", "[".repeat(depth), "]".repeat(depth));
            assert_eq!(
                skip(doc.as_bytes()).is_ok(),
                ok,
                "bare document, depth {depth}"
            );
        }
        // One level of envelope shifts the boundary to 129.
        for (depth, ok) in [(128usize, true), (129, false)] {
            let doc = format!("{{\"a\":{}1{}}}", "[".repeat(depth), "]".repeat(depth));
            assert_eq!(
                skip(doc.as_bytes()).is_ok(),
                ok,
                "wrapped value, depth {depth}"
            );
        }
    }

    /// The number fast path must accept exactly what the general path
    /// accepts, so every shape it declines falls through to the same
    /// verdict: sign, leading zero, fraction, exponent, junk and
    /// 21+ digits all take the general path, and the errors match it.
    #[test]
    fn number_fast_path_agrees_with_the_general_path() {
        for src in ["0", "1", "42", "9".repeat(20).as_str(), "0 ", "123,", " 7"] {
            assert!(skip(src.as_bytes()).is_ok(), "fast path rejected {src:?}");
        }
        for (src, at, msg) in [
            ("01", 1, "leading zero in number"),
            ("1.", 2, "expected a digit after '.'"),
            ("1e", 2, "expected a digit in exponent"),
            ("-", 1, "invalid number"),
            ("+1", 0, "invalid number"),
            (".5", 0, "invalid number"),
            ("42x", 2, "trailing characters in number"),
            ("", 0, "expected a number"),
        ] {
            let got = skip(src.as_bytes()).unwrap_err();
            assert_eq!(got, Error::from_parts(at, msg), "on {src:?}: got {got}");
        }
        // 21+ digits: the fast path declines, the general path's range
        // check accepts them (finite as f64) — same verdict, slower path.
        let long = "9".repeat(21);
        assert!(skip(long.as_bytes()).is_ok());
        // And the one the range check rejects, still via the general path.
        let huge = "9".repeat(400);
        let got = skip(huge.as_bytes()).unwrap_err();
        assert_eq!(got, Error::from_parts(0, "number out of range"));
    }
}
