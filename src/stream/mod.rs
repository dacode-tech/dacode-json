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
use crate::errmsg::Unexpected;

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

    // serde's default `invalid_type`/`invalid_value` format `unexp` with
    // `Display`, and the `Float` arm of that impl links `core::fmt`'s
    // float formatter — 8.7 KB, for an error message. See
    // `crate::errmsg::Unexpected`.
    #[cold]
    fn invalid_type(unexp: serde::de::Unexpected<'_>, exp: &dyn serde::de::Expected) -> Self {
        <Self as serde::de::Error>::custom(format_args!(
            "invalid type: {}, expected {exp}",
            Unexpected(unexp)
        ))
    }

    #[cold]
    fn invalid_value(unexp: serde::de::Unexpected<'_>, exp: &dyn serde::de::Expected) -> Self {
        <Self as serde::de::Error>::custom(format_args!(
            "invalid value: {}, expected {exp}",
            Unexpected(unexp)
        ))
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

/// Where the parts of a validated number are, without its value.
///
/// Two offsets, not four: the integer digits always start at
/// `neg as usize`, and the fraction digits — when there are any — always
/// start one byte past `int_end`, because that byte is the `.`.
///
/// Keeping it this narrow is not tidiness. Every caller on the no-float
/// path returns this by value, and the obvious four-offset version cost
/// 48 bytes of flash in the `pullint` probe where this one costs 32.
/// Both figures are against the pre-`as_fixed` 2 102; carrying the
/// fraction range at all is not free, and this is the cheapest shape
/// found. `docs/SIZE.md` has the rest.
///
/// The exponent is a yes/no. Nothing reads its digits without also
/// reaching the float parser, which reparses the token from scratch.
#[derive(Clone, Copy)]
pub(crate) struct Syntax {
    pub neg: bool,
    /// One past the last integer digit; also the offset of the `.`, if any.
    pub int_end: usize,
    /// One past the last fraction digit, or `int_end` when there is none.
    pub frac_end: usize,
    pub has_exp: bool,
}

impl Syntax {
    /// The integer digits, within the slice [`number_syntax`] was given.
    ///
    /// ASCII `0`–`9`, and never empty: a number has at least one.
    pub fn int<'a>(&self, s: &'a [u8]) -> &'a [u8] {
        s.get(usize::from(self.neg)..self.int_end).unwrap_or(&[])
    }

    /// The fraction digits, empty when there was no `.`.
    ///
    /// The `+ 1` steps over the `.`. When there is no fraction,
    /// `frac_end == int_end`, so the range is backwards, `get` declines
    /// it, and the answer is the empty slice — which is what a caller
    /// wants anyway.
    pub fn frac<'a>(&self, s: &'a [u8]) -> &'a [u8] {
        s.get(self.int_end + 1..self.frac_end).unwrap_or(&[])
    }

    /// Whether the token has to be read as a float to be read exactly.
    ///
    /// A fact about the spelling, not the value: `1.0` and `1e2` are
    /// integral and still answer `true`.
    ///
    /// Derived rather than cached. Caching it in the padding byte next
    /// to `has_exp` is free in layout and was tried; it cost 36 bytes of
    /// flash in `pullint`, because the compare folds into the branch
    /// that follows it and a stored flag does not.
    pub fn is_float(&self) -> bool {
        self.frac_end > self.int_end || self.has_exp
    }
}

