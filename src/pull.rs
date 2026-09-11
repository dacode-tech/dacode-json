//! On-demand extraction: pull the fields you want, skip the rest wholesale.
//!
//! `Deserialize` cannot express "I want two of these forty fields, stop
//! looking once you have them". serde's `MapAccess` must yield *every* key
//! so the derived field matcher can reject the unknown ones, so the cost of
//! a record scales with the record, not with what you asked for.
//!
//! This module is the other shape — the one simdjson's On-Demand API and
//! sonic-rs's `get` have, and the one behind C libraries that take a format
//! string and a list of pointers to fill. You drive it, it converts nothing
//! you do not ask for, and it **allocates nothing at all**.
//!
//! ```
//! # fn main() -> Result<(), dacodec::stream::Error> {
//! let src = br#"[{"id":1,"score":40,"junk":[1,2,3]},{"id":2,"score":2,"junk":{}}]"#;
//!
//! let mut total = 0i64;
//! dacodec::pull::for_each_object(src, |fields| {
//!     while let Some((key, val)) = fields.next()? {
//!         if key == b"score" {
//!             total += val.as_i64().unwrap_or(0);
//!             // Everything after this in the record is skipped without
//!             // being looked at field by field.
//!             break;
//!         }
//!     }
//!     Ok(())
//! })?;
//! assert_eq!(total, 42);
//! # Ok(()) }
//! ```
//!
//! # Validation: weaker, deliberately, and only here
//!
//! [`crate::from_slice`] validates every byte, including in fields the
//! target type ignores, so it accepts exactly what `serde_json` accepts.
//!
//! **This module does not.** A value you never read is skipped
//! structurally: strings are traversed to find their end, but escapes,
//! UTF-8 and control bytes in them are not checked, and numbers are not
//! checked for syntax. That is what makes it fast, it is the same trade
//! simdjson On-Demand makes, and it is why it lives behind a separate
//! function instead of quietly speeding up `from_slice`.
//!
//! Values you *do* read are fully validated: [`Raw::as_str`] rejects bad
//! escapes and bad UTF-8, [`Raw::as_i64`] rejects malformed numbers.
//!
//! If you need "reject the whole document if any part is malformed", call
//! [`crate::validate`] first, or use `from_slice`.
//!
//! # Embedded
//!
//! Nothing here allocates: no index, no pool, no `String`. The only memory
//! is the input slice and a handful of `usize` on the stack. Recursion
//! depth is bounded by [`MAX_DEPTH`]. It builds for a target with no
//! allocator and no OS, and for 8- and 16-bit ones.
//!
//! Several accessors exist for the same field because the choice is
//! worth 22 KB of flash:
//!
//! * [`Raw::as_int::<T>`](Raw::as_int) reads digits straight into the
//!   width you name and refuses fractions. It cannot reach a float
//!   parser, because it cannot return a float.
//! * [`Raw::as_fixed::<T>`](Raw::as_fixed) reads `12.34` as the integer
//!   `1234`. A *fractional* reading, still with no float parser: a feed
//!   quoting two decimal places does not need seventeen significant
//!   figures, and scaling by a power of ten is the same digit loop with
//!   the point moved.
//! * [`Raw::as_f64`] and [`Raw::as_i64`] are the general readings.
//!   `as_i64` accepts `1.0` as the integer 1, which means it has to be
//!   able to parse `1.5` first, which links `core`'s correctly-rounded
//!   float parser — 22 KB on a chip with no double-precision FPU.
//!
//! Reading `INPUT`'s one fractional field costs 2 214 bytes through
//! `as_fixed` and 24 486 through `as_f64`.
//!
//! Likewise [`Raw::as_borrowed_str`] against [`Raw::as_str`]: the first
//! hands back a subslice and needs no allocator, the second expands
//! escapes and needs one. `docs/SIZE.md` has the measurements.

use crate::stream::{expect_lit, parse_number, Error, Num};

type Result<T> = core::result::Result<T, Error>;

fn err(at: usize, msg: &'static str) -> Error {
    Error::from_parts(at, msg)
}

/// How deep a skipped container may nest before this gives up.
pub const MAX_DEPTH: usize = 128;

/// What a value is, without having converted it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Null,
    Bool,
    Number,
    Str,
    Array,
    Object,
}

/// A value located but not converted.
///
/// The bytes are the value's exact extent in the input: for a string, the
/// content between the quotes, still escaped.
#[derive(Debug, Clone, Copy)]
pub struct Raw<'de> {
    bytes: &'de [u8],
    at: usize,
    kind: Kind,
}

impl<'de> Raw<'de> {
    #[must_use]
    pub fn kind(&self) -> Kind {
        self.kind
    }

