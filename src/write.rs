//! Writing JSON into a buffer you own, with no allocation.
//!
//! [`crate::to_string`] builds a `String`. That is the right default, and
//! wrong in two places: when the output goes somewhere that is not a
//! `String` anyway, and when there is no allocator.
//!
//! This is the append form — the analogue of C's `appendf` into a caller's
//! buffer. You supply the storage, it never grows it, and it tells you if
//! the value did not fit.
//!
//! ```
//! use dacode_json::write::Writer;
//!
//! let mut buf = [0u8; 128];
//! let mut w = Writer::new(&mut buf);
//!
//! w.begin_object()?;
//! w.key("id")?.u64(7)?;
//! w.key("name")?.str("alpha \"quoted\"")?;
//! w.key("tags")?.begin_array()?;
//! w.str("a")?;
//! w.str("b")?;
//! w.end_array()?;
//! w.end_object()?;
//!
//! assert_eq!(
//!     w.finish()?,
//!     r#"{"id":7,"name":"alpha \"quoted\"","tags":["a","b"]}"#
//! );
//! # Ok::<(), dacode_json::write::Error>(())
//! ```
//!
//! # What it does and does not check
//!
//! Commas and colons are placed for you, and strings are escaped properly,
//! so the output is always well-formed JSON *if the calls are balanced*.
//! Unbalanced calls — ending an array you did not begin, finishing inside
//! an object — are errors, not silent corruption: [`Writer::finish`] fails
//! unless every container was closed.
//!
//! It does not deduplicate object keys, because neither does JSON.
//!
//! # Cost
//!
//! No allocation, no intermediate. Numbers are formatted into the output
//! directly. A rejected write leaves the buffer unchanged, so an overflow
//! can be recovered from by flushing and retrying.

use core::fmt;

/// Why a write failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// The value did not fit. The buffer is unchanged.
    Overflow,
    /// A container was closed that was not open, or the wrong kind was
    /// closed, or `finish` was called with something still open.
    Unbalanced,
    /// A key was written outside an object, or a value where a key was
    /// expected.
    OutOfOrder,
    /// Nesting exceeded [`MAX_DEPTH`].
    TooDeep,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Error::Overflow => "output buffer is too small",
            Error::Unbalanced => "unbalanced object or array",
            Error::OutOfOrder => "key or value written in the wrong position",
            Error::TooDeep => "nesting too deep",
        })
    }
}

impl core::error::Error for Error {}

type Result<T> = core::result::Result<T, Error>;

/// How deep containers may nest. Fixed, so the writer needs no heap.
pub const MAX_DEPTH: usize = 32;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Frame {
    Object,
    Array,
}

/// Appends JSON to a fixed buffer.
pub struct Writer<'a> {
    buf: &'a mut [u8],
    pos: usize,
    stack: [Frame; MAX_DEPTH],
    depth: usize,
    /// A separator is needed before the next item.
    need_comma: bool,
    /// Inside an object, a key has been written and a value is due.
    expect_value: bool,
}

impl fmt::Debug for Writer<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Writer")
            .field("written", &self.pos)
            .field("capacity", &self.buf.len())
            .field("depth", &self.depth)
            .finish()
    }
}

impl<'a> Writer<'a> {
    #[must_use]
    pub fn new(buf: &'a mut [u8]) -> Self {
        Writer {
            buf,
            pos: 0,
            stack: [Frame::Object; MAX_DEPTH],
            depth: 0,
            need_comma: false,
            expect_value: false,
        }
    }

    /// Bytes written so far.
    #[must_use]
    pub fn len(&self) -> usize {
        self.pos
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.pos == 0
    }

