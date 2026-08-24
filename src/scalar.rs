//! Bounded scalar parsing — `true` / `false` / `null` / numbers.
//!
//! Port of `__json_parse_scalar_fast`
//! (`runtime/json_pool_write.ll:626-835`), the Y4 fast path. It is only ever
//! called on the *gap* between two structural positions, which for
//! well-formed JSON is at most ~25 bytes, so a scalar loop is appropriate.
//!
//! # Fidelity warning
//!
//! This reproduces Vela's behaviour exactly, quirks included. See
//! [`crate::strict`] for an RFC 8259-conformant alternative.
//!
//! * **Numbers are `i64` only.** `.`, `e`, `E`, `+` and `-` encountered
//!   mid-number are *skipped* while digit accumulation continues, so
//!   `3.14` parses as `314` and `1e3` parses as `13`
//!   (`json_pool_write.ll:771-784`).
//! * **No overflow detection.** `acc = acc * 10 + d` wraps.
//! * **`null` is never validated.** The `read_null` block in the IR is an
//!   explicit no-op that falls into `push_null`
//!   (`json_pool_write.ll:732-735`), and `push_null` is also the universal
//!   fallback, so `nope` and `@!?` both parse as `null`.
//! * **All-whitespace ranges push `null`.**

use crate::pool::Pool;
use crate::tag::Type;

/// `idx_skip_ws` — `parse_indexed.vl:137-153`. JSON whitespace is
/// space, tab, LF, CR.
#[inline]
#[must_use]
pub fn skip_ws(input: &[u8], from: usize, to: usize) -> usize {
    let mut p = from;
    while p < to {
        match input.get(p) {
            Some(b' ' | b'\t' | b'\n' | b'\r') => p += 1,
            Some(_) => return p,
            None => return to,
        }
    }
    to
}

/// `__json_parse_scalar_fast(input, pool, from, to)`.
///
/// Always pushes exactly one node (the IR returns 1 on every path).
#[inline]
pub fn parse_scalar_fast(input: &[u8], pool: &mut Pool, from: usize, to: usize) {
    let to = to.min(input.len());
    let p = skip_ws(input, from, to);

    let Some(&first) = input.get(p) else {
        // Empty or all-whitespace range.
        pool.push(Type::Null, 0, 0);
        return;
    };
    if p >= to {
        pool.push(Type::Null, 0, 0);
        return;
    }

    match first {
        // "true" — the IR does a 4-byte LE word compare against 0x65757274.
        b't' if p + 4 <= to && input.get(p..p + 4) == Some(b"true".as_slice()) => {
            pool.push(Type::Bool, 0, 1);
        }

        // "false" — word compare on "fals" (0x736C6166) plus a byte compare
        // on the trailing 'e'.
        b'f' if p + 5 <= to && input.get(p..p + 5) == Some(b"false".as_slice()) => {
            pool.push(Type::Bool, 0, 0);
        }

        // "null" — deliberately unvalidated; see the module docs.
        b'n' => {
            pool.push(Type::Null, 0, 0);
        }

        b'-' | b'0'..=b'9' => {
            let value = parse_number(input, p, to);
            pool.push(Type::Number, 0, value as u64);
        }

        // Unrecognised — null is the universal fallback.
        _ => {
            pool.push(Type::Null, 0, 0);
        }
    }
}

/// The number accumulator from `json_pool_write.ll:746-806`, also spelled
/// out in Vela at `parse_indexed.vl:155-182`.
///
/// Returns the value as `i64`. Wrapping arithmetic matches the IR's
/// unchecked `mul`/`add`.
#[inline]
#[must_use]
pub fn parse_number(input: &[u8], from: usize, to: usize) -> i64 {
    let mut p = from;
    let neg = input.get(p) == Some(&b'-');
    if neg {
        p += 1;
    }

    let mut acc: i64 = 0;
    while p < to {
        let Some(&b) = input.get(p) else { break };
        match b {
            b'0'..=b'9' => {
                acc = acc
                    .wrapping_mul(10)
                    .wrapping_add(i64::from(b - b'0'));
                p += 1;
            }
            // Float syntax is skipped, not parsed. This is the lossy bit.
            b'.' | b'e' | b'E' | b'+' | b'-' => p += 1,
            _ => break,
        }
    }

    if neg {
        acc.wrapping_neg()
    } else {
        acc
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(s: &str) -> (Type, u64) {
        let mut pool = Pool::with_capacity(4);
        parse_scalar_fast(s.as_bytes(), &mut pool, 0, s.len());
        let n = pool.root();
        (n.typ(), n.payload)
    }

    #[test]
    fn booleans() {
        assert_eq!(parse("true"), (Type::Bool, 1));
        assert_eq!(parse("  true "), (Type::Bool, 1));
        assert_eq!(parse("false"), (Type::Bool, 0));
        assert_eq!(parse("tru"), (Type::Null, 0));
        assert_eq!(parse("fals"), (Type::Null, 0));
    }

    #[test]
    fn integers() {
        assert_eq!(parse("0"), (Type::Number, 0));
        assert_eq!(parse("42"), (Type::Number, 42));
        assert_eq!(parse("-42"), (Type::Number, (-42i64) as u64));
        assert_eq!(parse(" 123 "), (Type::Number, 123));
    }

    #[test]
    fn floats_are_lossy_exactly_as_vela() {
        // Documented divergence from RFC 8259: the decimal point and
        // exponent markers are skipped, and the digits run together.
        assert_eq!(parse("3.14"), (Type::Number, 314));
        assert_eq!(parse("1e3"), (Type::Number, 13));
        assert_eq!(parse("-0.5"), (Type::Number, (-5i64) as u64));
        assert_eq!(parse("1.5e-2"), (Type::Number, 152));
    }

    #[test]
    fn null_is_never_validated() {
        assert_eq!(parse("null"), (Type::Null, 0));
        assert_eq!(parse("nope"), (Type::Null, 0));
        assert_eq!(parse("n"), (Type::Null, 0));
    }

    #[test]
    fn garbage_and_whitespace_become_null() {
        assert_eq!(parse(""), (Type::Null, 0));
        assert_eq!(parse("   "), (Type::Null, 0));
        assert_eq!(parse("@!?"), (Type::Null, 0));
    }
}
