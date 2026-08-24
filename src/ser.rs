//! `serde::Serializer` producing JSON, with a vectorised string escaper.
//!
//! # Relationship to Vela
//!
//! Vela has two emitters. `emit.vl` builds output with `result =
//! "{result}{ch}"` per byte, which reallocates on every append — O(n²).
//! `emit_v2.vl` replaces the builder with a `DualBuffer` (front region for
//! output, back region for a comma-flag stack) and is documented as
//! "DualBuffer-backed **O(n)** emitter".
//!
//! It is not O(n). `emit_v2.vl:14` declares and calls `json_escape_string`,
//! and that function (`common.vl:52-80`) is the original per-byte
//! string-concat loop. Every string written through the "O(n)" emitter is
//! still escaped in quadratic time.
//!
//! This module is the shape the Vela emitter should have:
//!
//! * one output `Vec<u8>`, amortised growth, no intermediate strings;
//! * escaping by **bulk-copying the runs between escapes**, with the next
//!   escape position found 16 bytes at a time;
//! * integers formatted into a stack buffer, no allocation.
//!
//! # Escaping policy
//!
//! Escapes `"`, `\` and everything below `0x20`, matching `serde_json`.
//! Vela additionally escapes `/` (`common.vl:74`), which is legal but
//! unusual; [`Options::escape_solidus`] reproduces it.

use serde::{ser, Serialize};
use std::fmt;

/// Serialisation failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Error {
    msg: String,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.msg)
    }
}

impl std::error::Error for Error {}

impl ser::Error for Error {
    fn custom<T: fmt::Display>(msg: T) -> Self {
        Error {
            msg: msg.to_string(),
        }
    }
}

type Result<T> = core::result::Result<T, Error>;

fn err(msg: &str) -> Error {
    Error {
        msg: msg.to_string(),
    }
}

/// Emitter options.
#[derive(Debug, Clone, Copy, Default)]
pub struct Options {
    /// Escape `/` as `\/`, as `common.vl:74` does. Off by default, matching
    /// `serde_json`.
    pub escape_solidus: bool,
}

/// Serialize `value` to a JSON byte vector.
pub fn to_vec<T: Serialize + ?Sized>(value: &T) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity(128);
    to_writer(&mut out, value)?;
    Ok(out)
}

/// Serialize `value` to a `String`.
pub fn to_string<T: Serialize + ?Sized>(value: &T) -> Result<String> {
    let v = to_vec(value)?;
    String::from_utf8(v).map_err(|_| err("serializer produced invalid UTF-8"))
}

/// Serialize into an existing buffer, reusing its allocation.
pub fn to_writer<T: Serialize + ?Sized>(out: &mut Vec<u8>, value: &T) -> Result<()> {
    let mut s = Serializer {
        out,
        opts: Options::default(),
    };
    value.serialize(&mut s)
}

/// Serialize into an existing buffer with explicit options.
pub fn to_writer_with<T: Serialize + ?Sized>(
    out: &mut Vec<u8>,
    value: &T,
    opts: Options,
) -> Result<()> {
    let mut s = Serializer { out, opts };
    value.serialize(&mut s)
}

/// The serializer.
#[derive(Debug)]
pub struct Serializer<'a> {
    out: &'a mut Vec<u8>,
    opts: Options,
}

// =====================================================================
// String escaping
// =====================================================================

/// `ESCAPE[b]` is the character to write after the backslash, `UU` when the
/// byte needs the `\u00XX` long form, or 0 when it needs no escape.
///
/// One table lookup replaces a chain of comparisons in the inner loop; this
/// is the same shape `serde_json` uses.
const UU: u8 = 1;

#[allow(clippy::indexing_slicing)] // const-evaluated: OOB is a compile error
const ESCAPE: [u8; 256] = {
    let mut t = [0u8; 256];
    let mut b = 0usize;
    while b < 0x20 {
        t[b] = UU;
        b += 1;
    }
    t[0x08] = b'b';
    t[0x09] = b't';
    t[0x0A] = b'n';
    t[0x0C] = b'f';
    t[0x0D] = b'r';
    t[b'"' as usize] = b'"';
    t[b'\\' as usize] = b'\\';
    t
};

const HEX: &[u8; 16] = b"0123456789abcdef";

#[inline(always)]
fn needs_escape(b: u8, solidus: bool) -> bool {
    b < 0x20 || b == b'"' || b == b'\\' || (solidus && b == b'/')
}

