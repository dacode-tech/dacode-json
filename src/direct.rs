//! A no-index streaming deserializer: the `serde_json` design, our decoding.
//!
//! [`crate::stream`] keeps Stage 1 — the SIMD structural index — and walks
//! it. That makes whitespace free and skipping cheap, but it is a separate
//! pass over the whole document before the first value is read, worth
//! about 13% of deserialization time, and it holds an index roughly twice
//! the size of the input.
//!
//! `serde_json` pays neither, because it finds delimiters with scalar code
//! as it goes and never pays for the ones it skips. On "parse a struct
//! once and discard the text" it wins, and it is 10% ahead of
//! [`crate::stream`] on exactly that.
//!
//! This module tests whether the index is the reason, by removing it. Same
//! number parsing, same string validation, same serde surface — one byte
//! cursor and no precomputed structure.
//!
//! # What replaces the index
//!
//! Nothing, for whitespace: it is skipped a byte at a time, which is what
//! `serde_json` does. For strings the terminator scan is done eight bytes
//! at a time with SWAR (SIMD within a register), which needs no target
//! feature detection and no `unsafe`:
//!
//! ```text
//! chunk ^ broadcast('"')  →  a zero byte wherever a quote sits
//! (v - 0x01..) & !v & 0x80..  →  high bit set on each zero byte
//! ```
//!
//! One `trailing_zeros` then gives the offset. `serde_json` uses a
//! 256-entry lookup table byte at a time for the same job.
//!
//! # Status
//!
//! A prototype, kept because it is measured in `docs/RESULTS.md`. It is
//! held to the same conformance and fuzzing bar as everything else.

use serde::de::{self, DeserializeSeed, IntoDeserializer, MapAccess, SeqAccess, Visitor};

use alloc::borrow::Cow;

use crate::stream::{expect_lit, parse_number, validate_number, Error, Num};

type Result<T> = core::result::Result<T, Error>;

fn err(at: usize, msg: &'static str) -> Error {
    // Reuse the streaming error so both paths report identically.
    Error::from_parts(at, msg)
}

// =====================================================================
// SWAR scanning
// =====================================================================

const LO: u64 = 0x0101_0101_0101_0101;
const HI: u64 = 0x8080_8080_8080_8080;

/// High bit set on every zero byte of `v`.
#[inline]
fn zero_bytes(v: u64) -> u64 {
    v.wrapping_sub(LO) & !v & HI
}


// =====================================================================
// Cursor
// =====================================================================

struct Cursor<'de> {
    input: &'de [u8],
    pos: usize,
    /// The whole input was verified to be ASCII before parsing started.
    ///
    /// When set, a string with no escape and no control byte needs no
    /// UTF-8 check at all: every byte in the buffer is below 0x80, so
    /// every subslice of it is valid UTF-8 by construction.
    ascii: bool,
}

impl<'de> Cursor<'de> {
    #[inline]
    fn peek(&self) -> u8 {
        match self.input.get(self.pos) {
            Some(&b) => b,
            None => 0,
        }
    }

    #[inline]
    fn skip_ws(&mut self) {
        while let Some(&b) = self.input.get(self.pos) {
            if b == b' ' || b == b'\t' || b == b'\n' || b == b'\r' {
                self.pos += 1;
            } else {
                break;
            }
        }
    }