    /// The raw extent, unconverted and unvalidated.
    #[must_use]
    pub fn bytes(&self) -> &'de [u8] {
        self.bytes
    }

    /// Byte offset of the value in the input, for error messages.
    #[must_use]
    pub fn offset(&self) -> usize {
        self.at
    }

    #[must_use]
    pub fn as_bool(&self) -> Option<bool> {
        match self.kind {
            Kind::Bool => Some(self.bytes.first() == Some(&b't')),
            _ => None,
        }
    }

    #[must_use]
    pub fn is_null(&self) -> bool {
        self.kind == Kind::Null
    }

    /// Read an integer of the width you ask for, without linking a float
    /// parser.
    ///
    /// ```
    /// # fn main() -> Result<(), dacodec::stream::Error> {
    /// dacodec::pull::select(br#"[{"t":-40,"h":81}]"#, &[b"t", b"h"], |got| {
    ///     assert_eq!(got[0].and_then(|v| v.as_int::<i16>()), Some(-40));
    ///     assert_eq!(got[1].and_then(|v| v.as_int::<u8>()), Some(81));
    ///     Ok(())
    /// })?;
    /// # Ok(()) }
    /// ```
    ///
    /// `None` if the value is not a number, does not fit `T`, or **has a
    /// fraction or an exponent**: `1.0` and `1e2` are not integers here.
    /// Deciding otherwise would mean parsing the float, which is the cost
    /// this exists to avoid — [`as_i64`](Self::as_i64) is the lenient
    /// reading and accepts them.
    ///
    /// Digits accumulate in `T` and nothing wider, so asking for an
    /// `i32` links no 64-bit arithmetic either. On
    /// `thumbv7em-none-eabihf` this is the difference between a 24 KB
    /// binary and a 2 KB one; see [`crate::stream::Integer`] and
    /// `docs/SIZE.md`.
    #[must_use]
    pub fn as_int<T: crate::stream::Integer>(&self) -> Option<T> {
        if self.kind != Kind::Number {
            return None;
        }
        crate::stream::parse_integer(self.bytes, 0, self.bytes.len()).ok()
    }

    /// Read a fractional number as a scaled integer, without linking a
    /// float parser.
    ///
    /// `scale` is how many decimal places to keep, so the result is the
    /// value multiplied by `10^scale`:
    ///
    /// ```
    /// # fn main() -> Result<(), dacodec::stream::Error> {
    /// dacodec::pull::select(br#"[{"c":-12.34,"v":3.9}]"#, &[b"c", b"v"], |got| {
    ///     // -12.34 °C in hundredths, 3.9 V in millivolts.
    ///     assert_eq!(got[0].and_then(|v| v.as_fixed::<i32>(2)), Some(-1234));
    ///     assert_eq!(got[1].and_then(|v| v.as_fixed::<i32>(3)), Some(3900));
    ///     Ok(())
    /// })?;
    /// # Ok(()) }
    /// ```
    ///
    /// A whole number is fine — `12` at `scale = 2` is `1200` — and so is
    /// a fraction shorter than the scale, which is zero-padded.
    ///
    /// # Truncation, not rounding
    ///
    /// Digits past `scale` are **dropped**, towards zero on both signs:
    /// `12.345` and `-12.345` at `scale = 2` are `1234` and `-1234`.
    /// Rounding to nearest-even is what a float parser does, and doing it
    /// here would mean carrying a rounding decision back through the
    /// digits — the arithmetic this exists to avoid. Truncation is also
    /// what a caller reading a sensor at a fixed precision expects: the
    /// extra digits are noise, not information.
    ///
    /// If you need the rounded reading, [`as_f64`](Self::as_f64) gives
    /// you the float and the 22 KB that comes with it.
    ///
    /// # What is rejected
    ///
    /// `None` if the value is not a number, if the scaled result does not
    /// fit `T` — `1.0` at `scale = 9` overflows an `i32` — or if the
    /// number **has an exponent**. `1.234e2` would need the decimal point
    /// shifted a second time, by a signed amount, before any of this
    /// applies; [`as_f64`](Self::as_f64) is the reading that accepts it.
    ///
    /// [`as_int`](Self::as_int) is this at `scale = 0`, except that
    /// `as_int` additionally refuses a fraction rather than truncating
    /// it.
    ///
    /// # Why `scale` is an argument and not a const generic
    ///
    /// Width is a type parameter here because narrowing it removes
    /// arithmetic. `scale` removes nothing, so making it a const generic
    /// only adds a monomorphisation axis. Measured on
    /// `thumbv7em-none-eabihf`: at one call site the two are **identical**
    /// at 2 214 bytes, because a literal argument const-folds anyway; at
    /// three scales the runtime argument is 2 358 bytes and the const
    /// generic 3 022 — 664 bytes to say the same thing. See `docs/SIZE.md`.
    #[must_use]
    pub fn as_fixed<T: crate::stream::Integer>(&self, scale: u32) -> Option<T> {
        if self.kind != Kind::Number {
            return None;
        }
        crate::stream::parse_fixed(self.bytes, 0, self.bytes.len(), scale).ok()
    }

    /// Convert to `i64`, validating the number.
    ///
    /// Accepts an integral float, and therefore links `core`'s float
    /// parser. [`as_int`](Self::as_int) is the one that does not.
    #[must_use]
    pub fn as_i64(&self) -> Option<i64> {
        match self.num()? {
            Num::I(v) => Some(v),
            Num::U(v) => i64::try_from(v).ok(),
            Num::F(v) => {
                // Accept an integral float, as `serde_json`'s `as_i64` does not.
                //
                // Spelled without `f64::fract`/`f64::abs`, which live in
                // `std` rather than `core`: in range, a round-trip
                // through `i64` truncates, so it only compares equal when
                // there was no fraction to truncate. `LIMIT` is 2^63,
                // exactly representable, and the bound has to be there —
                // `i64::MAX as f64` rounds *up* to 2^63, so a value of
                // 2^63 would otherwise saturate and appear to round-trip.
                const LIMIT: f64 = 9_223_372_036_854_775_808.0;
                if v > -LIMIT && v < LIMIT && (v as i64) as f64 == v {
                    Some(v as i64)
                } else {
                    None
                }
            }
        }
    }

    #[must_use]
    pub fn as_u64(&self) -> Option<u64> {
        match self.num()? {
            Num::U(v) => Some(v),
            Num::I(v) => u64::try_from(v).ok(),
            Num::F(_) => None,
        }
    }

    #[must_use]
    pub fn as_f64(&self) -> Option<f64> {
        match self.num()? {
            Num::F(v) => Some(v),
            #[allow(clippy::cast_precision_loss)]
            Num::I(v) => Some(v as f64),
            #[allow(clippy::cast_precision_loss)]
            Num::U(v) => Some(v as f64),
        }
    }

    fn num(&self) -> Option<Num> {
        if self.kind != Kind::Number {
            return None;
        }
        parse_number(self.bytes, 0, self.bytes.len()).ok()
    }

    /// Decode a string, validating escapes and UTF-8.
    ///
    /// Borrows when there is no escape to expand, which is the common case.
    /// Needs an allocator only for the other case; see
    /// [`as_borrowed_str`](Self::as_borrowed_str) for the form that needs
    /// none.
    #[cfg(feature = "alloc")]
    #[must_use]
    pub fn as_str(&self) -> Option<alloc::borrow::Cow<'de, str>> {
        if self.kind != Kind::Str {
            return None;
        }
        crate::unescape::unescape_checked(self.bytes)
    }

    /// The string as a subslice of the input, if it can be one.
    ///
    /// `None` when the string contains an escape — expanding one needs a
    /// buffer, and this module has none — or is not valid UTF-8. See
    /// [`crate::unescape::borrow_str_checked`] for why those two are not
    /// distinguished. [`bytes`](Self::bytes) still gives the raw extent.
    ///
    /// This is the accessor that exists with no allocator, which is what
    /// makes the module usable on a target with no heap.
    #[must_use]
    pub fn as_borrowed_str(&self) -> Option<&'de str> {
        if self.kind != Kind::Str {
            return None;
        }
        crate::unescape::borrow_str_checked(self.bytes)
    }
}