/// Bitmask of bytes needing an escape, one bit per lane.
#[cfg(target_arch = "aarch64")]
#[inline]
fn escape_mask(chunk: &[u8; 16], solidus: bool) -> u16 {
    use core::arch::aarch64::*;
    const LANE_BITS: [u8; 16] = [
        0x01, 0x02, 0x04, 0x08, 0x10, 0x20, 0x40, 0x80, //
        0x01, 0x02, 0x04, 0x08, 0x10, 0x20, 0x40, 0x80,
    ];
    // SAFETY: NEON is baseline on aarch64; `chunk` is 16 readable bytes and
    // `LANE_BITS` is a 16-byte array.
    unsafe {
        let v = vld1q_u8(chunk.as_ptr());
        // b < 0x20  |  b == '"'  |  b == '\\'
        let mut hit = vorrq_u8(
            vcltq_u8(v, vdupq_n_u8(0x20)),
            vorrq_u8(vceqq_u8(v, vdupq_n_u8(b'"')), vceqq_u8(v, vdupq_n_u8(b'\\'))),
        );
        if solidus {
            hit = vorrq_u8(hit, vceqq_u8(v, vdupq_n_u8(b'/')));
        }
        let m = vandq_u8(hit, vld1q_u8(LANE_BITS.as_ptr()));
        u16::from(vaddv_u8(vget_low_u8(m))) | (u16::from(vaddv_u8(vget_high_u8(m))) << 8)
    }
}

#[cfg(not(target_arch = "aarch64"))]
#[inline]
fn escape_mask(chunk: &[u8; 16], solidus: bool) -> u16 {
    let mut m = 0u16;
    for (i, &b) in chunk.iter().enumerate() {
        m |= u16::from(needs_escape(b, solidus)) << i;
    }
    m
}

/// Emit the escape sequence for one byte that needs escaping.
#[inline]
fn emit_escape(out: &mut Vec<u8>, b: u8) {
    // '/' is not in ESCAPE because it is only escaped under an option.
    if b == b'/' {
        out.extend_from_slice(br"\/");
        return;
    }
    match ESCAPE.get(b as usize).copied().unwrap_or(0) {
        0 => out.push(b),
        UU => {
            out.extend_from_slice(br"\u00");
            out.push(HEX.get((b >> 4) as usize).copied().unwrap_or(b'0'));
            out.push(HEX.get((b & 0xF) as usize).copied().unwrap_or(b'0'));
        }
        c => {
            // Two bytes in one store rather than two pushes.
            out.extend_from_slice(&[b'\\', c]);
        }
    }
}

/// Write `s` as a quoted JSON string, copying the runs between escapes in
/// bulk.
///
/// Walks 16 bytes at a time and drains **every** escape in a chunk from the
/// one mask. An earlier version called `next_escape` again after each
/// escape, which restarted a 16-byte vector scan to advance a single byte —
/// fine for clean text, but it made densely escaped strings slower than
/// `serde_json`'s plain byte loop.
#[inline]
pub fn write_escaped(out: &mut Vec<u8>, s: &str, opts: Options) {
    let bytes = s.as_bytes();
    let solidus = opts.escape_solidus;

    // Worst case is 6 bytes out per byte in (`\u001f`), but reserving for
    // that would balloon the buffer; reserve for the common case and let
    // `extend_from_slice` grow if an escape-heavy string needs it.
    out.reserve(bytes.len() + 2);
    out.push(b'"');

    // Start of the not-yet-copied run.
    let mut start = 0usize;
    let mut i = 0usize;

    while i + 16 <= bytes.len() {
        let Some(arr) = bytes
            .get(i..i + 16)
            .and_then(|c| <&[u8; 16]>::try_from(c).ok())
        else {
            break;
        };
        let mut mask = escape_mask(arr, solidus);

        while mask != 0 {
            let pos = i + mask.trailing_zeros() as usize;
            if let Some(run) = bytes.get(start..pos) {
                out.extend_from_slice(run);
            }
            emit_escape(out, bytes.get(pos).copied().unwrap_or(0));
            start = pos + 1;
            mask &= mask.wrapping_sub(1);
        }

        i += 16;
    }

    // Tail: fewer than 16 bytes left.
    while i < bytes.len() {
        let b = bytes.get(i).copied().unwrap_or(0);
        if needs_escape(b, solidus) {
            if let Some(run) = bytes.get(start..i) {
                out.extend_from_slice(run);
            }
            emit_escape(out, b);
            start = i + 1;
        }
        i += 1;
    }

    if let Some(rest) = bytes.get(start..) {
        out.extend_from_slice(rest);
    }
    out.push(b'"');
}

