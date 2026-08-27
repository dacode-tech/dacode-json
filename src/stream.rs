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

use core::fmt;
use serde::de::{self, DeserializeSeed, IntoDeserializer, MapAccess, SeqAccess, Visitor};

use crate::scan::{self, Scanner, StructuralIndex};

// =====================================================================
// Errors
// =====================================================================

/// A streaming parse or type error, with the byte offset where it was hit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Error {
    msg: String,
    /// Byte offset in the input, when the error came from the parser.
    pub offset: Option<usize>,
}

impl Error {
    fn at(offset: usize, msg: impl Into<String>) -> Self {
        Error {
            msg: msg.into(),
            offset: Some(offset),
        }
    }
    fn plain(msg: impl Into<String>) -> Self {
        Error {
            msg: msg.into(),
            offset: None,
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.offset {
            Some(o) => write!(f, "{} at byte {o}", self.msg),
            None => f.write_str(&self.msg),
        }
    }
}

impl std::error::Error for Error {}

impl de::Error for Error {
    fn custom<T: fmt::Display>(msg: T) -> Self {
        Error::plain(msg.to_string())
    }
}

type Result<T> = core::result::Result<T, Error>;

// =====================================================================
// Cursor
// =====================================================================

/// A position in the document, expressed as an offset into the structural
/// index rather than into the bytes.
///
/// `at` indexes the next *unconsumed* structural character. `last` is the
/// byte offset of the one just consumed, which is where the next value
/// begins looking (a value always follows `{`, `[`, `,` or `:`).
struct Cursor<'de> {
    input: &'de [u8],
    pos: &'de [u32],
    at: usize,
    last: usize,
    /// Byte offset just past the last completed value. Everything between
    /// it and the next structural character must be whitespace; without
    /// this, `{"a":"a" 123}` parses as `{"a":"a"}`.
    vend: usize,
}

impl<'de> Cursor<'de> {
    /// Byte offset of the next structural character, or end of input.
    #[inline]
    fn peek_pos(&self) -> usize {
        match self.pos.get(self.at) {
            Some(&p) => p as usize,
            None => self.input.len(),
        }
    }

    /// The next structural character itself, or `\0` past the end.
    #[inline]
    fn peek(&self) -> u8 {
        match self.pos.get(self.at) {
            Some(&p) => match self.input.get(p as usize) {
                Some(&b) => b,
                None => 0,
            },
            None => 0,
        }
    }

    /// Consume a closing delimiter, recording where the value ended.
    #[inline]
    fn close(&mut self) {
        self.vend = self.peek_pos() + 1;
        self.bump();
    }

    /// Only whitespace may sit between the last value and the next
    /// structural character.
    #[inline]
    fn gap_ok(&self) -> bool {
        crate::scalar::skip_ws(self.input, self.vend, self.input.len()) == self.peek_pos()
    }

    #[inline]
    fn bump(&mut self) {
        self.last = self.peek_pos();
        self.at += 1;
    }

    /// Where the value after the just-consumed delimiter starts.
    #[inline]
    fn value_start(&self) -> usize {
        crate::scalar::skip_ws(self.input, self.last + 1, self.input.len())
    }

    /// Byte at `i`, or `\0`.
    #[inline]
    fn byte(&self, i: usize) -> u8 {
        match self.input.get(i) {
            Some(&b) => b,
            None => 0,
        }
    }

    /// End of a scalar starting at `start`: the next structural character,
    /// with trailing whitespace trimmed.
    #[inline]
    fn scalar_end(&self, start: usize) -> usize {
        let mut e = self.peek_pos();
        while e > start && matches!(self.byte(e - 1), b' ' | b'\t' | b'\n' | b'\r') {
            e -= 1;
        }
        e
    }
}

// =====================================================================
// Scalars
// =====================================================================