// =====================================================================
// Cursor
// =====================================================================

#[derive(Debug)]
struct Cur<'de> {
    input: &'de [u8],
    pos: usize,
}

impl<'de> Cur<'de> {
    #[inline]
    fn peek(&self) -> u8 {
        match self.input.get(self.pos) {
            Some(&b) => b,
            None => 0,
        }
    }

    #[inline]
    fn skip_ws(&mut self) {
        while matches!(self.input.get(self.pos), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.pos += 1;
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

    /// Find the end of a string whose opening quote is at `self.pos`.
    ///
    /// Returns the content between the quotes. Escapes are located, so the
    /// terminator is found correctly, but not decoded or validated.
    fn string(&mut self) -> Result<&'de [u8]> {
        let open = self.pos;
        if !self.eat(b'"') {
            return Err(err(open, "expected a string"));
        }
        let start = self.pos;
        let mut i = start;
        while let Some(&b) = self.input.get(i) {
            match b {
                b'"' => {
                    self.pos = i + 1;
                    return self
                        .input
                        .get(start..i)
                        .ok_or_else(|| err(open, "unterminated string"));
                }
                b'\\' => i += 2,
                _ => i += 1,
            }
        }
        Err(err(open, "unterminated string"))
    }

    /// Locate the value at `self.pos` and step past it.
    fn value(&mut self) -> Result<Raw<'de>> {
        self.skip_ws();
        let at = self.pos;
        let kind = match self.peek() {
            b'"' => {
                let bytes = self.string()?;
                return Ok(Raw {
                    bytes,
                    at,
                    kind: Kind::Str,
                });
            }
            b'{' => Kind::Object,
            b'[' => Kind::Array,
            b't' | b'f' => Kind::Bool,
            b'n' => Kind::Null,
            0 => return Err(err(at, "expected a value")),
            _ => Kind::Number,
        };
        match kind {
            Kind::Object | Kind::Array => {
                self.skip_container()?;
            }
            Kind::Bool => {
                let want: &[u8] = if self.peek() == b't' { b"true" } else { b"false" };
                let end = self.scalar_end();
                expect_lit(self.input, at, end, want)?;
                self.pos = end;
            }
            Kind::Null => {
                let end = self.scalar_end();
                expect_lit(self.input, at, end, b"null")?;
                self.pos = end;
            }
            // Numbers are not validated here; `Raw::as_i64` validates on
            // conversion, and a number nobody converts costs nothing.
            _ => self.pos = self.scalar_end(),
        }
        let bytes = self
            .input
            .get(at..self.pos)
            .ok_or_else(|| err(at, "value ran past the end"))?;
        Ok(Raw { bytes, at, kind })
    }

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

    /// Step past the remainder of the object we are already inside.
    ///
    /// Depth starts at 1: the opening brace has been consumed. This is a
    /// raw byte scan, not a parse — which is the entire reason breaking out
    /// of a [`Fields`] loop early is cheap.
    fn finish_object(&mut self) -> Result<()> {
        let start = self.pos;
        let mut depth = 1usize;
        loop {
            match self.input.get(self.pos) {
                None => return Err(err(start, "unterminated object")),
                Some(b'"') => {
                    self.string()?;
                    continue;
                }
                Some(b'{' | b'[') => {
                    depth += 1;
                    if depth > MAX_DEPTH {
                        return Err(err(self.pos, "nesting too deep"));
                    }
                }
                Some(b'}' | b']') => {
                    depth -= 1;
                    if depth == 0 {
                        self.pos += 1;
                        return Ok(());
                    }
                }
                Some(_) => {}
            }
            self.pos += 1;
        }
    }

    /// Step past a whole `{...}` or `[...]`, counting depth.
    ///
    /// Strings are traversed so that a brace inside one is not mistaken for
    /// structure. Their contents are not validated — see the module note.
    fn skip_container(&mut self) -> Result<()> {
        let start = self.pos;
        let mut depth = 0usize;
        loop {
            match self.input.get(self.pos) {
                None => return Err(err(start, "unterminated container")),
                Some(b'"') => {
                    self.string()?;
                    continue;
                }
                Some(b'{' | b'[') => {
                    depth += 1;
                    if depth > MAX_DEPTH {
                        return Err(err(self.pos, "nesting too deep"));
                    }
                }
                Some(b'}' | b']') => {
                    depth -= 1;
                    if depth == 0 {
                        self.pos += 1;
                        return Ok(());
                    }
                }
                Some(_) => {}
            }
            self.pos += 1;
        }
    }
}

// =====================================================================
// Field iteration
// =====================================================================

