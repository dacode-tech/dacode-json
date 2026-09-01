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
//! depth is bounded by [`MAX_DEPTH`].

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

    /// Convert to `i64`, validating the number.
    #[must_use]
    pub fn as_i64(&self) -> Option<i64> {
        match self.num()? {
            Num::I(v) => Some(v),
            Num::U(v) => i64::try_from(v).ok(),
            Num::F(v) => {
                // Accept an integral float, as `serde_json`'s `as_i64` does not.
                if v.fract() == 0.0 && v.abs() < 9.223_372_036_854_776e18 {
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
    #[must_use]
    pub fn as_str(&self) -> Option<std::borrow::Cow<'de, str>> {
        if self.kind != Kind::Str {
            return None;
        }
        crate::unescape::unescape_checked(self.bytes)
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
#[derive(Debug)]
///
/// Stop early — `break` out of the loop — and the rest of the object is
/// skipped in one go rather than field by field. That is the whole point.
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
}