/// A validated JSON number, in the widest form that holds it exactly.
enum Num {
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
fn number_syntax(s: &[u8], start: usize) -> Result<(bool, usize, usize, bool)> {
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
fn validate_number(input: &[u8], start: usize, end: usize) -> Result<()> {
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

fn parse_number(input: &[u8], start: usize, end: usize) -> Result<Num> {
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
fn expect_lit(input: &[u8], start: usize, end: usize, lit: &[u8]) -> Result<()> {
    if input.get(start..end) == Some(lit) {
        Ok(())
    } else {
        Err(Error::at(start, "invalid literal"))
    }
}

// =====================================================================
// Entry points
// =====================================================================

/// Parse `input` into `T` without building a node pool.
///
/// `T` must be [`serde::de::DeserializeOwned`]; see [`from_slice_with`]
/// for the borrowing form, which needs the index to outlive the result.
pub fn from_slice<T: serde::de::DeserializeOwned>(input: &[u8]) -> Result<T> {
    let mut idx = StructuralIndex::default();
    idx.reserve_for(input.len());
    scan::scan_into(Scanner::default(), input, &mut idx);
    from_parts(input, &idx)
}

/// Parse into a possibly-borrowing `T`, reusing a caller-owned index.
///
/// The index is the only scratch space, so a caller in a loop allocates
/// once:
///
/// ```
/// # use serde::Deserialize;
/// # #[derive(Deserialize)] struct Row<'a> { #[serde(borrow)] name: &'a str }
/// let mut idx = dacodec::stream::Index::default();
/// for src in [r#"{"name":"a"}"#, r#"{"name":"b"}"#] {
///     let row: Row<'_> = dacodec::stream::from_slice_with(&mut idx, src.as_bytes())?;
///     assert!(!row.name.is_empty());
/// }
/// # Ok::<(), dacodec::stream::Error>(())
/// ```
pub fn from_slice_with<'de, T: serde::Deserialize<'de>>(
    idx: &'de mut StructuralIndex,
    input: &'de [u8],
) -> Result<T> {
    idx.clear();
    idx.reserve_for(input.len());
    scan::scan_into(Scanner::default(), input, idx);
    from_parts(input, idx)
}

/// The scratch buffer [`from_slice_with`] reuses.
pub type Index = StructuralIndex;

fn from_parts<'de, T: serde::Deserialize<'de>>(
    input: &'de [u8],
    idx: &'de StructuralIndex,
) -> Result<T> {
    if input.len() > scan::MAX_INPUT_LEN {
        return Err(Error::plain("input exceeds the 2 GiB position limit"));
    }
    let mut c = Cursor {
        input,
        pos: idx.positions(),
        at: 0,
        last: 0,
        vend: 0,
    };
    // `value_start` reads from `last + 1`, so bias it for the root.
    let start = crate::scalar::skip_ws(input, 0, input.len());
    if start >= input.len() {
        return Err(Error::at(start, "empty document"));
    }
    let out = T::deserialize(ValueDe {
        c: &mut c,
        start,
        depth: 0,
    })?;

    // Nothing but whitespace may follow the root value.
    let tail = crate::scalar::skip_ws(input, c.vend, input.len());
    if tail < input.len() {
        return Err(Error::at(tail, "trailing characters after the document"));
    }
    Ok(out)
}

/// Nesting limit, matching `serde_json`'s default, so that deeply nested
/// input is rejected rather than overflowing the stack.
const MAX_DEPTH: usize = 128;

// =====================================================================
// The deserializer
// =====================================================================

/// A single value, positioned at byte `start`.
struct ValueDe<'a, 'de> {
    c: &'a mut Cursor<'de>,
    start: usize,
    depth: usize,
}

impl<'a, 'de> ValueDe<'a, 'de> {
    /// Consume the string beginning at `self.start`, returning its raw
    /// bytes (still escaped) and advancing past the closing quote.
    fn take_raw_str(&mut self) -> Result<&'de [u8]> {
        // The opener is the current index entry, the closer the next: the
        // scanner emits every unescaped quote.
        if self.c.peek() != b'"' || self.c.peek_pos() != self.start {
            return Err(Error::at(self.start, "expected a string"));
        }
        let open = self.c.peek_pos();
        self.c.bump();
        let close = self.c.peek_pos();
        if self.c.peek() != b'"' {
            return Err(Error::at(open, "unterminated string"));
        }
        self.c.vend = close + 1;
        self.c.bump();
        let raw = self
            .c
            .input
            .get(open + 1..close)
            .ok_or_else(|| Error::at(open, "unterminated string"))?;
        Ok(raw)
    }

}

macro_rules! forward_scalar {
    ($($m:ident)*) => {
        $(fn $m<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value> {
            self.deserialize_any(visitor)
        })*
    };
}

impl<'a, 'de> de::Deserializer<'de> for ValueDe<'a, 'de> {
    type Error = Error;

    fn deserialize_any<V: Visitor<'de>>(mut self, visitor: V) -> Result<V::Value> {
        if self.depth > MAX_DEPTH {
            return Err(Error::at(self.start, "nesting too deep"));
        }
        match self.c.byte(self.start) {
            b'{' => {
                if self.c.peek_pos() != self.start {
                    return Err(Error::at(self.start, "misaligned object"));
                }
                self.c.bump();
                let depth = self.depth + 1;
                visitor.visit_map(ObjectAccess {
                    c: self.c,
                    first: true,
                    depth,
                    done: false,
                })
            }
            b'[' => {
                if self.c.peek_pos() != self.start {
                    return Err(Error::at(self.start, "misaligned array"));
                }
                self.c.bump();
                let depth = self.depth + 1;
                visitor.visit_seq(ArrayAccess {
                    c: self.c,
                    first: true,
                    depth,
                    done: false,
                })
            }
            b'"' => {
                let raw = self.take_raw_str()?;
                match crate::unescape::unescape_checked(raw) {
                    Some(std::borrow::Cow::Borrowed(s)) => visitor.visit_borrowed_str(s),
                    Some(std::borrow::Cow::Owned(s)) => visitor.visit_string(s),
                    None => Err(Error::at(self.start, "invalid escape or UTF-8 in string")),
                }
            }
            b't' => {
                let e = self.c.scalar_end(self.start);
                expect_lit(self.c.input, self.start, e, b"true")?;
                self.c.vend = e;
                visitor.visit_bool(true)
            }
            b'f' => {
                let e = self.c.scalar_end(self.start);
                expect_lit(self.c.input, self.start, e, b"false")?;
                self.c.vend = e;
                visitor.visit_bool(false)
            }
            b'n' => {
                let e = self.c.scalar_end(self.start);
                expect_lit(self.c.input, self.start, e, b"null")?;
                self.c.vend = e;
                visitor.visit_unit()
            }
            _ => {
                let e = self.c.scalar_end(self.start);
                self.c.vend = e;
                match parse_number(self.c.input, self.start, e)? {
                    Num::I(v) => visitor.visit_i64(v),
                    Num::U(v) => visitor.visit_u64(v),
                    Num::F(v) => visitor.visit_f64(v),
                }
            }
        }
    }

    fn deserialize_option<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value> {
        if self.c.byte(self.start) == b'n' {
            let e = self.c.scalar_end(self.start);
            expect_lit(self.c.input, self.start, e, b"null")?;
            self.c.vend = e;
            visitor.visit_none()
        } else {
            visitor.visit_some(self)
        }
    }

    /// Skipping validates but does not convert.
    ///
    /// A plain depth-count over the index would be faster still, but it
    /// would accept documents `serde_json` rejects whenever the malformed
    /// part sits in a field the target type ignores — a correctness
    /// regression dressed up as an optimisation. The fuzzer catches that
    /// within a few hundred thousand mutations.
    ///
    /// So every byte is still checked; what is skipped is the *work after
    /// checking*: no float is parsed, no `String` is built, no escape is
    /// expanded. Containers recurse through the normal accessors, so
    /// their contents get the same treatment.
    fn deserialize_ignored_any<V: Visitor<'de>>(mut self, visitor: V) -> Result<V::Value> {
        match self.c.byte(self.start) {
            // Recurse: the accessors call back here for each child.
            b'{' | b'[' => self.deserialize_any(visitor),
            b'"' => {
                // `take_raw_str` already rejects control bytes; this adds
                // the escape and UTF-8 checks without building a `Cow`.
                let raw = self.take_raw_str()?;
                if crate::unescape::unescape_checked(raw).is_none() {
                    return Err(Error::at(self.start, "invalid escape or UTF-8 in string"));
                }
                visitor.visit_unit()
            }
            b't' => {
                let e = self.c.scalar_end(self.start);
                expect_lit(self.c.input, self.start, e, b"true")?;
                self.c.vend = e;
                visitor.visit_unit()
            }
            b'f' => {
                let e = self.c.scalar_end(self.start);
                expect_lit(self.c.input, self.start, e, b"false")?;
                self.c.vend = e;
                visitor.visit_unit()
            }
            b'n' => {
                let e = self.c.scalar_end(self.start);
                expect_lit(self.c.input, self.start, e, b"null")?;
                self.c.vend = e;
                visitor.visit_unit()
            }
            _ => {
                let e = self.c.scalar_end(self.start);
                validate_number(self.c.input, self.start, e)?;
                self.c.vend = e;
                visitor.visit_unit()
            }
        }
    }

