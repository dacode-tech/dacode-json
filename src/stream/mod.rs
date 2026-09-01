//! An index-fed streaming deserializer: Stage 1, then straight into serde.
//!
//! `src/de.rs` builds a node pool and then walks it. Profiling put that at
//! roughly 45% parse / 55% walk, and it is why the pool loses to
//! `serde_json` on struct deserialization (194 vs 293 MiB/s) — filling a
//! struct does not need an intermediate representation, so building one is
//! work that the winner never does.
//!
//! This module keeps Stage 1 (the SIMD structural index, 2.1–3.3 GiB/s)
//! and throws Stage 2 away. Values are decoded directly into serde
//! visitors as the walk passes them, so:
//!
//! * no node is ever written;
//! * a field the target struct does not want is **skipped by counting
//!   depth over the index**, never by scanning its bytes;
//! * a number is converted once, into the type actually asked for.
//!
//! This is the shape of simdjson's On-Demand API, which is the fastest
//! JSON reader there is, so the design is not speculative.
//!
//! # Why keep Stage 1
//!
//! A byte-at-a-time streaming parser (what `serde_json` is) must find
//! every delimiter with scalar code. The index has already found them, 16
//! or 32 bytes at a time, and it makes whitespace free: the next
//! interesting byte is the next index entry, never a scan.
//!
//! # Validation
//!
//! Identical to [`crate::strict`], including for values that are skipped.
//! Skipping avoids *converting* a value, not *checking* it — otherwise
//! this would accept documents that `serde_json` rejects, which for a
//! drop-in replacement is a bug, not an optimisation. `tests/stream.rs`
//! and the differential fuzzer both hold it to that.
//!
//! # What lives where
//!
//! The deserializer itself is in `deserializer.rs` and needs `serde`, and
//! through the structural index an allocator. What stays here needs
//! neither: [`Error`], and the number and literal readers that
//! [`crate::pull`] and [`crate::direct`] share with it. That is the split
//! that lets `pull` build for a target with no heap while still reporting
//! errors that read the same as this module's.

use core::fmt;

use crate::errmsg::Msg;

#[cfg(feature = "serde")]
mod deserializer;
#[cfg(feature = "serde")]
pub use deserializer::{from_slice, from_slice_with, Index};

// =====================================================================
// Errors
// =====================================================================

/// A streaming parse or type error, with the byte offset where it was hit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Error {
    msg: Msg,
    /// Byte offset in the input, when the error came from the parser.
    pub offset: Option<usize>,
}

impl Error {
    /// Build an error from a byte offset and a message.
    ///
    /// Exists so [`crate::direct`] and [`crate::pull`] report identically
    /// to this module.
    pub(crate) fn from_parts(offset: usize, msg: &'static str) -> Self {
        Error::at(offset, msg)
    }