    #[inline]
    fn eat(&mut self, b: u8) -> bool {
        if self.peek() == b {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    /// Consume a string, `self.pos` sitting on the opening quote.
    ///
    /// Returns the raw bytes between the quotes, still escaped, and
    /// whether the content is *simple*: no backslash, no byte below 0x20,
    /// no byte at or above 0x80. A simple string needs no unescaping and
    /// is ASCII, so it is valid UTF-8 by construction.
    ///
    /// Finding the terminator and deciding simplicity happen in the same
    /// pass. Doing them separately meant every string was scanned twice —
    /// once here, once in `unescape_checked` — and on this corpus strings
    /// average 4.8 bytes, so a second traversal is mostly call overhead.
    fn scan_string(&mut self) -> Result<(&'de [u8], bool)> {
        let open = self.pos;
        if !self.eat(b'"') {
            return Err(err(open, "expected a string"));
        }
        let start = self.pos;
        let n = self.input.len();
        let mut i = start;
        let mut simple = true;
        const QUOTES: u64 = LO * b'"' as u64;
        const SLASHES: u64 = LO * b'\\' as u64;

        loop {
            while i + 8 <= n {
                let Some(arr) = self
                    .input
                    .get(i..i + 8)
                    .and_then(|s| <[u8; 8]>::try_from(s).ok())
                else {
                    break;
                };
                let chunk = u64::from_le_bytes(arr);
                let term = zero_bytes(chunk ^ QUOTES) | zero_bytes(chunk ^ SLASHES);
                // High bit set on any byte >= 0x80, or any byte < 0x20.
                // The second test borrows incorrectly in the presence of
                // high bytes, which does not matter: either way the byte
                // is not simple.
                let odd = (chunk & HI) | (chunk.wrapping_sub(LO * 0x20) & !chunk & HI);

                if term == 0 {
                    simple &= odd == 0;
                    i += 8;
                    continue;
                }
                let bit = term.trailing_zeros();
                let at = i + (bit / 8) as usize;
                // Only bytes before the terminator count.
                let before = if bit == 0 { 0 } else { (1u64 << bit) - 1 };
                simple &= odd & before == 0;

                if self.input.get(at) == Some(&b'"') {
                    self.pos = at + 1;
                    let raw = self
                        .input
                        .get(start..at)
                        .ok_or_else(|| err(open, "unterminated string"))?;
                    return Ok((raw, simple));
                }
                // A backslash: skip the escaped byte and keep going.
                simple = false;
                i = at + 2;
                break;
            }
            if i + 8 <= n {
                continue;
            }
            // Tail, under eight bytes from the end.
            while let Some(&b) = self.input.get(i) {
                match b {
                    b'"' => {
                        self.pos = i + 1;
                        let raw = self
                            .input
                            .get(start..i)
                            .ok_or_else(|| err(open, "unterminated string"))?;
                        return Ok((raw, simple));
                    }
                    b'\\' => {
                        simple = false;
                        i += 2;
                    }
                    _ => {
                        if !(0x20..0x80).contains(&b) {
                            simple = false;
                        }
                        i += 1;
                    }
                }
            }
            return Err(err(open, "unterminated string"));
        }
    }

    /// A `simple` string as `&str`.
    ///
    /// In ASCII mode this is free: [`from_slice_ascii`] verified every byte
    /// of the buffer is below 0x80 before parsing began, and `raw` is a
    /// subslice of that buffer, so it is valid UTF-8 by construction.
    /// Otherwise `simple` only promises no escape and no control byte, so
    /// the UTF-8 check still runs.
    #[inline]
    fn ascii_str<'s>(&self, raw: &'s [u8]) -> Option<&'s str> {
        if self.ascii {
            // SAFETY: `from_slice_ascii` returned early unless `is_ascii`
            // held for the whole input, and `raw` came from `scan_string`,
            // which only ever yields subslices of `self.input`. Every byte
            // is therefore below 0x80, and all-ASCII is valid UTF-8. The
            // invariant is established and checked in this module, three
            // functions away, and nowhere else can set `ascii`.
            return Some(unsafe { core::str::from_utf8_unchecked(raw) });
        }
        core::str::from_utf8(raw).ok()
    }

    /// End of the scalar starting at `self.pos`: the next delimiter,
    /// whitespace or end of input.
    #[inline]
    fn scalar_end(&self) -> usize {
        let mut i = self.pos;
        while let Some(&b) = self.input.get(i) {
            if matches!(b, b',' | b'}' | b']' | b' ' | b'\t' | b'\n' | b'\r') {
                break;
            }
            i += 1;
        }
        i
    }
}

// =====================================================================
// Entry points
// =====================================================================

/// Nesting limit, as `serde_json` has, so deep input errors rather than
/// overflowing the stack.
const MAX_DEPTH: usize = 128;

/// Parse `input` into an owned `T`, with no structural index.
pub fn from_slice<T: serde::de::DeserializeOwned>(input: &[u8]) -> Result<T> {
    from_slice_borrowed(input)
}

/// True when no byte has its high bit set.
///
/// One pass, eight bytes at a time, no branches: OR everything together
/// and look at the result once. This is the check that lets the ASCII
/// parser skip every per-string UTF-8 validation.
#[must_use]
pub fn is_ascii(input: &[u8]) -> bool {
    let mut acc = 0u64;
    let mut i = 0usize;
    while i + 8 <= input.len() {
        let Some(arr) = input
            .get(i..i + 8)
            .and_then(|s| <[u8; 8]>::try_from(s).ok())
        else {
            break;
        };
        acc |= u64::from_le_bytes(arr);
        i += 8;
    }
    let mut tail = 0u8;
    while let Some(&b) = input.get(i) {
        tail |= b;
        i += 1;
    }
    (acc & HI) == 0 && (tail & 0x80) == 0
}