    fn deserialize_unit_struct<V: Visitor<'de>>(
        self,
        _n: &'static str,
        visitor: V,
    ) -> Result<V::Value> {
        visitor.visit_unit()
    }

    fn deserialize_newtype_struct<V: Visitor<'de>>(
        self,
        _n: &'static str,
        visitor: V,
    ) -> Result<V::Value> {
        visitor.visit_newtype_struct(self)
    }

    fn deserialize_enum<V: Visitor<'de>>(
        mut self,
        _n: &'static str,
        _v: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value> {
        match self.c.byte(self.start) {
            b'"' => {
                let raw = self.take_raw_str()?;
                let name = crate::unescape::unescape_checked(raw)
                    .ok_or_else(|| Error::at(self.start, "invalid variant name"))?;
                visitor.visit_enum(UnitVariant { name })
            }
            b'{' => {
                self.c.bump();
                let depth = self.depth + 1;
                visitor.visit_enum(VariantAccess { c: self.c, depth })
            }
            _ => Err(Error::at(self.start, "expected an enum")),
        }
    }

    forward_scalar! {
        deserialize_bool deserialize_i8 deserialize_i16 deserialize_i32
        deserialize_i64 deserialize_i128 deserialize_u8 deserialize_u16
        deserialize_u32 deserialize_u64 deserialize_u128 deserialize_f32
        deserialize_f64 deserialize_char deserialize_str deserialize_string
        deserialize_bytes deserialize_byte_buf deserialize_unit
        deserialize_seq deserialize_map deserialize_identifier
    }

    fn deserialize_tuple<V: Visitor<'de>>(self, _len: usize, visitor: V) -> Result<V::Value> {
        self.deserialize_any(visitor)
    }

    fn deserialize_tuple_struct<V: Visitor<'de>>(
        self,
        _n: &'static str,
        _len: usize,
        visitor: V,
    ) -> Result<V::Value> {
        self.deserialize_any(visitor)
    }

    fn deserialize_struct<V: Visitor<'de>>(
        self,
        _n: &'static str,
        _f: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value> {
        self.deserialize_any(visitor)
    }
}