    /// The finished document.
    ///
    /// Fails if any container is still open.
    pub fn finish(self) -> Result<&'a str> {
        if self.depth != 0 || self.expect_value {
            return Err(Error::Unbalanced);
        }
        let end = self.pos;
        let bytes = self.buf.get(..end).ok_or(Error::Overflow)?;
        // Everything written here is ASCII or valid UTF-8 copied from a
        // `&str`, so this cannot fail; it is checked rather than asserted.
        core::str::from_utf8(bytes).map_err(|_| Error::Overflow)
    }

    /// The finished document as bytes, for callers that do not want the
    /// UTF-8 check.
    pub fn finish_bytes(self) -> Result<&'a [u8]> {
        if self.depth != 0 || self.expect_value {
            return Err(Error::Unbalanced);
        }
        let end = self.pos;
        self.buf.get(..end).ok_or(Error::Overflow)
    }

    /// Run a write as all-or-nothing.
    ///
    /// A value write is several steps — separator, then bytes — and any of
    /// them can overflow. Rolling back only the bytes leaves the separator
    /// behind, which produced `[,1]` after a rejected string. So the whole
    /// cursor state is snapshotted, not just the position.
    #[inline]
    fn atomic(&mut self, f: impl FnOnce(&mut Self) -> Result<()>) -> Result<&mut Self> {
        let saved = (self.pos, self.need_comma, self.expect_value, self.depth);
        match f(self) {
            Ok(()) => Ok(self),
            Err(e) => {
                self.pos = saved.0;
                self.need_comma = saved.1;
                self.expect_value = saved.2;
                self.depth = saved.3;
                Err(e)
            }
        }
    }

    // --- raw output -------------------------------------------------

    #[inline]
    fn put(&mut self, bytes: &[u8]) -> Result<()> {
        let end = self.pos.checked_add(bytes.len()).ok_or(Error::Overflow)?;
        let dst = self.buf.get_mut(self.pos..end).ok_or(Error::Overflow)?;
        dst.copy_from_slice(bytes);
        self.pos = end;
        Ok(())
    }

    #[inline]
    fn put_byte(&mut self, b: u8) -> Result<()> {
        let slot = self.buf.get_mut(self.pos).ok_or(Error::Overflow)?;
        *slot = b;
        self.pos += 1;
        Ok(())
    }

    /// Emit the separator due before the next value.
    #[inline]
    fn sep(&mut self) -> Result<()> {
        if self.expect_value {
            // The colon was already written by `key`.
            self.expect_value = false;
            return Ok(());
        }
        if self.depth > 0 && matches!(self.stack.get(self.depth - 1), Some(Frame::Object)) {
            // A value in an object must follow a key.
            return Err(Error::OutOfOrder);
        }
        if self.need_comma {
            self.put_byte(b',')?;
        }
        self.need_comma = true;
        Ok(())
    }

    // --- containers -------------------------------------------------

    fn push(&mut self, frame: Frame, open: u8) -> Result<&mut Self> {
        self.atomic(|w| {
            if w.depth >= MAX_DEPTH {
                return Err(Error::TooDeep);
            }
            w.sep()?;
            w.put_byte(open)?;
            let slot = w.stack.get_mut(w.depth).ok_or(Error::TooDeep)?;
            *slot = frame;
            w.depth += 1;
            w.need_comma = false;
            Ok(())
        })
    }

    fn pop(&mut self, frame: Frame, close: u8) -> Result<&mut Self> {
        self.atomic(|w| {
            if w.expect_value {
                return Err(Error::Unbalanced);
            }
            let depth = w.depth.checked_sub(1).ok_or(Error::Unbalanced)?;
            if w.stack.get(depth) != Some(&frame) {
                return Err(Error::Unbalanced);
            }
            w.put_byte(close)?;
            w.depth = depth;
            w.need_comma = true;
            Ok(())
        })
    }

    pub fn begin_object(&mut self) -> Result<&mut Self> {
        self.push(Frame::Object, b'{')
    }

    pub fn end_object(&mut self) -> Result<&mut Self> {
        self.pop(Frame::Object, b'}')
    }

    pub fn begin_array(&mut self) -> Result<&mut Self> {
        self.push(Frame::Array, b'[')
    }

    pub fn end_array(&mut self) -> Result<&mut Self> {
        self.pop(Frame::Array, b']')
    }

    /// Write an object key. The next call must write a value.
    pub fn key(&mut self, k: &str) -> Result<&mut Self> {
        if self.depth == 0 || !matches!(self.stack.get(self.depth - 1), Some(Frame::Object)) {
            return Err(Error::OutOfOrder);
        }
        if self.expect_value {
            return Err(Error::OutOfOrder);
        }
        self.atomic(|w| {
            if w.need_comma {
                w.put_byte(b',')?;
            }
            w.write_escaped(k)?;
            w.put_byte(b':')?;
            w.need_comma = true;
            w.expect_value = true;
            Ok(())
        })
    }

    // --- values -----------------------------------------------------

    pub fn null(&mut self) -> Result<&mut Self> {
        self.atomic(|w| {
            w.sep()?;
            w.put(b"null")
        })
    }

    pub fn bool(&mut self, v: bool) -> Result<&mut Self> {
        self.atomic(|w| {
            w.sep()?;
            w.put(if v {
                b"true".as_slice()
            } else {
                b"false".as_slice()
            })
        })
    }

    pub fn u64(&mut self, v: u64) -> Result<&mut Self> {
        self.atomic(|w| {
            w.sep()?;
            let mut tmp = [0u8; 20];
            w.put(format_u64(v, &mut tmp))
        })
    }

    pub fn i64(&mut self, v: i64) -> Result<&mut Self> {
        self.atomic(|w| {
            w.sep()?;
            let mut tmp = [0u8; 21];
            w.put(format_i64(v, &mut tmp))
        })
    }

    /// Write an `f64`.
    ///
    /// Non-finite values are not representable in JSON and are rejected
    /// rather than emitted as `NaN`, which no parser accepts.
    pub fn f64(&mut self, v: f64) -> Result<&mut Self> {
        if !v.is_finite() {
            return Err(Error::OutOfOrder);
        }
        self.atomic(|w| {
            w.sep()?;
            // `zmij::Buffer` is a stack buffer, so this allocates nothing,
            // and it is the same formatter `to_string` uses — so the two
            // writers agree byte for byte, and both match `serde_json`.
            let mut tmp = zmij::Buffer::new();
            w.put(tmp.format_finite(v).as_bytes())
        })
    }

    pub fn str(&mut self, s: &str) -> Result<&mut Self> {
        self.atomic(|w| {
            w.sep()?;
            w.write_escaped(s)
        })
    }

    /// Write a value that is already JSON, without checking it.
    ///
    /// For splicing a cached fragment. The caller is responsible for it
    /// being valid; nothing else here can produce invalid output.
    pub fn raw_json(&mut self, json: &str) -> Result<&mut Self> {
        self.atomic(|w| {
            w.sep()?;
            w.put(json.as_bytes())
        })
    }

    fn write_escaped(&mut self, s: &str) -> Result<()> {
        self.put_byte(b'"')?;
        let bytes = s.as_bytes();
        let mut run = 0usize;
        for (i, &b) in bytes.iter().enumerate() {
            let esc: &[u8] = match b {
                b'"' => b"\\\"",
                b'\\' => b"\\\\",
                0x08 => b"\\b",
                0x0c => b"\\f",
                b'\n' => b"\\n",
                b'\r' => b"\\r",
                b'\t' => b"\\t",
                0x00..=0x1f => {
                    // Copy the clean run, then the \u00XX form.
                    if let Some(chunk) = bytes.get(run..i) {
                        self.put(chunk)?;
                    }
                    const HEX: &[u8; 16] = b"0123456789abcdef";
                    let hi = HEX.get(usize::from(b >> 4)).copied().unwrap_or(b'0');
                    let lo = HEX.get(usize::from(b & 0xf)).copied().unwrap_or(b'0');
                    self.put(&[b'\\', b'u', b'0', b'0', hi, lo])?;
                    run = i + 1;
                    continue;
                }
                _ => continue,
            };
            if let Some(chunk) = bytes.get(run..i) {
                self.put(chunk)?;
            }
            self.put(esc)?;
            run = i + 1;
        }
        if let Some(chunk) = bytes.get(run..) {
            self.put(chunk)?;
        }
        self.put_byte(b'"')?;
        Ok(())
    }
}