    fn at(offset: usize, msg: &'static str) -> Self {
        Error {
            msg: Msg::Static(msg),
            offset: Some(offset),
        }
    }
    /// Only the deserializer raises an error with no position: a type
    /// mismatch is about the value, not a place in the bytes.
    #[cfg(feature = "serde")]
    fn plain(msg: &'static str) -> Self {
        Error {
            msg: Msg::Static(msg),
            offset: None,
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.offset {
            Some(o) => write!(f, "{} at byte {o}", self.msg),
            None => fmt::Display::fmt(&self.msg, f),
        }
    }
}

// `core::error::Error` rather than `std::error::Error`: the two are the
// same trait — `std` re-exports it — so this costs a `std` user nothing
// and saves gating the impl out of a `no_std` build.
impl core::error::Error for Error {}

#[cfg(feature = "serde")]
impl serde::de::Error for Error {
    fn custom<T: fmt::Display>(msg: T) -> Self {
        Error {
            msg: Msg::custom(msg),
            offset: None,
        }
    }
}

type Result<T> = core::result::Result<T, Error>;

// =====================================================================
// Scalars
// =====================================================================

/// A validated JSON number, in the widest form that holds it exactly.
pub(crate) enum Num {
    I(i64),
    U(u64),
    F(f64),
}

/// Validate and convert a number, following RFC 8259 exactly.
///
/// Integers that fit go to `i64`/`u64` so no precision is lost; anything
/// with a fraction or exponent goes through `str::parse`, which is
/// correctly rounded. `docs/RESULTS.md` records that `serde_json`'s own
/// float parser is not, by up to 2 ULP.
/// Validate a number's syntax without converting it.
///
/// Returns `(negative, integer-digit range, is_float)`. Skipped fields use
/// this alone: checking `1.7976931348623157e308` is a scan, converting it
/// is a call into the float parser, and a field the target type ignores
/// should not pay for the second.
pub(crate) fn number_syntax(s: &[u8], start: usize) -> Result<(bool, usize, usize, bool)> {
    if s.is_empty() {
        return Err(Error::at(start, "expected a number"));
    }
    let mut i = 0usize;
    let neg = s.first() == Some(&b'-');
    if neg {
        i = 1;
    }
    let int_start = i;
    match s.get(i) {
        Some(b'0') => {
            i += 1;
            // `01` is two tokens, not a number.
            if matches!(s.get(i), Some(c) if c.is_ascii_digit()) {
                return Err(Error::at(start + i, "leading zero in number"));
            }
        }
        Some(c) if c.is_ascii_digit() => {
            while matches!(s.get(i), Some(c) if c.is_ascii_digit()) {
                i += 1;
            }
        }
        _ => return Err(Error::at(start + i, "invalid number")),
    }
    let int_end = i;
    let mut is_float = false;

    if s.get(i) == Some(&b'.') {
        is_float = true;
        i += 1;
        let f0 = i;
        while matches!(s.get(i), Some(c) if c.is_ascii_digit()) {
            i += 1;
        }
        if i == f0 {
            return Err(Error::at(start + i, "expected a digit after '.'"));
        }
    }
    if matches!(s.get(i), Some(b'e' | b'E')) {
        is_float = true;
        i += 1;
        if matches!(s.get(i), Some(b'+' | b'-')) {
            i += 1;
        }
        let e0 = i;
        while matches!(s.get(i), Some(c) if c.is_ascii_digit()) {
            i += 1;
        }
        if i == e0 {
            return Err(Error::at(start + i, "expected a digit in exponent"));
        }
    }
    if i != s.len() {
        return Err(Error::at(start + i, "trailing characters in number"));
    }
    Ok((neg, int_start, int_end, is_float))
}

/// Syntax-check a number the way a skipped field needs, including the
/// range check that makes `1e400` an error rather than an infinity.
///
/// Only the two deserializers skip fields — [`crate::pull`] hands the
/// bytes back unconverted — so this exists only when they do.
#[cfg(feature = "serde")]
pub(crate) fn validate_number(input: &[u8], start: usize, end: usize) -> Result<()> {
    let s = input.get(start..end).unwrap_or(&[]);
    let (_, _, _, is_float) = number_syntax(s, start)?;
    if is_float {
        // Only floats can overflow, and only then must we actually parse.
        if let Ok(t) = core::str::from_utf8(s) {
            if t.parse::<f64>().map(|v| !v.is_finite()).unwrap_or(true) {
                return Err(Error::at(start, "number out of range"));
            }
        }
    }
    Ok(())
}

pub(crate) fn parse_number(input: &[u8], start: usize, end: usize) -> Result<Num> {
    let s = input.get(start..end).unwrap_or(&[]);
    if s.is_empty() {
        return Err(Error::at(start, "expected a number"));
    }
    let (neg, int_start, int_end, is_float) = number_syntax(s, start)?;

    if !is_float {
        let digits = s.get(int_start..int_end).unwrap_or(&[]);
        // Fast path: short enough that u64 cannot overflow.
        if digits.len() <= 19 {
            let mut acc: u64 = 0;
            for &d in digits {
                acc = acc.wrapping_mul(10).wrapping_add(u64::from(d - b'0'));
            }
            if neg {
                // `-0` is negative zero, not the integer 0. serde_json
                // agrees, and `docs/RESULTS.md` records this as a bug the
                // corpus caught in the pool parser too.
                if acc == 0 {
                    return Ok(Num::F(-0.0));
                }
                if acc <= (i64::MAX as u64) + 1 {
                    return Ok(Num::I((acc as i64).wrapping_neg()));
                }
            } else {
                return Ok(Num::U(acc));
            }
        }
        // Long integers: fall back to the library parsers, then to f64,
        // matching what serde_json does with values that do not fit.
        if let Ok(t) = core::str::from_utf8(s) {
            if let Ok(v) = t.parse::<i64>() {
                return Ok(Num::I(v));
            }
            if let Ok(v) = t.parse::<u64>() {
                return Ok(Num::U(v));
            }
            if let Ok(v) = t.parse::<f64>() {
                if v.is_finite() {
                    return Ok(Num::F(v));
                }
            }
        }
        return Err(Error::at(start, "number out of range"));
    }

    match core::str::from_utf8(s).ok().and_then(|t| t.parse::<f64>().ok()) {
        // `1e400` overflows to infinity. serde_json calls that out of
        // range rather than storing an infinity, so we do too.
        Some(v) if v.is_finite() => Ok(Num::F(v)),
        Some(_) => Err(Error::at(start, "number out of range")),
        None => Err(Error::at(start, "invalid number")),
    }
}

/// Validate a `true` / `false` / `null` literal.
pub(crate) fn expect_lit(input: &[u8], start: usize, end: usize, lit: &[u8]) -> Result<()> {
    if input.get(start..end) == Some(lit) {
        Ok(())
    } else {
        Err(Error::at(start, "invalid literal"))
    }
}