/// Parse input that is known to be ASCII, checking that claim first.
///
/// Payloads that are ASCII by construction — machine-generated logs,
/// identifiers, base64, numeric telemetry — pay for UTF-8 validation they
/// cannot fail. This verifies the whole buffer is ASCII once, in a single
/// vectorisable pass, and then skips the per-string check entirely.
///
/// Returns [`Error`] if the input is not ASCII; it does not fall back,
/// because silently taking a slower path would hide the fact that the
/// assumption was wrong. Use [`from_slice`] for arbitrary UTF-8.
///
/// Validation is otherwise identical: escapes, control bytes, numbers and
/// structure are all checked exactly as [`from_slice`] checks them.
pub fn from_slice_ascii<T: serde::de::DeserializeOwned>(input: &[u8]) -> Result<T> {
    from_slice_ascii_borrowed(input)
}

/// [`from_slice_ascii`], borrowing from the input.
pub fn from_slice_ascii_borrowed<'de, T: serde::Deserialize<'de>>(
    input: &'de [u8],
) -> Result<T> {
    if !is_ascii(input) {
        return Err(err(0, "input is not ASCII"));
    }
    run(input, true)
}

/// Parse into a possibly-borrowing `T`.
///
/// Nothing needs to outlive the call except the input itself — there is no
/// scratch buffer at all — so unlike [`crate::stream::from_slice_with`]
/// this needs no caller-owned state.
pub fn from_slice_borrowed<'de, T: serde::Deserialize<'de>>(input: &'de [u8]) -> Result<T> {
    run(input, false)
}

fn run<'de, T: serde::Deserialize<'de>>(input: &'de [u8], ascii: bool) -> Result<T> {
    let mut c = Cursor {
        input,
        pos: 0,
        ascii,
    };
    c.skip_ws();
    if c.pos >= input.len() {
        return Err(err(c.pos, "empty document"));
    }
    let out = T::deserialize(ValueDe {
        c: &mut c,
        depth: 0,
    })?;
    c.skip_ws();
    if c.pos < input.len() {
        return Err(err(c.pos, "trailing characters after the document"));
    }
    Ok(out)
}

// =====================================================================
// Deserializer
// =====================================================================

struct ValueDe<'a, 'de> {
    c: &'a mut Cursor<'de>,
    depth: usize,
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

    fn deserialize_any<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value> {
        if self.depth > MAX_DEPTH {
            return Err(err(self.c.pos, "nesting too deep"));
        }
        self.c.skip_ws();
        let at = self.c.pos;
        match self.c.peek() {
            b'{' => {
                self.c.pos += 1;
                let depth = self.depth + 1;
                visitor.visit_map(ObjectAccess {
                    c: self.c,
                    first: true,
                    depth,
                })
            }
            b'[' => {
                self.c.pos += 1;
                let depth = self.depth + 1;
                visitor.visit_seq(ArrayAccess {
                    c: self.c,
                    first: true,
                    depth,
                })
            }
            b'"' => {
                let (raw, simple) = self.c.scan_string()?;
                if simple {
                    if let Some(s) = self.c.ascii_str(raw) {
                        return visitor.visit_borrowed_str(s);
                    }
                    return Err(err(at, "invalid UTF-8 in string"));
                }
                match crate::unescape::unescape_checked(raw) {
                    Some(Cow::Borrowed(s)) => visitor.visit_borrowed_str(s),
                    Some(Cow::Owned(s)) => visitor.visit_string(s),
                    None => Err(err(at, "invalid escape or UTF-8 in string")),
                }
            }
            b't' => {
                let e = self.c.scalar_end();
                expect_lit(self.c.input, at, e, b"true")?;
                self.c.pos = e;
                visitor.visit_bool(true)
            }
            b'f' => {
                let e = self.c.scalar_end();
                expect_lit(self.c.input, at, e, b"false")?;
                self.c.pos = e;
                visitor.visit_bool(false)
            }
            b'n' => {
                let e = self.c.scalar_end();
                expect_lit(self.c.input, at, e, b"null")?;
                self.c.pos = e;
                visitor.visit_unit()
            }
            _ => {
                let e = self.c.scalar_end();
                let n = parse_number(self.c.input, at, e)?;
                self.c.pos = e;
                match n {
                    Num::I(v) => visitor.visit_i64(v),
                    Num::U(v) => visitor.visit_u64(v),
                    Num::F(v) => visitor.visit_f64(v),
                }
            }
        }
    }