/// Yields `(key, value)` pairs of one object.
///
/// Stop early — `break` out of the loop — and the rest of the object is
/// skipped in one go rather than field by field. That is the whole point.
#[derive(Debug)]
pub struct Fields<'a, 'de> {
    c: &'a mut Cur<'de>,
    first: bool,
    done: bool,
}

impl<'a, 'de> Fields<'a, 'de> {
    /// The next `(key, value)`, or `None` at the end of the object.
    ///
    /// The key is raw bytes, still escaped. Compare it with `b"name"`
    /// directly; that is why no UTF-8 check happens here.
    #[allow(clippy::should_implement_trait)]
    pub fn next(&mut self) -> Result<Option<(&'de [u8], Raw<'de>)>> {
        if self.done {
            return Ok(None);
        }
        self.c.skip_ws();
        if self.c.eat(b'}') {
            self.done = true;
            return Ok(None);
        }
        if !self.first {
            if !self.c.eat(b',') {
                return Err(err(self.c.pos, "expected ',' or '}' in object"));
            }
            self.c.skip_ws();
        }
        self.first = false;
        let key = self.c.string()?;
        self.c.skip_ws();
        if !self.c.eat(b':') {
            return Err(err(self.c.pos, "expected ':' after key"));
        }
        let val = self.c.value()?;
        Ok(Some((key, val)))
    }

    /// Consume whatever is left of the object.
    ///
    /// Walking the remaining fields one at a time would undo the early
    /// exit — and did, in the first version of this module: selecting the
    /// first field of seven measured the same as selecting the last.
    fn finish(&mut self) -> Result<()> {
        if self.done {
            return Ok(());
        }
        self.done = true;
        self.c.finish_object()
    }
}

/// Walk one object, pulling fields.
pub fn object<'de, F>(input: &'de [u8], mut f: F) -> Result<()>
where
    F: FnMut(&mut Fields<'_, 'de>) -> Result<()>,
{
    let mut c = Cur { input, pos: 0 };
    c.skip_ws();
    if !c.eat(b'{') {
        return Err(err(c.pos, "expected an object"));
    }
    let mut fields = Fields {
        c: &mut c,
        first: true,
        done: false,
    };
    f(&mut fields)?;
    fields.finish()?;
    Ok(())
}

/// Walk a root array of objects, pulling fields from each.
///
/// Returns how many objects were seen.
pub fn for_each_object<'de, F>(input: &'de [u8], mut f: F) -> Result<usize>
where
    F: FnMut(&mut Fields<'_, 'de>) -> Result<()>,
{
    let mut c = Cur { input, pos: 0 };
    c.skip_ws();
    if !c.eat(b'[') {
        return Err(err(c.pos, "expected an array"));
    }
    let mut n = 0usize;
    loop {
        c.skip_ws();
        if c.eat(b']') {
            return Ok(n);
        }
        if n > 0 {
            if !c.eat(b',') {
                return Err(err(c.pos, "expected ',' or ']' in array"));
            }
            c.skip_ws();
        }
        if !c.eat(b'{') {
            return Err(err(c.pos, "expected an object in the array"));
        }
        let mut fields = Fields {
            c: &mut c,
            first: true,
            done: false,
        };
        f(&mut fields)?;
        fields.finish()?;
        n += 1;
    }
}