// =====================================================================
// Containers
// =====================================================================

struct ObjectAccess<'a, 'de> {
    c: &'a mut Cursor<'de>,
    first: bool,
    depth: usize,
    done: bool,
}

impl<'a, 'de> MapAccess<'de> for ObjectAccess<'a, 'de> {
    type Error = Error;

    fn next_key_seed<K: DeserializeSeed<'de>>(&mut self, seed: K) -> Result<Option<K::Value>> {
        if self.done {
            return Ok(None);
        }
        // A scalar is not in the index, so `peek` alone cannot tell an
        // empty container from one holding a bare token: for `{fals}` the
        // next index entry is the `}`. Only treat the closer as the end
        // when nothing but whitespace precedes it.
        if self.first {
            if self.c.peek() == b'}' && self.c.value_start() == self.c.peek_pos() {
                self.c.close();
                self.done = true;
                return Ok(None);
            }
        } else {
            if !self.c.gap_ok() {
                return Err(Error::at(self.c.vend, "unexpected characters after value"));
            }
            match self.c.peek() {
                b'}' => {
                    self.c.close();
                    self.done = true;
                    return Ok(None);
                }
                b',' => self.c.bump(),
                other => {
                    return Err(Error::at(
                        self.c.peek_pos(),
                        format!("expected ',' or '}}' in object, found {:?}", other as char),
                    ))
                }
            }
        }
        self.first = false;
        // The key's opening quote must be the very next non-whitespace
        // byte. Checking `peek()` alone is not enough: a quote is in the
        // index but junk before it is not, so `{.x"a":1}` would slip
        // through. The fuzzer found exactly that.
        if self.c.peek() != b'"' || self.c.value_start() != self.c.peek_pos() {
            return Err(Error::at(self.c.value_start(), "expected an object key"));
        }
        let open = self.c.peek_pos();
        self.c.bump();
        let close = self.c.peek_pos();
        self.c.bump();
        let raw = self
            .c
            .input
            .get(open + 1..close)
            .ok_or_else(|| Error::at(open, "unterminated key"))?;
        self.c.vend = close + 1;
        if !self.c.gap_ok() {
            return Err(Error::at(self.c.vend, "unexpected characters after key"));
        }
        if self.c.peek() != b':' {
            return Err(Error::at(self.c.peek_pos(), "expected ':' after object key"));
        }
        self.c.bump();

        let key = crate::unescape::unescape_checked(raw)
            .ok_or_else(|| Error::at(open, "invalid escape or UTF-8 in key"))?;
        match key {
            std::borrow::Cow::Borrowed(s) => seed.deserialize(BorrowedStr(s)).map(Some),
            std::borrow::Cow::Owned(s) => seed.deserialize(s.into_deserializer()).map(Some),
        }
    }

    fn next_value_seed<V: DeserializeSeed<'de>>(&mut self, seed: V) -> Result<V::Value> {
        let start = self.c.value_start();
        let depth = self.depth;
        seed.deserialize(ValueDe {
            c: self.c,
            start,
            depth,
        })
    }
}