    fn deserialize_option<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value> {
        self.c.skip_ws();
        if self.c.peek() == b'n' {
            let at = self.c.pos;
            let e = self.c.scalar_end();
            expect_lit(self.c.input, at, e, b"null")?;
            self.c.pos = e;
            visitor.visit_none()
        } else {
            visitor.visit_some(self)
        }
    }

    /// Validate, convert nothing. See the note in [`crate::stream`]: a
    /// skip that does not check would accept documents `serde_json`
    /// rejects whenever the fault is in an ignored field.
    fn deserialize_ignored_any<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value> {
        self.c.skip_ws();
        let at = self.c.pos;
        match self.c.peek() {
            b'{' | b'[' => self.deserialize_any(visitor),
            b'"' => {
                let (raw, simple) = self.c.scan_string()?;
                if !simple && crate::unescape::unescape_checked(raw).is_none() {
                    return Err(err(at, "invalid escape or UTF-8 in string"));
                }
                visitor.visit_unit()
            }
            b't' => {
                let e = self.c.scalar_end();
                expect_lit(self.c.input, at, e, b"true")?;
                self.c.pos = e;
                visitor.visit_unit()
            }
            b'f' => {
                let e = self.c.scalar_end();
                expect_lit(self.c.input, at, e, b"false")?;
                self.c.pos = e;
                visitor.visit_unit()
            }
            b'n' => {
                let e = self.c.scalar_end();
                expect_lit(self.c.input, at, e, b"null")?;
                self.c.pos = e;
                visitor.visit_unit()
            }
            _ => {
                let e = self.c.scalar_end();
                validate_number(self.c.input, at, e)?;
                self.c.pos = e;
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
        self,
        _n: &'static str,
        _v: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value> {
        self.c.skip_ws();
        let at = self.c.pos;
        match self.c.peek() {
            b'"' => {
                let (raw, _) = self.c.scan_string()?;
                let name = crate::unescape::unescape_checked(raw)
                    .ok_or_else(|| err(at, "invalid variant name"))?;
                visitor.visit_enum(UnitVariant { name })
            }
            b'{' => {
                self.c.pos += 1;
                let depth = self.depth + 1;
                visitor.visit_enum(VariantAccess { c: self.c, depth })
            }
            _ => Err(err(at, "expected an enum")),
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
}

impl<'a, 'de> MapAccess<'de> for ObjectAccess<'a, 'de> {
    type Error = Error;

    fn next_key_seed<K: DeserializeSeed<'de>>(&mut self, seed: K) -> Result<Option<K::Value>> {
        self.c.skip_ws();
        if self.c.eat(b'}') {
            return Ok(None);
        }
        if !self.first {
            if !self.c.eat(b',') {
                return Err(err(self.c.pos, "expected ',' or '}' in object"));
            }
            self.c.skip_ws();
            // `{"a":1,}` is not valid JSON.
            if self.c.peek() == b'}' {
                return Err(err(self.c.pos, "trailing comma in object"));
            }
        }
        self.first = false;

        let at = self.c.pos;
        if self.c.peek() != b'"' {
            return Err(err(at, "expected an object key"));
        }
        let (raw, simple) = self.c.scan_string()?;
        self.c.skip_ws();
        if !self.c.eat(b':') {
            return Err(err(self.c.pos, "expected ':' after object key"));
        }

        // Keys are matched against the struct's field names by comparing
        // bytes, and serde's generated field visitor implements
        // `visit_bytes`. So a simple key needs no UTF-8 validation at all:
        // hand over the bytes and let the target decide. `String` keys
        // still get validated, because serde's `String` visitor calls
        // `str::from_utf8` in its own `visit_bytes`.
        if simple {
            return seed.deserialize(BorrowedBytes(raw)).map(Some);
        }
        let key = crate::unescape::unescape_checked(raw)
            .ok_or_else(|| err(at, "invalid escape or UTF-8 in key"))?;
        match key {
            Cow::Borrowed(s) => seed.deserialize(BorrowedStr(s)).map(Some),
            Cow::Owned(s) => seed.deserialize(s.into_deserializer()).map(Some),
        }
    }

    fn next_value_seed<V: DeserializeSeed<'de>>(&mut self, seed: V) -> Result<V::Value> {
        let depth = self.depth;
        seed.deserialize(ValueDe { c: self.c, depth })
    }
}

struct ArrayAccess<'a, 'de> {
    c: &'a mut Cursor<'de>,
    first: bool,
    depth: usize,
}

impl<'a, 'de> SeqAccess<'de> for ArrayAccess<'a, 'de> {
    type Error = Error;

    fn next_element_seed<T: DeserializeSeed<'de>>(&mut self, seed: T) -> Result<Option<T::Value>> {
        self.c.skip_ws();
        if self.c.eat(b']') {
            return Ok(None);
        }
        if !self.first {
            if !self.c.eat(b',') {
                return Err(err(self.c.pos, "expected ',' or ']' in array"));
            }
            self.c.skip_ws();
            if self.c.peek() == b']' {
                return Err(err(self.c.pos, "trailing comma in array"));
            }
        }
        self.first = false;
        let depth = self.depth;
        seed.deserialize(ValueDe { c: self.c, depth }).map(Some)
    }
}