/// Pull named fields out of every object in a root array, in one pass.
///
/// The closest safe, type-checked equivalent of the C idiom
/// `scan(json, "{id:%u,score:%d}", &id, &score)`: you name the fields, you
/// get back a slot per name per record, and nothing else is converted.
/// Scanning stops for a record as soon as every named field is found.
///
/// ```
/// # fn main() -> Result<(), dacodec::stream::Error> {
/// let src = br#"[{"a":1,"skip":[1,2],"b":"x"},{"b":"y","a":2}]"#;
/// let mut out = Vec::new();
/// dacodec::pull::select(src, &[b"a", b"b"], |got| {
///     out.push((
///         got[0].and_then(|v| v.as_i64()),
///         got[1].and_then(|v| v.as_str()).map(|s| s.into_owned()),
///     ));
///     Ok(())
/// })?;
/// assert_eq!(out, vec![(Some(1), Some("x".into())), (Some(2), Some("y".into()))]);
/// # Ok(()) }
/// ```
pub fn select<'de, const N: usize, F>(
    input: &'de [u8],
    names: &[&[u8]; N],
    mut f: F,
) -> Result<usize>
where
    F: FnMut([Option<Raw<'de>>; N]) -> Result<()>,
{
    for_each_object(input, |fields| {
        let mut got: [Option<Raw<'de>>; N] = [None; N];
        let mut found = 0usize;
        while let Some((key, val)) = fields.next()? {
            if let Some(slot) = names
                .iter()
                .position(|n| *n == key)
                .and_then(|i| got.get_mut(i))
            {
                if slot.is_none() {
                    *slot = Some(val);
                    found += 1;
                    // Every name accounted for: skip the rest of the record.
                    if found == N {
                        break;
                    }
                }
            }
        }
        f(got)
    })
}

/// Pull named fields out of a single root object, in one pass.
///
/// The one-record form of [`select`]. A config file, a manifest, a
/// response header — anything that is one object rather than a stream of
/// them — has exactly one set of slots, so there is nothing to hand to a
/// closure: the array *is* the return value. Destructure it and the
/// bindings are your struct, checked at compile time to be as many as you
/// named.
///
/// ```
/// # fn main() -> Result<(), dacodec::stream::Error> {
/// let src = br#"{"host":"edge","port":8080,"extra":{"skip":[1,2]}}"#;
/// let [host, port] = dacodec::pull::select_object(src, &[b"host", b"port"])?;
/// assert_eq!(host.and_then(|v| v.as_borrowed_str()), Some("edge"));
/// assert_eq!(port.and_then(|v| v.as_int::<u16>()), Some(8080));
/// # Ok(()) }
/// ```
///
/// # Duplicate keys
///
/// The **last** occurrence wins, as in `JSON.parse`, `serde_json` and
/// Python's `json`. This differs from [`select`], which keeps the first
/// because it stops scanning a record the moment every name is accounted
/// for. That trade is worth making across a million-record array and is
/// worth nothing on one object, so this scans to the closing brace and
/// matches what every other JSON reader does.
pub fn select_object<'de, const N: usize>(
    input: &'de [u8],
    names: &[&[u8]; N],
) -> Result<[Option<Raw<'de>>; N]> {
    select_object_with(input, names, |_, _| {})
}

/// As [`select_object`], reporting fields that matched none of `names`.
///
/// [`select`] and [`select_object`] skip whatever they were not asked
/// for. That is right for pulling two fields out of a large record and
/// wrong for reading a document a person wrote by hand, where a mistyped
/// key is otherwise silently nothing at all — the reader does what it was
/// told, the writer sees no effect, and neither is wrong.
///
/// `unknown` is called with each such `(key, value)` in document order.
/// The key is raw bytes, still escaped; the value carries its
/// [`offset`](Raw::offset), which is what a caller needs to say *where*.
///
/// ```
/// # fn main() -> Result<(), dacodec::stream::Error> {
/// let src = br#"{"prot": 8080, "host": "edge"}"#;
/// let mut typo = None;
/// let [host] = dacodec::pull::select_object_with(src, &[b"host"], |key, val| {
///     typo = Some((key, val.offset()));
/// })?;
/// assert_eq!(host.and_then(|v| v.as_borrowed_str()), Some("edge"));
/// assert_eq!(typo, Some((&b"prot"[..], 9)));
/// # Ok(()) }
/// ```
pub fn select_object_with<'de, const N: usize, F>(
    input: &'de [u8],
    names: &[&[u8]; N],
    mut unknown: F,
) -> Result<[Option<Raw<'de>>; N]>
where
    F: FnMut(&'de [u8], Raw<'de>),
{
    let mut got: [Option<Raw<'de>>; N] = [None; N];
    object(input, |fields| {
        while let Some((key, val)) = fields.next()? {
            match names.iter().position(|n| *n == key) {
                Some(i) => {
                    if let Some(slot) = got.get_mut(i) {
                        *slot = Some(val);
                    }
                }
                None => unknown(key, val),
            }
        }
        Ok(())
    })?;
    Ok(got)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SRC: &[u8] = br#"[
      {"id":1,"name":"a","score":40,"junk":[1,2,{"x":"}"}],"tail":true},
      {"id":2,"score":2,"name":"b","junk":{"nested":[{"deep":"]"}]}}
    ]"#;

    #[test]
    fn select_finds_fields_in_any_order() {
        let mut rows = Vec::new();
        let n = select(SRC, &[b"id", b"score"], |got| {
            rows.push((
                got[0].and_then(|v| v.as_u64()),
                got[1].and_then(|v| v.as_i64()),
            ));
            Ok(())
        })
        .expect("scan");
        assert_eq!(n, 2);
        assert_eq!(rows, vec![(Some(1), Some(40)), (Some(2), Some(2))]);
    }

    /// Braces and brackets inside strings must not confuse the skipper.
    #[test]
    fn skips_containers_holding_brace_like_strings() {
        let mut names = Vec::new();
        select(SRC, &[b"name"], |got| {
            names.push(got[0].and_then(|v| v.as_str()).map(|s| s.into_owned()));
            Ok(())
        })
        .expect("scan");
        assert_eq!(names, vec![Some("a".into()), Some("b".into())]);
    }

    #[test]
    fn missing_field_is_none() {
        let n = select(SRC, &[b"nope"], |got| {
            assert!(got[0].is_none());
            Ok(())
        })
        .expect("scan");
        assert_eq!(n, 2);
    }

    #[test]
    fn agrees_with_the_full_parser_on_values() {
        let mut sum = 0i64;
        select(SRC, &[b"score"], |got| {
            sum += got[0].and_then(|v| v.as_i64()).unwrap_or(0);
            Ok(())
        })
        .expect("scan");

        let doc: serde_json::Value = crate::from_slice(SRC).expect("full parse");
        let want: i64 = doc
            .as_array()
            .map(|a| a.iter().filter_map(|r| r["score"].as_i64()).sum())
            .unwrap_or(0);
        assert_eq!(sum, want);
    }

    #[test]
    fn select_object_destructures_into_bindings() {
        let src = br#"{"host":"edge","port":8080,"junk":{"a":[1,{"b":"}"}]}}"#;
        let [host, port] = select_object(src, &[b"host", b"port"]).expect("scan");
        assert_eq!(host.and_then(|v| v.as_borrowed_str()), Some("edge"));
        assert_eq!(port.and_then(|v| v.as_int::<u16>()), Some(8080));
    }

    #[test]
    fn select_object_leaves_absent_names_none() {
        let [a, b] = select_object(br#"{"a":1}"#, &[b"a", b"b"]).expect("scan");
        assert!(a.is_some());
        assert!(b.is_none());
    }

    #[test]
    fn select_object_finds_fields_in_any_order() {
        let [a, b] = select_object(br#"{"b":2,"a":1}"#, &[b"a", b"b"]).expect("scan");
        assert_eq!(a.and_then(|v| v.as_i64()), Some(1));
        assert_eq!(b.and_then(|v| v.as_i64()), Some(2));
    }

    /// The documented deviation from [`select`]: `JSON.parse`, `serde_json`
    /// and Python all keep the last, and a config file silently taking the
    /// first of two settings would be a memorable afternoon.
    #[test]
    fn select_object_keeps_the_last_duplicate() {
        let [a] = select_object(br#"{"a":1,"a":2,"a":3}"#, &[b"a"]).expect("scan");
        assert_eq!(a.and_then(|v| v.as_i64()), Some(3));
    }

    #[test]
    fn select_object_reports_unmatched_keys_in_document_order() {
        let src = br#"{"one":1,"a":2,"two":3,"three":4}"#;
        let mut seen = Vec::new();
        let [a] = select_object_with(src, &[b"a"], |k, v| {
            seen.push((String::from_utf8_lossy(k).into_owned(), v.offset()));
        })
        .expect("scan");
        assert_eq!(a.and_then(|v| v.as_i64()), Some(2));
        assert_eq!(
            seen.iter().map(|(k, _)| k.as_str()).collect::<Vec<_>>(),
            vec!["one", "two", "three"]
        );
        // The offsets must point at the values, for a caller reporting a
        // position back to whoever wrote the file.
        assert!(seen.iter().all(|(_, at)| *at > 0 && *at < src.len()));
    }

    /// A repeated *known* key is a known key, not an unknown one.
    #[test]
    fn a_duplicate_of_a_named_key_is_not_reported_as_unknown() {
        let mut seen = 0;
        let _ = select_object_with(br#"{"a":1,"a":2}"#, &[b"a"], |_, _| seen += 1).expect("scan");
        assert_eq!(seen, 0);
    }

    #[test]
    fn select_object_rejects_an_array_root() {
        assert!(select_object(br#"[{"a":1}]"#, &[b"a"]).is_err());
    }

    #[test]
    fn select_object_propagates_a_syntax_error_with_its_offset() {
        let err = select_object(br#"{"a":1,}"#, &[b"a"]).expect_err("should fail");
        assert_eq!(err.offset, Some(7));
    }

    /// Zero names is degenerate but must not misbehave: everything is
    /// unknown and the array is empty.
    #[test]
    fn select_object_with_no_names_reports_everything() {
        let mut seen = 0;
        let got = select_object_with(br#"{"a":1,"b":2}"#, &[], |_, _| seen += 1).expect("scan");
        assert_eq!(got.len(), 0);
        assert_eq!(seen, 2);
    }

    #[test]
    fn single_object_entry_point() {
        let mut got = None;
        object(br#"{"a":1,"b":2}"#, |fields| {
            while let Some((k, v)) = fields.next()? {
                if k == b"b" {
                    got = v.as_i64();
                    break;
                }
            }
            Ok(())
        })
        .expect("scan");
        assert_eq!(got, Some(2));
    }

    #[test]
    fn structural_errors_are_still_errors() {
        for bad in [
            &b"[{\"a\":1}"[..], // unterminated array
            &b"[{\"a\"}]"[..],  // no colon after a key we read
            &b"{\"a\":1}"[..],  // not an array
            &b"[{\"a\":1},"[..], // unterminated after a complete object
        ] {
            assert!(
                select(bad, &[b"a"], |_| Ok(())).is_err(),
                "accepted {:?}",
                String::from_utf8_lossy(bad)
            );
        }
    }

    /// The documented trade, pinned so it cannot drift silently.
    ///
    /// Once every requested field has been found, the rest of the record
    /// is skipped by counting braces. Malformed content in that tail is
    /// *not* rejected. `crate::from_slice` rejects all of these; this
    /// module is a different contract, not a faster version of the same
    /// one.
    #[test]
    fn malformed_content_in_a_skipped_tail_is_accepted() {
        let cases: &[&[u8]] = &[
            br#"[{"a":1,}]"#,          // trailing comma after what we read
            br#"[{"a":1,"b":01}]"#,    // invalid number, never converted
            br#"[{"a":1,"b":tru}]"#,   // invalid literal
            br#"[{"a":1,"b":"\q"}]"#,  // invalid escape
        ];
        for src in cases {
            // serde_json rejects every one of these.
            assert!(
                serde_json::from_slice::<serde_json::Value>(src).is_err(),
                "fix the test, serde_json accepted {:?}",
                String::from_utf8_lossy(src)
            );
            // So does the full parser.
            assert!(
                crate::from_slice::<serde_json::Value>(src).is_err(),
                "from_slice accepted {:?}",
                String::from_utf8_lossy(src)
            );
            // `pull` does not, because it never looks.
            let got = select(src, &[b"a"], |g| {
                assert_eq!(g[0].and_then(|v| v.as_i64()), Some(1));
                Ok(())
            });
            assert!(
                got.is_ok(),
                "pull rejected {:?} - if this now errors, the module docs \
                 and README need updating, not this test",
                String::from_utf8_lossy(src)
            );
        }
    }

    #[test]
    fn conversions_still_validate() {
        // `01` is not a valid number, and asking for it must fail.
        let src = br#"[{"a":01}]"#;
        select(src, &[b"a"], |got| {
            assert_eq!(got[0].and_then(|v| v.as_i64()), None);
            Ok(())
        })
        .expect("structure is fine");
    }
    /// `as_int` must agree with `as_i64` wherever both answer, and must
    /// refuse exactly the cases that would need a float parser.
    #[test]
    fn as_int_is_the_strict_reading() {
        let cases: &[(&str, Option<i64>, Option<i64>)] = &[
            // json      as_int::<i64>  as_i64
            ("0", Some(0), Some(0)),
            ("-0", Some(0), Some(0)),
            ("7", Some(7), Some(7)),
            ("-40", Some(-40), Some(-40)),
            ("9223372036854775807", Some(i64::MAX), Some(i64::MAX)),
            ("-9223372036854775808", Some(i64::MIN), Some(i64::MIN)),
            // Out of range for i64 either way.
            ("9223372036854775808", None, None),
            // Integral floats: `as_i64` accepts, `as_int` refuses,
            // because accepting would mean parsing the float.
            ("1.0", None, Some(1)),
            ("1e2", None, Some(100)),
            ("-2.0", None, Some(-2)),
            // Not integers by any reading.
            ("1.5", None, None),
        ];
        for &(src, want_int, want_i64) in cases {
            let doc = alloc::format!("[{{\"v\":{src}}}]");
            select(doc.as_bytes(), &[b"v"], |got| {
                let v = got[0].expect("field present");
                assert_eq!(v.as_int::<i64>(), want_int, "as_int({src})");
                assert_eq!(v.as_i64(), want_i64, "as_i64({src})");
                Ok(())
            })
            .unwrap_or_else(|e| panic!("{src}: {e}"));
        }
    }

    /// Every width is exact at its own boundaries, and one past them is
    /// `None` rather than a wrap.
    #[test]
    fn as_int_respects_the_width_asked_for() {
        macro_rules! check {
            ($t:ty, $min:expr, $max:expr) => {{
                for (src, want) in [
                    (alloc::format!("{}", $min), Some($min)),
                    (alloc::format!("{}", $max), Some($max)),
                ] {
                    let doc = alloc::format!("[{{\"v\":{src}}}]");
                    select(doc.as_bytes(), &[b"v"], |got| {
                        assert_eq!(got[0].and_then(|v| v.as_int::<$t>()), want, "{src}");
                        Ok(())
                    })
                    .expect("valid");
                }
                // One past each end must not wrap.
                for src in [
                    alloc::format!("{}", i128::from($min) - 1),
                    alloc::format!("{}", i128::from($max) + 1),
                ] {
                    let doc = alloc::format!("[{{\"v\":{src}}}]");
                    select(doc.as_bytes(), &[b"v"], |got| {
                        assert_eq!(got[0].and_then(|v| v.as_int::<$t>()), None, "{src}");
                        Ok(())
                    })
                    .expect("valid");
                }
            }};
        }
        check!(i8, i8::MIN, i8::MAX);
        check!(u8, u8::MIN, u8::MAX);
        check!(i16, i16::MIN, i16::MAX);
        check!(u16, u16::MIN, u16::MAX);
        check!(i32, i32::MIN, i32::MAX);
        check!(u32, u32::MIN, u32::MAX);
    }

    /// An unsigned width takes `-0` and refuses every other negative,
    /// rather than wrapping it.
    #[test]
    fn negatives_do_not_wrap_into_unsigned() {
        for (src, want) in [("-0", Some(0u32)), ("-1", None), ("-4294967295", None)] {
            let doc = alloc::format!("[{{\"v\":{src}}}]");
            select(doc.as_bytes(), &[b"v"], |got| {
                assert_eq!(got[0].and_then(|v| v.as_int::<u32>()), want, "{src}");
                Ok(())
            })
            .expect("valid");
        }
    }

    /// Malformed numbers are still rejected: `as_int` validates syntax
    /// even though it never converts a float.
    #[test]
    fn as_int_still_validates_syntax() {
        for src in ["01", "1.", "1e", "-", "+1", "0x10", "--1", "1e+", "0."] {
            let doc = alloc::format!("[{{\"v\":{src}}}]");
            let mut checked = false;
            let _ = select(doc.as_bytes(), &[b"v"], |got| {
                if let Some(v) = got[0] {
                    assert_eq!(v.as_int::<i64>(), None, "as_int({src})");
                    assert_eq!(v.as_i64(), None, "as_i64({src})");
                    checked = true;
                }
                Ok(())
            });
            assert!(checked, "{src}: field was never offered");
        }
    }

    /// The scaling itself: padding a short fraction, truncating a long
    /// one, and accepting a whole number.
    #[test]
    fn as_fixed_scales_by_a_power_of_ten() {
        let cases: &[(&str, u32, Option<i64>)] = &[
            // json        scale  as_fixed::<i64>
            ("12.34", 2, Some(1234)),
            // Fewer fraction digits than the scale: zero-padded.
            ("12.3", 2, Some(1230)),
            ("12", 2, Some(1200)),
            ("0.5", 3, Some(500)),
            // More than the scale: dropped, not rounded. `.345` at two
            // places is 34, never 35 — this is the documented choice.
            ("12.345", 2, Some(1234)),
            ("12.999", 2, Some(1299)),
            ("0.9999999", 1, Some(9)),
            // Scale 0 throws the whole fraction away.
            ("12.99", 0, Some(12)),
            ("0.000", 0, Some(0)),
            // A scale bigger than any fraction is still just padding.
            ("7", 6, Some(7_000_000)),
        ];
        for &(src, scale, want) in cases {
            let doc = alloc::format!("[{{\"v\":{src}}}]");
            select(doc.as_bytes(), &[b"v"], |got| {
                let v = got[0].expect("field present");
                assert_eq!(v.as_fixed::<i64>(scale), want, "as_fixed({src}, {scale})");
                Ok(())
            })
            .unwrap_or_else(|e| panic!("{src}: {e}"));
        }
    }

    /// Truncation is towards zero, not towards negative infinity.
    ///
    /// `-12.345` at two places is `-1234`. The alternative, `-1235`, is
    /// what flooring would give and would make the reading asymmetric
    /// about zero — a sensor swinging either side of zero would show a
    /// half-count bias. Falls out of accumulating the magnitude and
    /// subtracting, but it is the kind of thing that gets "simplified"
    /// later, so it is pinned here.
    #[test]
    fn as_fixed_truncates_towards_zero_on_both_signs() {
        let cases: &[(&str, Option<i32>)] = &[
            ("12.345", Some(1234)),
            ("-12.345", Some(-1234)),
            ("0.999", Some(99)),
            ("-0.999", Some(-99)),
            ("-0.001", Some(0)),
            ("-0.0", Some(0)),
        ];
        for &(src, want) in cases {
            let doc = alloc::format!("[{{\"v\":{src}}}]");
            select(doc.as_bytes(), &[b"v"], |got| {
                assert_eq!(got[0].and_then(|v| v.as_fixed::<i32>(2)), want, "{src}");
                Ok(())
            })
            .unwrap_or_else(|e| panic!("{src}: {e}"));
        }
    }

    /// `as_fixed(0)` and `as_int` agree on everything `as_int` accepts.
    ///
    /// They are the same digit loop; if they ever diverge on an integer,
    /// one of them has grown a bug rather than a feature.
    #[test]
    fn as_fixed_at_scale_zero_matches_as_int() {
        for src in [
            "0",
            "-0",
            "7",
            "-40",
            "127",
            "-128",
            "9223372036854775807",
            "-9223372036854775808",
            "9223372036854775808",
        ] {
            let doc = alloc::format!("[{{\"v\":{src}}}]");
            select(doc.as_bytes(), &[b"v"], |got| {
                let v = got[0].expect("field present");
                assert_eq!(v.as_fixed::<i64>(0), v.as_int::<i64>(), "{src}");
                Ok(())
            })
            .unwrap_or_else(|e| panic!("{src}: {e}"));
        }
    }

    /// The scaled result must fit the width asked for, and overflow is
    /// `None` rather than a wrap — including overflow caused by the
    /// scale alone.
    #[test]
    fn as_fixed_overflow_is_none_not_a_wrap() {
        let doc = br#"[{"v":327.68,"w":-327.68,"x":1.0,"y":32.767}]"#;
        select(doc, &[b"v", b"w", b"x", b"y"], |got| {
            let (v, w, x, y) = (
                got[0].expect("v"),
                got[1].expect("w"),
                got[2].expect("x"),
                got[3].expect("y"),
            );
            // 32768 is one past i16::MAX; -32768 is exactly i16::MIN, so
            // the negative side must still succeed where the positive
            // one fails.
            assert_eq!(v.as_fixed::<i16>(2), None, "327.68 -> i16");
            assert_eq!(w.as_fixed::<i16>(2), Some(-32768), "-327.68 -> i16");
            assert_eq!(y.as_fixed::<i16>(3), Some(32767), "32.767 -> i16");
            // Overflow from padding alone: the digits fit, the scale does
            // not.
            assert_eq!(x.as_fixed::<i32>(9), Some(1_000_000_000), "1.0 at 9");
            assert_eq!(x.as_fixed::<i32>(10), None, "1.0 at 10");
            assert_eq!(x.as_fixed::<i32>(u32::MAX), None, "1.0 at u32::MAX");
            Ok(())
        })
        .expect("valid");
    }

    /// An unsigned width takes a negative only when the truncated result
    /// is zero, and never wraps.
    #[test]
    fn as_fixed_negatives_do_not_wrap_into_unsigned() {
        for (src, want) in [
            ("-0.0", Some(0u16)),
            // Truncates to zero, so it is representable.
            ("-0.001", Some(0)),
            ("-0.01", None),
            ("-1.5", None),
        ] {
            let doc = alloc::format!("[{{\"v\":{src}}}]");
            select(doc.as_bytes(), &[b"v"], |got| {
                assert_eq!(got[0].and_then(|v| v.as_fixed::<u16>(2)), want, "{src}");
                Ok(())
            })
            .expect("valid");
        }
    }

    /// Exponents are refused, and malformed numbers are still refused.
    ///
    /// `1.234e2` has a value `as_fixed` could represent; taking it would
    /// mean a second, signed shift of the point. See the accessor docs —
    /// if that decision is ever reversed, this test is the one to change.
    #[test]
    fn as_fixed_refuses_exponents_and_bad_syntax() {
        for src in [
            "1e2", "1.234e2", "1E2", "1e-2", "0.5e1", // exponents
            "01", "1.", "1e", "-", "+1", "0x10", "--1", ".5", "0.", // malformed
        ] {
            let doc = alloc::format!("[{{\"v\":{src}}}]");
            let mut checked = false;
            let _ = select(doc.as_bytes(), &[b"v"], |got| {
                if let Some(v) = got[0] {
                    assert_eq!(v.as_fixed::<i64>(2), None, "as_fixed({src})");
                    checked = true;
                }
                Ok(())
            });
            assert!(checked, "{src}: field was never offered");
        }
    }

    /// Only numbers answer; the other kinds are `None`, not a parse of
    /// their bytes.
    #[test]
    fn as_fixed_only_reads_numbers() {
        let doc = br#"[{"s":"1.5","b":true,"n":null,"a":[1.5],"o":{}}]"#;
        select(doc, &[b"s", b"b", b"n", b"a", b"o"], |got| {
            for (i, name) in ["s", "b", "n", "a", "o"].iter().enumerate() {
                let v = got.get(i).copied().flatten().expect("field present");
                assert_eq!(v.as_fixed::<i32>(2), None, "{name}");
            }
            Ok(())
        })
        .expect("valid");
    }

    /// Where `pull`'s laxness shows, and that `as_int` does not widen it.
    ///
    /// `pull` ends a number at the next delimiter and does not police
    /// what follows, so `{"v":1 2}` is accepted here and rejected by
    /// [`crate::from_slice`] — the trade the module docs describe. What
    /// matters for `as_int` is that it reads the extent it was given and
    /// does not invent a value from the rest.
    #[test]
    fn trailing_garbage_is_pulls_gap_not_a_wrong_number() {
        for (src, want) in [("1 2", 1i64), ("1,", 1), ("1 true", 1)] {
            let doc = alloc::format!("[{{\"v\":{src}}}]");
            select(doc.as_bytes(), &[b"v"], |got| {
                let v = got[0].expect("field present");
                assert_eq!(v.as_int::<i64>(), Some(want), "{src}");
                Ok(())
            })
            .unwrap_or_else(|e| panic!("{src}: {e}"));
            // The strict path is the one that rejects it.
            assert!(
                crate::validate(doc.as_bytes()).is_err(),
                "{src}: validate should reject"
            );
        }
    }
}