struct ArrayAccess<'a, 'de> {
    c: &'a mut Cursor<'de>,
    first: bool,
    depth: usize,
    done: bool,
}

impl<'a, 'de> SeqAccess<'de> for ArrayAccess<'a, 'de> {
    type Error = Error;

    fn next_element_seed<T: DeserializeSeed<'de>>(&mut self, seed: T) -> Result<Option<T::Value>> {
        if self.done {
            return Ok(None);
        }
        // See the note in `ObjectAccess`: `[fals]` has no index entry for
        // the token, so the next entry is the `]`.
        if self.first {
            if self.c.peek() == b']' && self.c.value_start() == self.c.peek_pos() {
                self.c.close();
                self.done = true;
                return Ok(None);
            }
        } else {
            if !self.c.gap_ok() {
                return Err(Error::at(self.c.vend, "unexpected characters after value"));
            }
            match self.c.peek() {
                b']' => {
                    self.c.close();
                    self.done = true;
                    return Ok(None);
                }
                b',' => self.c.bump(),
                other => {
                    return Err(Error::at(
                        self.c.peek_pos(),
                        format!("expected ',' or ']' in array, found {:?}", other as char),
                    ))
                }
            }
        }
        self.first = false;
        let start = self.c.value_start();
        let depth = self.depth;
        seed.deserialize(ValueDe {
            c: self.c,
            start,
            depth,
        })
        .map(Some)
    }
}

// =====================================================================
// Keys and enums
// =====================================================================

struct BorrowedStr<'de>(&'de str);

impl<'de> de::Deserializer<'de> for BorrowedStr<'de> {
    type Error = Error;

    fn deserialize_any<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value> {
        visitor.visit_borrowed_str(self.0)
    }

    fn deserialize_enum<V: Visitor<'de>>(
        self,
        _n: &'static str,
        _v: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value> {
        visitor.visit_enum(UnitVariant {
            name: std::borrow::Cow::Borrowed(self.0),
        })
    }

    serde::forward_to_deserialize_any! {
        bool i8 i16 i32 i64 i128 u8 u16 u32 u64 u128 f32 f64 char str string
        bytes byte_buf option unit unit_struct newtype_struct seq tuple
        tuple_struct map struct identifier ignored_any
    }
}

struct UnitVariant<'de> {
    name: std::borrow::Cow<'de, str>,
}

impl<'de> de::EnumAccess<'de> for UnitVariant<'de> {
    type Error = Error;
    type Variant = UnitPayload;

    fn variant_seed<V: DeserializeSeed<'de>>(self, seed: V) -> Result<(V::Value, UnitPayload)> {
        let v = match self.name {
            std::borrow::Cow::Borrowed(s) => seed.deserialize(BorrowedStr(s))?,
            std::borrow::Cow::Owned(s) => seed.deserialize(s.into_deserializer())?,
        };
        Ok((v, UnitPayload))
    }
}