// =====================================================================
// Keys and enums
// =====================================================================

/// A key handed over as bytes, skipping UTF-8 validation.
///
/// Sound and safe: whoever consumes it decides whether it needs to be
/// `str`. Struct field matching does not, which is the common case.
struct BorrowedBytes<'de>(&'de [u8]);

impl<'de> de::Deserializer<'de> for BorrowedBytes<'de> {
    type Error = Error;

    fn deserialize_any<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value> {
        visitor.visit_borrowed_bytes(self.0)
    }

    fn deserialize_str<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value> {
        match core::str::from_utf8(self.0) {
            Ok(s) => visitor.visit_borrowed_str(s),
            Err(_) => Err(Error::from_parts(0, "invalid UTF-8 in key")),
        }
    }

    fn deserialize_string<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value> {
        self.deserialize_str(visitor)
    }

    fn deserialize_identifier<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value> {
        visitor.visit_borrowed_bytes(self.0)
    }

    fn deserialize_enum<V: Visitor<'de>>(
        self,
        n: &'static str,
        v: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value> {
        match core::str::from_utf8(self.0) {
            Ok(s) => BorrowedStr(s).deserialize_enum(n, v, visitor),
            Err(_) => Err(Error::from_parts(0, "invalid UTF-8 in key")),
        }
    }

    serde::forward_to_deserialize_any! {
        bool i8 i16 i32 i64 i128 u8 u16 u32 u64 u128 f32 f64 char
        bytes byte_buf option unit unit_struct newtype_struct seq tuple
        tuple_struct map struct ignored_any
    }
}

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
            name: Cow::Borrowed(self.0),
        })
    }

    serde::forward_to_deserialize_any! {
        bool i8 i16 i32 i64 i128 u8 u16 u32 u64 u128 f32 f64 char str string
        bytes byte_buf option unit unit_struct newtype_struct seq tuple
        tuple_struct map struct identifier ignored_any
    }
}

struct UnitVariant<'de> {
    name: Cow<'de, str>,
}

impl<'de> de::EnumAccess<'de> for UnitVariant<'de> {
    type Error = Error;
    type Variant = UnitPayload;

    fn variant_seed<V: DeserializeSeed<'de>>(self, seed: V) -> Result<(V::Value, UnitPayload)> {
        let v = match self.name {
            Cow::Borrowed(s) => seed.deserialize(BorrowedStr(s))?,
            Cow::Owned(s) => seed.deserialize(s.into_deserializer())?,
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
        Err(err(0, "expected a unit variant"))
    }
    fn tuple_variant<V: Visitor<'de>>(self, _l: usize, _v: V) -> Result<V::Value> {
        Err(err(0, "expected a unit variant"))
    }
    fn struct_variant<V: Visitor<'de>>(self, _f: &'static [&'static str], _v: V) -> Result<V::Value> {
        Err(err(0, "expected a unit variant"))
    }
}

struct VariantAccess<'a, 'de> {
    c: &'a mut Cursor<'de>,
    depth: usize,
}