// =====================================================================
// Number formatting, no allocation
// =====================================================================

fn format_u64(mut v: u64, out: &mut [u8; 20]) -> &[u8] {
    if v == 0 {
        if let Some(slot) = out.first_mut() {
            *slot = b'0';
        }
        return out.get(..1).unwrap_or(&[]);
    }
    let mut i = out.len();
    while v > 0 {
        i -= 1;
        if let Some(slot) = out.get_mut(i) {
            *slot = b'0' + (v % 10) as u8;
        }
        v /= 10;
    }
    out.get(i..).unwrap_or(&[])
}

fn format_i64(v: i64, out: &mut [u8; 21]) -> &[u8] {
    let neg = v < 0;
    let mag = v.unsigned_abs();
    let mut tmp = [0u8; 20];
    let digits = format_u64(mag, &mut tmp);
    let mut n = 0usize;
    if neg {
        if let Some(slot) = out.get_mut(0) {
            *slot = b'-';
        }
        n = 1;
    }
    let end = n + digits.len();
    if let Some(dst) = out.get_mut(n..end) {
        dst.copy_from_slice(digits);
    }
    out.get(..end).unwrap_or(&[])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn build(f: impl FnOnce(&mut Writer<'_>) -> Result<()>) -> String {
        let mut buf = [0u8; 512];
        let mut w = Writer::new(&mut buf);
        f(&mut w).expect("write");
        w.finish().expect("finish").to_owned()
    }

    #[test]
    fn matches_serde_json_byte_for_byte() {
        let out = build(|w| {
            w.begin_object()?;
            w.key("id")?.u64(7)?;
            w.key("neg")?.i64(-42)?;
            w.key("f")?.f64(3.25)?;
            w.key("t")?.bool(true)?;
            w.key("n")?.null()?;
            w.key("s")?.str("a\"b\\c\nd\te")?;
            w.key("arr")?.begin_array()?;
            w.u64(1)?;
            w.u64(2)?;
            w.end_array()?;
            w.end_object()?;
            Ok(())
        });

        let want = serde_json::json!({
            "id": 7, "neg": -42, "f": 3.25, "t": true, "n": null,
            "s": "a\"b\\c\nd\te", "arr": [1, 2]
        });
        // Compare as values, since object key order in `json!` is not ours.
        let got: serde_json::Value = serde_json::from_str(&out).expect("valid JSON");
        assert_eq!(got, want, "output was {out}");

        // And the escaping itself must be byte-identical to serde_json's.
        let s_ours = build(|w| {
            w.str("a\"b\\c\nd\te\u{1}")?;
            Ok(())
        });
        assert_eq!(
            s_ours,
            serde_json::to_string("a\"b\\c\nd\te\u{1}").expect("ser")
        );
    }

    #[test]
    fn round_trips_through_the_parser() {
        let out = build(|w| {
            w.begin_array()?;
            w.begin_object()?;
            w.key("k")?.str("v")?;
            w.end_object()?;
            w.f64(-0.5)?;
            w.i64(i64::MIN)?;
            w.u64(u64::MAX)?;
            w.end_array()?;
            Ok(())
        });
        let v: serde_json::Value = crate::from_slice(out.as_bytes()).expect("parses");
        assert_eq!(v[3].as_u64(), Some(u64::MAX));
        assert_eq!(v[2].as_i64(), Some(i64::MIN));
    }

    #[test]
    fn overflow_is_reported_and_leaves_the_buffer_usable() {
        let mut buf = [0u8; 8];
        let mut w = Writer::new(&mut buf);
        w.begin_array().expect("open");
        // Does not fit.
        assert_eq!(w.str("a very long string").err(), Some(Error::Overflow));
        // The rejected write left nothing behind, so a short one still fits.
        w.u64(1).expect("short value fits");
        w.end_array().expect("close");
        assert_eq!(w.finish().expect("finish"), "[1]");
    }

    #[test]
    fn unbalanced_is_an_error_not_bad_output() {
        let mut buf = [0u8; 64];
        let mut w = Writer::new(&mut buf);
        w.begin_object().expect("open");
        assert_eq!(w.end_array().err(), Some(Error::Unbalanced));

        let mut buf = [0u8; 64];
        let mut w = Writer::new(&mut buf);
        w.begin_object().expect("open");
        assert_eq!(w.finish(), Err(Error::Unbalanced));
    }

    #[test]
    fn a_value_needs_a_key_inside_an_object() {
        let mut buf = [0u8; 64];
        let mut w = Writer::new(&mut buf);
        w.begin_object().expect("open");
        assert_eq!(w.u64(1).err(), Some(Error::OutOfOrder));
    }

    #[test]
    fn non_finite_floats_are_rejected() {
        let mut buf = [0u8; 64];
        let mut w = Writer::new(&mut buf);
        w.begin_array().expect("open");
        assert!(w.f64(f64::NAN).is_err());
        assert!(w.f64(f64::INFINITY).is_err());
    }

    #[test]
    fn integer_formatting_matches_the_library() {
        for v in [0u64, 1, 9, 10, 99, u64::MAX, 12345678901234567890] {
            let mut t = [0u8; 20];
            assert_eq!(
                core::str::from_utf8(format_u64(v, &mut t)).expect("ascii"),
                v.to_string()
            );
        }
        for v in [0i64, -1, 1, i64::MIN, i64::MAX, -9999999999] {
            let mut t = [0u8; 21];
            assert_eq!(
                core::str::from_utf8(format_i64(v, &mut t)).expect("ascii"),
                v.to_string()
            );
        }
    }
}