/// Format an `i64` into `out` without allocating.
#[inline]
fn write_i64(out: &mut Vec<u8>, v: i64) {
    if v < 0 {
        out.push(b'-');
    }
    write_u64(out, v.unsigned_abs());
}

/// Format a `u64` into `out` without allocating.
#[inline]
fn write_u64(out: &mut Vec<u8>, mut v: u64) {
    if v == 0 {
        out.push(b'0');
        return;
    }
    let mut buf = [0u8; 20];
    let mut i = buf.len();
    while v > 0 {
        i -= 1;
        if let Some(slot) = buf.get_mut(i) {
            *slot = b'0' + (v % 10) as u8;
        }
        v /= 10;
    }
    if let Some(digits) = buf.get(i..) {
        out.extend_from_slice(digits);
    }
}

/// Format an `f64`. JSON has no infinity or NaN, so those become `null`,
/// matching `serde_json`.
///
/// Uses `zmij` (Schubfach), as `serde_json` 1.0.151 does — it switched from
/// `ryu`, and the two disagree on exponent formatting (`1e300` vs
/// `1e+300`). `write!(out, "{v}")` was tried first
/// and is wrong for this purpose in two ways: it drops the fractional part
/// (`2.0` becomes `2`, changing the JSON type on re-read for some consumers)
/// and it never uses exponent notation, so `3.7e20` becomes twenty-one
/// digits. It is also several times slower.
#[inline]
fn write_f64(out: &mut Vec<u8>, v: f64) {
    if !v.is_finite() {
        out.extend_from_slice(b"null");
        return;
    }
    let mut buf = zmij::Buffer::new();
    out.extend_from_slice(buf.format_finite(v).as_bytes());
}

// =====================================================================
// serde::Serializer
// =====================================================================

impl<'a, 'b> ser::Serializer for &'b mut Serializer<'a> {
    type Ok = ();
    type Error = Error;
    type SerializeSeq = Compound<'a, 'b>;
    type SerializeTuple = Compound<'a, 'b>;
    type SerializeTupleStruct = Compound<'a, 'b>;
    type SerializeTupleVariant = Compound<'a, 'b>;
    type SerializeMap = Compound<'a, 'b>;
    type SerializeStruct = Compound<'a, 'b>;
    type SerializeStructVariant = Compound<'a, 'b>;

    fn serialize_bool(self, v: bool) -> Result<()> {
        self.out
            .extend_from_slice(if v { b"true" } else { b"false" });
        Ok(())
    }

    fn serialize_i8(self, v: i8) -> Result<()> {
        self.serialize_i64(i64::from(v))
    }
    fn serialize_i16(self, v: i16) -> Result<()> {
        self.serialize_i64(i64::from(v))
    }
    fn serialize_i32(self, v: i32) -> Result<()> {
        self.serialize_i64(i64::from(v))
    }
    fn serialize_i64(self, v: i64) -> Result<()> {
        write_i64(self.out, v);
        Ok(())
    }
    fn serialize_i128(self, v: i128) -> Result<()> {
        use std::io::Write as _;
        let _ = write!(self.out, "{v}");
        Ok(())
    }

    fn serialize_u8(self, v: u8) -> Result<()> {
        self.serialize_u64(u64::from(v))
    }
    fn serialize_u16(self, v: u16) -> Result<()> {
        self.serialize_u64(u64::from(v))
    }
    fn serialize_u32(self, v: u32) -> Result<()> {
        self.serialize_u64(u64::from(v))
    }
    fn serialize_u64(self, v: u64) -> Result<()> {
        write_u64(self.out, v);
        Ok(())
    }
    fn serialize_u128(self, v: u128) -> Result<()> {
        use std::io::Write as _;
        let _ = write!(self.out, "{v}");
        Ok(())
    }

    fn serialize_f32(self, v: f32) -> Result<()> {
        self.serialize_f64(f64::from(v))
    }
    fn serialize_f64(self, v: f64) -> Result<()> {
        write_f64(self.out, v);
        Ok(())
    }