/// Validate a number's syntax without converting it.
///
/// Returns where the digits are, not what they mean. Skipped fields use
/// this alone: checking `1.7976931348623157e308` is a scan, converting it
/// is a call into the float parser, and a field the target type ignores
/// should not pay for the second.
///
/// [`Integer::from_json`] uses it for the same reason: a caller that only
/// wants an `i32` should not link a float parser. On a target with no
/// double-precision FPU that is not a nicety — see `docs/SIZE.md`, where
/// it is 93% of the binary. [`parse_fixed`] is the same trick extended to
/// the fraction digits, which is why they are located and returned rather
/// than merely stepped over.
pub(crate) fn number_syntax(s: &[u8], start: usize) -> Result<Syntax> {
    if s.is_empty() {
        return Err(Error::at(start, "expected a number"));
    }
    let mut i = 0usize;
    let neg = s.first() == Some(&b'-');
    if neg {
        i = 1;
    }
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
    // Equal to `int_end` means "no fraction", which is what an absent
    // `.` leaves behind.
    let mut frac_end = i;

    if s.get(i) == Some(&b'.') {
        i += 1;
        let f0 = i;
        while matches!(s.get(i), Some(c) if c.is_ascii_digit()) {
            i += 1;
        }
        if i == f0 {
            return Err(Error::at(start + i, "expected a digit after '.'"));
        }
        frac_end = i;
    }
    let mut has_exp = false;
    if matches!(s.get(i), Some(b'e' | b'E')) {
        has_exp = true;
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
    Ok(Syntax {
        neg,
        int_end,
        frac_end,
        has_exp,
    })
}

/// Syntax-check a number the way a skipped field needs, including the
/// range check that makes `1e400` an error rather than an infinity.
///
/// Only the two deserializers skip fields — [`crate::pull`] hands the
/// bytes back unconverted — so this exists only when they do.
#[cfg(feature = "serde")]
pub(crate) fn validate_number(input: &[u8], start: usize, end: usize) -> Result<()> {
    let s = input.get(start..end).unwrap_or(&[]);
    if number_syntax(s, start)?.is_float() {
        // Only floats can overflow, and only then must we actually parse.
        if let Ok(t) = core::str::from_utf8(s) {
            if t.parse::<f64>().map(|v| !v.is_finite()).unwrap_or(true) {
                return Err(Error::at(start, "number out of range"));
            }
        }
    }
    Ok(())
}

/// Validate and convert a number, following RFC 8259 exactly.
///
/// Integers that fit go to `i64`/`u64` so no precision is lost; anything
/// with a fraction or exponent goes through `str::parse`, which is
/// correctly rounded. `docs/RESULTS.md` records that `serde_json`'s own
/// float parser is not, by up to 2 ULP.
///
/// Reaching this function is what links `core`'s float parser. See
/// [`Integer`] for the way not to.
pub(crate) fn parse_number(input: &[u8], start: usize, end: usize) -> Result<Num> {
    let s = input.get(start..end).unwrap_or(&[]);
    if s.is_empty() {
        return Err(Error::at(start, "expected a number"));
    }
    let n = number_syntax(s, start)?;

    if !n.is_float() {
        let (neg, digits) = (n.neg, n.int(s));
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

    match core::str::from_utf8(s)
        .ok()
        .and_then(|t| t.parse::<f64>().ok())
    {
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

// =====================================================================
// Integers, without a float parser
// =====================================================================

mod sealed {
    pub trait Sealed {}
}

/// An integer type a JSON number can be read into directly.
///
/// # Why this exists
///
/// The general reading of a number has to be able to answer `f64`, so
/// reaching it instantiates `core`'s correctly-rounded float parser. On
/// `thumbv7em-none-eabihf` that is 22 KB — `POWER_OF_FIVE_128` alone is
/// 10 416 bytes, and there is no double-precision FPU, so the arithmetic
/// is soft too. A program reading integer fields off a sensor was paying
/// all of it.
///
/// Implementations here accumulate digits in `Self` and nothing wider,
/// so a program that only ever asks for an `i32` links no 64-bit
/// arithmetic and no float code at all. Measured: a `pull` binary goes
/// from 24 486 bytes to 2 102. See `docs/SIZE.md`.
///
/// # Width is a type, not a feature
///
/// Ask for the width you want at the call site. It is deliberately not a
/// Cargo feature: features are additive and global, so a crate anywhere
/// in the tree turning on a hypothetical `num-i16` would silently narrow
/// every other crate's integers. `flat` refused the same bargain for the
/// same reason — see the README.
///
/// Sealed: the digit loop is an implementation detail, and the set of
/// integer types is not open.
pub trait Integer: Copy + sealed::Sealed {
    /// Read `digits` as `Self`, or `None` if it does not fit.
    ///
    /// `digits` is ASCII `0`–`9`, already validated. `neg` is the sign.
    #[doc(hidden)]
    fn from_json(neg: bool, digits: &[u8]) -> Option<Self>;

    /// Read `int_digits.frac_digits` scaled by `10^scale` as `Self`.
    ///
    /// Both slices are validated ASCII `0`–`9`; `frac_digits` may be
    /// empty. Fraction digits past `scale` are dropped and a short
    /// fraction is zero-padded, so this is one loop over `scale` digits
    /// either way. `None` if the scaled value does not fit `Self`.
    #[doc(hidden)]
    fn from_json_fixed(
        neg: bool,
        int_digits: &[u8],
        frac_digits: &[u8],
        scale: u32,
    ) -> Option<Self>;
}

/// One digit into an accumulator, in the accumulator's own width.
///
/// A negative is built by subtracting rather than by negating at the
/// end: `-128` is a valid `i8` and `-(128i8)` is not representable to
/// negate. On an unsigned type that same subtraction is what rejects a
/// negative — `-0` subtracts nothing and survives, `-1` does not — so
/// signed and unsigned need only this one spelling.
macro_rules! step {
    ($t:ty, $v:expr, $d:expr, $neg:expr) => {{
        // Spelled as a `match` and not as `checked_mul(..).and_then(..)`,
        // which reads better and costs 8 bytes more in `pullfixed`: the
        // closure does not fold into the digit loop as cleanly.
        let x = ($d - b'0') as $t;
        match <$t>::checked_mul($v, 10) {
            Some(v) if $neg => v.checked_sub(x),
            Some(v) => v.checked_add(x),
            None => None,
        }
    }};
}

macro_rules! integer {
    ($($t:ty),*) => { $(
        impl sealed::Sealed for $t {}
        impl Integer for $t {
            #[inline]
            fn from_json(neg: bool, digits: &[u8]) -> Option<Self> {
                let mut v: $t = 0;
                for &d in digits {
                    v = step!($t, v, d, neg)?;
                }
                Some(v)
            }

            #[inline]
            fn from_json_fixed(
                neg: bool,
                int_digits: &[u8],
                frac_digits: &[u8],
                scale: u32,
            ) -> Option<Self> {
                let mut v: $t = 0;
                for &d in int_digits {
                    v = step!($t, v, d, neg)?;
                }
                // Exactly `scale` fraction digits, whatever the input
                // offered: `b'0'` pads a short one and the iterator
                // simply is not drained on a long one. Truncation
                // towards zero falls out of stopping early, on both
                // signs, because the magnitude is what is being built.
                let mut rest = frac_digits.iter();
                for _ in 0..scale {
                    let d = rest.next().copied().unwrap_or(b'0');
                    v = step!($t, v, d, neg)?;
                }
                Some(v)
            }
        }
    )* }
}

integer!(i8, i16, i32, i64, i128, isize, u8, u16, u32, u64, u128, usize);

/// Read `input[start..end]` as an integer of the caller's chosen width.
///
/// Rejects anything with a fraction or an exponent — `1.0` and `1e2` are
/// not integers here, even though their values are. That is the whole
/// point: deciding otherwise would mean parsing the float, which is the
/// cost being avoided. [`parse_number`] is the lenient reading.
pub(crate) fn parse_integer<T: Integer>(input: &[u8], start: usize, end: usize) -> Result<T> {
    let s = input.get(start..end).unwrap_or(&[]);
    let n = number_syntax(s, start)?;
    if n.is_float() {
        return Err(Error::at(start, "expected an integer, found a float"));
    }
    T::from_json(n.neg, n.int(s)).ok_or_else(|| Error::at(start, "integer out of range"))
}

/// Read `input[start..end]` as a fixed-point number scaled by `10^scale`.
///
/// `12.34` at `scale = 2` is `1234`; so is `12.349`, and so is
/// `12.3` — digits past the scale are dropped and a short fraction is
/// padded. Truncation, not rounding: see [`crate::pull::Raw::as_fixed`]
/// for why.
///
/// Rejects an exponent. `1.234e2` is representable at `scale = 1` and
/// folding the exponent in would mean a second, signed shift of the
/// decimal point and a second overflow story, for a spelling that a
/// device emitting fixed-point data does not use. [`parse_number`] is
/// the lenient reading, at the cost of the float parser.
pub(crate) fn parse_fixed<T: Integer>(
    input: &[u8],
    start: usize,
    end: usize,
    scale: u32,
) -> Result<T> {
    let s = input.get(start..end).unwrap_or(&[]);
    let n = number_syntax(s, start)?;
    if n.has_exp {
        return Err(Error::at(start, "fixed-point cannot take an exponent"));
    }
    T::from_json_fixed(n.neg, n.int(s), n.frac(s), scale)
        .ok_or_else(|| Error::at(start, "fixed-point value out of range"))
}