impl<'a, 'de> de::EnumAccess<'de> for VariantAccess<'a, 'de> {
    type Error = Error;
    type Variant = Payload<'a, 'de>;

    fn variant_seed<V: DeserializeSeed<'de>>(self, seed: V) -> Result<(V::Value, Self::Variant)> {
        self.c.skip_ws();
        let at = self.c.pos;
        if self.c.peek() != b'"' {
            return Err(err(at, "expected a variant name"));
        }
        let (raw, _) = self.c.scan_string()?;
        self.c.skip_ws();
        if !self.c.eat(b':') {
            return Err(err(self.c.pos, "expected ':' after variant"));
        }
        let name = crate::unescape::unescape_checked(raw)
            .ok_or_else(|| err(at, "invalid variant name"))?;
        let v = match name {
            Cow::Borrowed(s) => seed.deserialize(BorrowedStr(s))?,
            Cow::Owned(s) => seed.deserialize(s.into_deserializer())?,
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
    fn close(self) -> Result<()> {
        self.c.skip_ws();
        if !self.c.eat(b'}') {
            return Err(err(self.c.pos, "enum object must have exactly one key"));
        }
        Ok(())
    }
}

impl<'a, 'de> de::VariantAccess<'de> for Payload<'a, 'de> {
    type Error = Error;

    fn unit_variant(self) -> Result<()> {
        Err(err(self.c.pos, "expected a newtype, tuple or struct variant"))
    }

    fn newtype_variant_seed<T: DeserializeSeed<'de>>(self, seed: T) -> Result<T::Value> {
        let depth = self.depth;
        let v = seed.deserialize(ValueDe { c: self.c, depth })?;
        self.close()?;
        Ok(v)
    }

    fn tuple_variant<V: Visitor<'de>>(self, _len: usize, visitor: V) -> Result<V::Value> {
        let depth = self.depth;
        let v = de::Deserializer::deserialize_any(ValueDe { c: self.c, depth }, visitor)?;
        self.close()?;
        Ok(v)
    }

    fn struct_variant<V: Visitor<'de>>(
        self,
        _f: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value> {
        let depth = self.depth;
        let v = de::Deserializer::deserialize_any(ValueDe { c: self.c, depth }, visitor)?;
        self.close()?;
        Ok(v)
    }
}

#[cfg(test)]
mod tests {
    use super::Cursor;

    fn scan(src: &[u8]) -> Option<(Vec<u8>, bool)> {
        let mut c = Cursor {
            input: src,
            pos: 0,
            ascii: false,
        };
        c.scan_string().ok().map(|(r, s)| (r.to_vec(), s))
    }

    /// The SWAR scan crosses 8-byte boundaries, so every alignment and
    /// every length either side of the chunk size has to be covered.
    #[test]
    fn finds_the_terminator_at_every_alignment() {
        for len in 0..40usize {
            let body = "a".repeat(len);
            let src = format!("\"{body}\"tail");
            let got = scan(src.as_bytes()).unwrap_or_else(|| panic!("len={len}"));
            assert_eq!(got.0, body.as_bytes(), "len={len}");
            assert!(got.1, "plain ASCII should be simple, len={len}");
        }
    }

    /// `simple` must be false for anything needing the slow path, at every
    /// offset - including when the special byte shares a chunk with the
    /// terminator.
    #[test]
    fn simple_is_false_for_escapes_control_bytes_and_non_ascii() {
        for pad in 0..20usize {
            let a = "a".repeat(pad);
            for (body, why) in [
                (format!("{a}\\n"), "escape"),
                (format!("{a}\u{7f}\u{80}"), "non-ascii"),
                (format!("{a}é"), "utf-8"),
            ] {
                let src = format!("\"{body}\"");
                let got = scan(src.as_bytes())
                    .unwrap_or_else(|| panic!("{why} pad={pad} did not scan"));
                assert!(!got.1, "{why} at pad={pad} was reported simple");
            }
            // A raw control byte, which cannot go through format!.
            let mut src = vec![b'"'];
            src.extend(core::iter::repeat_n(b'a', pad));
            src.push(0x09);
            src.push(b'"');
            let got = scan(&src).unwrap_or_else(|| panic!("ctrl pad={pad}"));
            assert!(!got.1, "raw tab at pad={pad} was reported simple");
        }
    }

    /// Bytes after the closing quote must not affect `simple`.
    #[test]
    fn trailing_bytes_do_not_taint_simplicity() {
        // The non-ASCII byte sits after the terminator, inside the same
        // 8-byte chunk.
        let src = "\"ab\",\"é\"".as_bytes();
        let got = scan(src).expect("scan");
        assert_eq!(got.0, b"ab");
        assert!(got.1, "a later chunk byte leaked into `simple`");
    }

    #[test]
    fn unterminated_is_an_error() {
        assert!(scan(b"\"abc").is_none());
        assert!(scan(b"\"abc\\").is_none());
    }
}