    fn serialize_char(self, v: char) -> Result<()> {
        let mut buf = [0u8; 4];
        self.serialize_str(v.encode_utf8(&mut buf))
    }

    fn serialize_str(self, v: &str) -> Result<()> {
        write_escaped(self.out, v, self.opts);
        Ok(())
    }

    /// JSON has no byte string; emit an array of numbers, as `serde_json`
    /// does by default.
    fn serialize_bytes(self, v: &[u8]) -> Result<()> {
        self.out.push(b'[');
        for (i, b) in v.iter().enumerate() {
            if i > 0 {
                self.out.push(b',');
            }
            write_u64(self.out, u64::from(*b));
        }
        self.out.push(b']');
        Ok(())
    }

    fn serialize_none(self) -> Result<()> {
        self.serialize_unit()
    }
    fn serialize_some<T: Serialize + ?Sized>(self, v: &T) -> Result<()> {
        v.serialize(self)
    }
    fn serialize_unit(self) -> Result<()> {
        self.out.extend_from_slice(b"null");
        Ok(())
    }
    fn serialize_unit_struct(self, _name: &'static str) -> Result<()> {
        self.serialize_unit()
    }

    fn serialize_unit_variant(
        self,
        _name: &'static str,
        _idx: u32,
        variant: &'static str,
    ) -> Result<()> {
        self.serialize_str(variant)
    }

    fn serialize_newtype_struct<T: Serialize + ?Sized>(
        self,
        _name: &'static str,
        v: &T,
    ) -> Result<()> {
        v.serialize(self)
    }

    fn serialize_newtype_variant<T: Serialize + ?Sized>(
        self,
        _name: &'static str,
        _idx: u32,
        variant: &'static str,
        v: &T,
    ) -> Result<()> {
        self.out.push(b'{');
        write_escaped(self.out, variant, self.opts);
        self.out.push(b':');
        v.serialize(&mut *self)?;
        self.out.push(b'}');
        Ok(())
    }

    fn serialize_seq(self, _len: Option<usize>) -> Result<Compound<'a, 'b>> {
        self.out.push(b'[');
        Ok(Compound {
            ser: self,
            first: true,
            close: Close::Array,
        })
    }

    fn serialize_tuple(self, len: usize) -> Result<Compound<'a, 'b>> {
        self.serialize_seq(Some(len))
    }

    fn serialize_tuple_struct(
        self,
        _name: &'static str,
        len: usize,
    ) -> Result<Compound<'a, 'b>> {
        self.serialize_seq(Some(len))
    }

    fn serialize_tuple_variant(
        self,
        _name: &'static str,
        _idx: u32,
        variant: &'static str,
        _len: usize,
    ) -> Result<Compound<'a, 'b>> {
        self.out.push(b'{');
        write_escaped(self.out, variant, self.opts);
        self.out.extend_from_slice(b":[");
        Ok(Compound {
            ser: self,
            first: true,
            close: Close::ArrayInObject,
        })
    }

    fn serialize_map(self, _len: Option<usize>) -> Result<Compound<'a, 'b>> {
        self.out.push(b'{');
        Ok(Compound {
            ser: self,
            first: true,
            close: Close::Object,
        })
    }

    fn serialize_struct(self, _name: &'static str, len: usize) -> Result<Compound<'a, 'b>> {
        self.serialize_map(Some(len))
    }

    fn serialize_struct_variant(
        self,
        _name: &'static str,
        _idx: u32,
        variant: &'static str,
        _len: usize,
    ) -> Result<Compound<'a, 'b>> {
        self.out.push(b'{');
        write_escaped(self.out, variant, self.opts);
        self.out.extend_from_slice(b":{");
        Ok(Compound {
            ser: self,
            first: true,
            close: Close::ObjectInObject,
        })
    }
}

#[derive(Debug, Clone, Copy)]
enum Close {
    Array,
    Object,
    ArrayInObject,
    ObjectInObject,
}

/// Shared state for every compound form.
///
/// Vela keeps the "does this level need a comma?" flag on the `DualBuffer`'s
/// back-region byte stack (`emit_v2.vl:41-56`). serde hands us a distinct
/// value per nesting level, so the flag lives in that value and no stack is
/// needed.
#[derive(Debug)]
pub struct Compound<'a, 'b> {
    ser: &'b mut Serializer<'a>,
    first: bool,
    close: Close,
}

