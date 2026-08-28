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

use crate::stream::{expect_lit, parse_number, validate_number, Error, Num};

type Result<T> = core::result::Result<T, Error>;

fn err(at: usize, msg: &str) -> Error {
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

/// Index of the first `"` or `\` at or after `from`, or `input.len()`.
///
/// Eight bytes per iteration. Control bytes are not looked for here:
/// `unescape_checked` rejects them when the string is decoded, and adding
/// a third comparison to this loop measurably slowed it down.
#[inline]
fn find_quote_or_escape(input: &[u8], from: usize) -> usize {
    let n = input.len();
    let mut i = from;
    const QUOTES: u64 = LO * b'"' as u64;
    const SLASHES: u64 = LO * b'\\' as u64;

    while i + 8 <= n {
        let Some(bytes) = input.get(i..i + 8) else {
            break;
        };
        let Ok(arr) = <[u8; 8]>::try_from(bytes) else {
            break;
        };
        let chunk = u64::from_le_bytes(arr);
        let mask = zero_bytes(chunk ^ QUOTES) | zero_bytes(chunk ^ SLASHES);
        if mask != 0 {
            return i + (mask.trailing_zeros() / 8) as usize;
        }
        i += 8;
    }
    while let Some(&b) = input.get(i) {
        if b == b'"' || b == b'\\' {
            return i;
        }
        i += 1;
    }
    n
}

// =====================================================================
// Cursor
// =====================================================================

struct Cursor<'de> {
    input: &'de [u8],
    pos: usize,
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
    /// Returns the raw bytes between the quotes, still escaped.
    fn scan_string(&mut self) -> Result<&'de [u8]> {
        let open = self.pos;
        if !self.eat(b'"') {
            return Err(err(open, "expected a string"));
        }
        let start = self.pos;
        let mut i = start;
        loop {
            i = find_quote_or_escape(self.input, i);
            match self.input.get(i) {
                Some(b'"') => {
                    self.pos = i + 1;
                    return self
                        .input
                        .get(start..i)
                        .ok_or_else(|| err(open, "unterminated string"));
                }
                // Skip the escaped byte and keep going. A trailing
                // backslash runs off the end and is caught below.
                Some(b'\\') => i += 2,
                _ => return Err(err(open, "unterminated string")),
            }
        }
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

/// Parse into a possibly-borrowing `T`.
///
/// Nothing needs to outlive the call except the input itself — there is no
/// scratch buffer at all — so unlike [`crate::stream::from_slice_with`]
/// this needs no caller-owned state.
pub fn from_slice_borrowed<'de, T: serde::Deserialize<'de>>(input: &'de [u8]) -> Result<T> {
    let mut c = Cursor { input, pos: 0 };
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
                let raw = self.c.scan_string()?;
                match crate::unescape::unescape_checked(raw) {
                    Some(std::borrow::Cow::Borrowed(s)) => visitor.visit_borrowed_str(s),
                    Some(std::borrow::Cow::Owned(s)) => visitor.visit_string(s),
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
                let raw = self.c.scan_string()?;
                if crate::unescape::unescape_checked(raw).is_none() {
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
                let raw = self.c.scan_string()?;
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
        let raw = self.c.scan_string()?;
        self.c.skip_ws();
        if !self.c.eat(b':') {
            return Err(err(self.c.pos, "expected ':' after object key"));
        }

        let key = crate::unescape::unescape_checked(raw)
            .ok_or_else(|| err(at, "invalid escape or UTF-8 in key"))?;
        match key {
            std::borrow::Cow::Borrowed(s) => seed.deserialize(BorrowedStr(s)).map(Some),
            std::borrow::Cow::Owned(s) => seed.deserialize(s.into_deserializer()).map(Some),
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
        let raw = self.c.scan_string()?;
        self.c.skip_ws();
        if !self.c.eat(b':') {
            return Err(err(self.c.pos, "expected ':' after variant"));
        }
        let name = crate::unescape::unescape_checked(raw)
            .ok_or_else(|| err(at, "invalid variant name"))?;
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
    #[test]
    fn swar_finds_the_same_byte_as_a_scalar_scan() {
        // Cover every alignment and both terminators.
        for pad in 0..24usize {
            for term in [b'"', b'\\'] {
                let mut v = vec![b'a'; pad];
                v.push(term);
                v.extend_from_slice(b"tail");
                let want = pad;
                assert_eq!(super::find_quote_or_escape(&v, 0), want, "pad={pad}");
            }
        }
        // No terminator at all returns the length.
        let none = vec![b'x'; 37];
        assert_eq!(super::find_quote_or_escape(&none, 0), 37);
    }
}