struct UnitPayload;

impl<'de> de::VariantAccess<'de> for UnitPayload {
    type Error = Error;

    fn unit_variant(self) -> Result<()> {
        Ok(())
    }
    fn newtype_variant_seed<T: DeserializeSeed<'de>>(self, _s: T) -> Result<T::Value> {
        Err(Error::plain("expected a unit variant"))
    }
    fn tuple_variant<V: Visitor<'de>>(self, _l: usize, _v: V) -> Result<V::Value> {
        Err(Error::plain("expected a unit variant"))
    }
    fn struct_variant<V: Visitor<'de>>(self, _f: &'static [&'static str], _v: V) -> Result<V::Value> {
        Err(Error::plain("expected a unit variant"))
    }
}

/// `{"Variant": payload}` — exactly one pair.
struct VariantAccess<'a, 'de> {
    c: &'a mut Cursor<'de>,
    depth: usize,
}

impl<'a, 'de> de::EnumAccess<'de> for VariantAccess<'a, 'de> {
    type Error = Error;
    type Variant = Payload<'a, 'de>;

    fn variant_seed<V: DeserializeSeed<'de>>(self, seed: V) -> Result<(V::Value, Self::Variant)> {
        if self.c.peek() != b'"' || self.c.value_start() != self.c.peek_pos() {
            return Err(Error::at(self.c.value_start(), "expected a variant name"));
        }
        let open = self.c.peek_pos();
        self.c.bump();
        let close = self.c.peek_pos();
        self.c.bump();
        let raw = self
            .c
            .input
            .get(open + 1..close)
            .ok_or_else(|| Error::at(open, "unterminated variant name"))?;
        if self.c.peek() != b':' {
            return Err(Error::at(self.c.peek_pos(), "expected ':' after variant"));
        }
        self.c.bump();
        let name = crate::unescape::unescape_checked(raw)
            .ok_or_else(|| Error::at(open, "invalid variant name"))?;
        let v = match name {
            std::borrow::Cow::Borrowed(s) => seed.deserialize(BorrowedStr(s))?,
            std::borrow::Cow::Owned(s) => seed.deserialize(s.into_deserializer())?,
        };
        let depth = self.depth;
        Ok((v, Payload { c: self.c, depth }))
    }
}

struct Payload<'a, 'de> {
    c: &'a mut Cursor<'de>,
    depth: usize,
}

impl<'a, 'de> Payload<'a, 'de> {
    /// Consume the trailing `}` of `{"Variant": payload}`.
    fn close(self) -> Result<()> {
        if self.c.peek() != b'}' {
            return Err(Error::at(
                self.c.peek_pos(),
                "enum object must have exactly one key",
            ));
        }
        self.c.bump();
        Ok(())
    }
}

impl<'a, 'de> de::VariantAccess<'de> for Payload<'a, 'de> {
    type Error = Error;

    fn unit_variant(self) -> Result<()> {
        Err(Error::plain("expected a newtype, tuple or struct variant"))
    }

    fn newtype_variant_seed<T: DeserializeSeed<'de>>(self, seed: T) -> Result<T::Value> {
        let start = self.c.value_start();
        let depth = self.depth;
        let v = seed.deserialize(ValueDe {
            c: self.c,
            start,
            depth,
        })?;
        self.close()?;
        Ok(v)
    }

    fn tuple_variant<V: Visitor<'de>>(self, _len: usize, visitor: V) -> Result<V::Value> {
        let start = self.c.value_start();
        let depth = self.depth;
        let v = de::Deserializer::deserialize_any(
            ValueDe {
                c: self.c,
                start,
                depth,
            },
            visitor,
        )?;
        self.close()?;
        Ok(v)
    }

    fn struct_variant<V: Visitor<'de>>(
        self,
        _f: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value> {
        let start = self.c.value_start();
        let depth = self.depth;
        let v = de::Deserializer::deserialize_any(
            ValueDe {
                c: self.c,
                start,
                depth,
            },
            visitor,
        )?;
        self.close()?;
        Ok(v)
    }
}