impl Compound<'_, '_> {
    #[inline]
    fn comma(&mut self) {
        if self.first {
            self.first = false;
        } else {
            self.ser.out.push(b',');
        }
    }

    fn finish(self) -> Result<()> {
        match self.close {
            Close::Array => self.ser.out.push(b']'),
            Close::Object => self.ser.out.push(b'}'),
            Close::ArrayInObject => self.ser.out.extend_from_slice(b"]}"),
            Close::ObjectInObject => self.ser.out.extend_from_slice(b"}}"),
        }
        Ok(())
    }
}

impl ser::SerializeSeq for Compound<'_, '_> {
    type Ok = ();
    type Error = Error;
    fn serialize_element<T: Serialize + ?Sized>(&mut self, v: &T) -> Result<()> {
        self.comma();
        v.serialize(&mut *self.ser)
    }
    fn end(self) -> Result<()> {
        self.finish()
    }
}

impl ser::SerializeTuple for Compound<'_, '_> {
    type Ok = ();
    type Error = Error;
    fn serialize_element<T: Serialize + ?Sized>(&mut self, v: &T) -> Result<()> {
        self.comma();
        v.serialize(&mut *self.ser)
    }
    fn end(self) -> Result<()> {
        self.finish()
    }
}

impl ser::SerializeTupleStruct for Compound<'_, '_> {
    type Ok = ();
    type Error = Error;
    fn serialize_field<T: Serialize + ?Sized>(&mut self, v: &T) -> Result<()> {
        self.comma();
        v.serialize(&mut *self.ser)
    }
    fn end(self) -> Result<()> {
        self.finish()
    }
}

impl ser::SerializeTupleVariant for Compound<'_, '_> {
    type Ok = ();
    type Error = Error;
    fn serialize_field<T: Serialize + ?Sized>(&mut self, v: &T) -> Result<()> {
        self.comma();
        v.serialize(&mut *self.ser)
    }
    fn end(self) -> Result<()> {
        self.finish()
    }
}

impl ser::SerializeMap for Compound<'_, '_> {
    type Ok = ();
    type Error = Error;

    fn serialize_key<T: Serialize + ?Sized>(&mut self, k: &T) -> Result<()> {
        self.comma();
        // JSON keys must be strings, so route through a serializer that
        // accepts nothing else.
        k.serialize(KeySerializer { ser: self.ser })
    }

    fn serialize_value<T: Serialize + ?Sized>(&mut self, v: &T) -> Result<()> {
        self.ser.out.push(b':');
        v.serialize(&mut *self.ser)
    }

    fn end(self) -> Result<()> {
        self.finish()
    }
}

impl ser::SerializeStruct for Compound<'_, '_> {
    type Ok = ();
    type Error = Error;
    fn serialize_field<T: Serialize + ?Sized>(
        &mut self,
        key: &'static str,
        v: &T,
    ) -> Result<()> {
        self.comma();
        write_escaped(self.ser.out, key, self.ser.opts);
        self.ser.out.push(b':');
        v.serialize(&mut *self.ser)
    }
    fn end(self) -> Result<()> {
        self.finish()
    }
}

impl ser::SerializeStructVariant for Compound<'_, '_> {
    type Ok = ();
    type Error = Error;
    fn serialize_field<T: Serialize + ?Sized>(
        &mut self,
        key: &'static str,
        v: &T,
    ) -> Result<()> {
        self.comma();
        write_escaped(self.ser.out, key, self.ser.opts);
        self.ser.out.push(b':');
        v.serialize(&mut *self.ser)
    }
    fn end(self) -> Result<()> {
        self.finish()
    }
}

/// Accepts only the types that can legally be a JSON object key.
struct KeySerializer<'a, 'b> {
    ser: &'b mut Serializer<'a>,
}

macro_rules! key_signed {
    ($($m:ident: $t:ty),* $(,)?) => {$(
        fn $m(self, v: $t) -> Result<()> {
            self.ser.out.push(b'"');
            if v < 0 {
                self.ser.out.push(b'-');
            }
            write_u64(self.ser.out, u64::from(v.unsigned_abs()));
            self.ser.out.push(b'"');
            Ok(())
        }
    )*};
}

