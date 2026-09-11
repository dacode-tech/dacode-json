//! The serde half of [`crate::stream`]: index in, visitor calls out.
//!
//! Split from the parent module because everything here needs `serde`
//! and, through [`StructuralIndex`], an allocator. The error type and the
//! number readers do not, and stay in `mod.rs` so [`crate::pull`] can use
//! them on a target with neither.

use alloc::borrow::Cow;
use serde::de::{self, DeserializeSeed, IntoDeserializer, MapAccess, SeqAccess, Visitor};

use super::{expect_lit, parse_number, validate_number, Error, Num, Result};
use crate::scan::{self, Scanner, StructuralIndex};

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
// Entry points
// =====================================================================

/// Parse `input` into `T` without building a node pool.
///
/// `T` must be [`serde::de::DeserializeOwned`]; see [`from_slice_with`]
/// for the borrowing form, which needs the index to outlive the result.
pub fn from_slice<T: serde::de::DeserializeOwned>(input: &[u8]) -> Result<T> {
    let mut idx = StructuralIndex::default();
    idx.reserve_estimated(input.len());
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
/// let mut idx = dacode_json::stream::Index::default();
/// for src in [r#"{"name":"a"}"#, r#"{"name":"b"}"#] {
///     let row: Row<'_> = dacode_json::stream::from_slice_with(&mut idx, src.as_bytes())?;
///     assert!(!row.name.is_empty());
/// }
/// # Ok::<(), dacode_json::stream::Error>(())
/// ```
pub fn from_slice_with<'de, T: serde::Deserialize<'de>>(
    idx: &'de mut StructuralIndex,
    input: &'de [u8],
) -> Result<T> {
    idx.clear();
    idx.reserve_estimated(input.len());
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
                    Some(Cow::Borrowed(s)) => visitor.visit_borrowed_str(s),
                    Some(Cow::Owned(s)) => visitor.visit_string(s),
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
                // The offending byte used to be formatted into the
                // message. The offset already points at it, and keeping
                // it cost an allocation on every parse error.
                _ => return Err(Error::at(self.c.peek_pos(), "expected ',' or '}' in object")),
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
            Cow::Borrowed(s) => seed.deserialize(BorrowedStr(s)).map(Some),
            Cow::Owned(s) => seed.deserialize(s.into_deserializer()).map(Some),
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
                _ => return Err(Error::at(self.c.peek_pos(), "expected ',' or ']' in array")),
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

    /// How many elements remain, when that is cheap to find out.
    ///
    /// serde uses this to size the `Vec` up front instead of growing it by
    /// doubling. The count is a walk over the index to the matching
    /// bracket, so it is bounded: past `LIMIT` elements this gives up and
    /// returns `None`. serde caps the capacity it will trust anyway
    /// (`4096 / size_of::<T>()`), so counting a 100 000-element array
    /// would be a walk of the whole index for a hint that gets clamped to
    /// about 50.
    fn size_hint(&self) -> Option<usize> {
        const LIMIT: usize = 512;
        if self.done {
            return Some(0);
        }
        let mut at = self.c.at;
        let mut depth = 0usize;
        let mut count = 0usize;
        // `first` means no element has been consumed yet, so an immediate
        // closer is an empty array rather than a trailing element.
        let mut seen_any = !self.first;
        loop {
            let p = *self.c.pos.get(at)? as usize;
            match self.c.input.get(p)? {
                b'{' | b'[' => {
                    depth += 1;
                    seen_any = true;
                }
                b'}' => depth = depth.checked_sub(1)?,
                b']' => {
                    if depth == 0 {
                        return Some(if seen_any { count + 1 } else { 0 });
                    }
                    depth -= 1;
                }
                b'"' => {
                    seen_any = true;
                    // Skip the closing quote so a bracket inside a string
                    // is not counted as structure.
                    at += 1;
                }
                b',' if depth == 0 => {
                    count += 1;
                    seen_any = true;
                    if count > LIMIT {
                        return None;
                    }
                }
                _ => seen_any = true,
            }
            at += 1;
        }
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