macro_rules! key_unsigned {
    ($($m:ident: $t:ty),* $(,)?) => {$(
        fn $m(self, v: $t) -> Result<()> {
            self.ser.out.push(b'"');
            write_u64(self.ser.out, u64::from(v));
            self.ser.out.push(b'"');
            Ok(())
        }
    )*};
}

impl ser::Serializer for KeySerializer<'_, '_> {
    type Ok = ();
    type Error = Error;
    type SerializeSeq = ser::Impossible<(), Error>;
    type SerializeTuple = ser::Impossible<(), Error>;
    type SerializeTupleStruct = ser::Impossible<(), Error>;
    type SerializeTupleVariant = ser::Impossible<(), Error>;
    type SerializeMap = ser::Impossible<(), Error>;
    type SerializeStruct = ser::Impossible<(), Error>;
    type SerializeStructVariant = ser::Impossible<(), Error>;

    fn serialize_str(self, v: &str) -> Result<()> {
        write_escaped(self.ser.out, v, self.ser.opts);
        Ok(())
    }

    fn serialize_char(self, v: char) -> Result<()> {
        let mut buf = [0u8; 4];
        self.serialize_str(v.encode_utf8(&mut buf))
    }

    fn serialize_unit_variant(
        self,
        _name: &'static str,
        _idx: u32,
        variant: &'static str,
    ) -> Result<()> {
        self.serialize_str(variant)
    }

    fn serialize_newtype_struct<T: Serialize + ?Sized>(
        self,
        _name: &'static str,
        v: &T,
    ) -> Result<()> {
        v.serialize(self)
    }

    key_signed! {
        serialize_i8: i8, serialize_i16: i16, serialize_i32: i32, serialize_i64: i64,
    }
    key_unsigned! {
        serialize_u8: u8, serialize_u16: u16, serialize_u32: u32, serialize_u64: u64,
    }

    fn serialize_bool(self, _: bool) -> Result<()> {
        Err(err("object key must be a string"))
    }
    fn serialize_i128(self, _: i128) -> Result<()> {
        Err(err("object key must be a string"))
    }
    fn serialize_u128(self, _: u128) -> Result<()> {
        Err(err("object key must be a string"))
    }
    fn serialize_f32(self, _: f32) -> Result<()> {
        Err(err("object key must be a string"))
    }
    fn serialize_f64(self, _: f64) -> Result<()> {
        Err(err("object key must be a string"))
    }
    fn serialize_bytes(self, _: &[u8]) -> Result<()> {
        Err(err("object key must be a string"))
    }
    fn serialize_none(self) -> Result<()> {
        Err(err("object key must be a string"))
    }
    fn serialize_some<T: Serialize + ?Sized>(self, _: &T) -> Result<()> {
        Err(err("object key must be a string"))
    }
    fn serialize_unit(self) -> Result<()> {
        Err(err("object key must be a string"))
    }
    fn serialize_unit_struct(self, _: &'static str) -> Result<()> {
        Err(err("object key must be a string"))
    }
    fn serialize_newtype_variant<T: Serialize + ?Sized>(
        self,
        _: &'static str,
        _: u32,
        _: &'static str,
        _: &T,
    ) -> Result<()> {
        Err(err("object key must be a string"))
    }
    fn serialize_seq(self, _: Option<usize>) -> Result<Self::SerializeSeq> {
        Err(err("object key must be a string"))
    }
    fn serialize_tuple(self, _: usize) -> Result<Self::SerializeTuple> {
        Err(err("object key must be a string"))
    }
    fn serialize_tuple_struct(
        self,
        _: &'static str,
        _: usize,
    ) -> Result<Self::SerializeTupleStruct> {
        Err(err("object key must be a string"))
    }
    fn serialize_tuple_variant(
        self,
        _: &'static str,
        _: u32,
        _: &'static str,
        _: usize,
    ) -> Result<Self::SerializeTupleVariant> {
        Err(err("object key must be a string"))
    }
    fn serialize_map(self, _: Option<usize>) -> Result<Self::SerializeMap> {
        Err(err("object key must be a string"))
    }
    fn serialize_struct(self, _: &'static str, _: usize) -> Result<Self::SerializeStruct> {
        Err(err("object key must be a string"))
    }
    fn serialize_struct_variant(
        self,
        _: &'static str,
        _: u32,
        _: &'static str,
        _: usize,
    ) -> Result<Self::SerializeStructVariant> {
        Err(err("object key must be a string"))
    }
}
