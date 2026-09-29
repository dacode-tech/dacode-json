//! Schema-driven layout: the same wire format, with field names removed.
//!
//! [`super::View`] is self-describing — it stores every key string, so any
//! consumer can read it without knowing the shape. That costs space
//! (1.30× the source JSON, measured in `docs/ZEROCOPY.md`) and makes field
//! access a binary search over stored keys.
//!
//! When both ends know the type, none of that is necessary. A field's
//! position is a compile-time constant, so keys need not be stored at all
//! and a read is a load at a fixed offset. That is what `rkyv` and
//! FlatBuffers do, and it is why `rkyv` reaches 0.59× where the dynamic
//! format sits at 1.30×.
//!
//! # Why the layout is a type, not a Cargo feature
//!
//! Selecting it with `#[cfg(feature = ...)]` looks natural and is a trap:
//!
//! * **Features are additive and global.** One crate anywhere in the
//!   dependency graph enabling `dynamic` silently switches every other
//!   crate over. `rkyv` shipped `size_16`/`size_32` as mutually exclusive
//!   features and had to move them to generics in 0.8 because the
//!   combination was unbuildable ([rkyv#67](https://github.com/rkyv/rkyv/issues/67)).
//! * **The choice is per-message.** A schema for a hot RPC type and dynamic
//!   for a config blob, in one binary, is a normal requirement.
//! * **It is a wire hazard.** Two builds of the same program would produce
//!   mutually unreadable buffers with nothing to detect it.
//!
//! So the layout is chosen by *which type you construct*, and the header
//! records which was written ([`super::FLAG_TYPED`]) along with a schema
//! hash. [`super::View`] refuses a typed buffer, [`TypedView`] refuses a
//! dynamic one or a mismatched schema, both with a real error.
//!
//! A Cargo feature still has a job here — gating a `derive` macro and its
//! `syn`/`quote` compile cost, the way `serde` does. That is additive and
//! safe. [`crate::flat_struct!`] is the `macro_rules!` stand-in, so today
//! the crate
//! needs no proc-macro dependency at all.
//!
//! # Layout
//!
//! ```text
//! 0..32    standard 32-byte header (FLAG_TYPED set)
//! 32..40   schema id (u64)
//! 40..48   record count (u64)
//! 48..     records: fixed stride, one 8-byte slot per field
//!          string-offset table (u32 pairs, for list fields)
//!          byte blob (UTF-8, interned)
//! ```
//!
//! `record[i].field[f]` lives at `48 + i * stride + f * 8` — a
//! multiply-add, no indirection and no search.
//!
//! | field type | slot contents |
//! |---|---|
//! | `u64` `i64` `u32` `i32` `f64` `bool` | the value, zero/sign-extended to 64 bits |
//! | `str` | `u32` blob offset, `u32` byte length |
//! | `[str]` | `u32` index into the offset table, `u32` count |
//!
//! Eight-byte slots waste seven bytes on a `bool`. Packing by size would
//! recover it; the simple version is measured first and `docs/ZEROCOPY.md`
//! records what it costs.

use super::{Error, HEADER, MAGIC, VERSION};

#[cfg(feature = "alloc")]
use alloc::boxed::Box;
#[cfg(feature = "alloc")]
use alloc::vec::Vec;

#[cfg(feature = "alloc")]
use super::Interner;

/// Bytes per field slot.
pub const SLOT: usize = 8;

/// Offset of the schema id.
const SCHEMA_OFF: usize = HEADER;
/// Offset of the record count.
const COUNT_OFF: usize = HEADER + 8;
/// Offset of the first record.
const RECORDS_OFF: usize = HEADER + 16;

/// How one field is stored in its slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FieldKind {
    /// 64-bit value stored directly.
    Scalar64,
    /// `(u32 offset, u32 len)` into the byte blob.
    Str,
    /// `(u32 index, u32 count)` into the string-offset table.
    StrList,
}

/// A type with a fixed, compile-time-known field layout.
///
/// Implemented by [`crate::flat_struct!`].
pub trait FlatSchema {
    /// Field kinds, in slot order.
    const FIELDS: &'static [FieldKind];
    /// Field names. **Not stored in the buffer** — they exist only to
    /// compute [`FlatSchema::SCHEMA_ID`] and to name the accessors.
    const NAMES: &'static [&'static str];
    /// Identifies the schema so a buffer cannot be read as the wrong type.
    ///
    /// Derived from names and kinds, so adding, removing, reordering or
    /// retyping a field changes it and the mismatch is caught by
    /// [`TypedView::new`] instead of producing silent garbage.
    const SCHEMA_ID: u64 = schema_id(Self::NAMES, Self::FIELDS);
    /// Bytes per record.
    const STRIDE: usize = Self::FIELDS.len() * SLOT;
}

/// FNV-1a over field names and kinds.
///
/// Indexing is deliberate: this is a `const fn` evaluated at compile time,
/// so an out-of-bounds index is a *compile error* rather than a runtime
/// panic — strictly stronger than `get`, and free. All three indices are
/// bounded by the `while` conditions.
#[allow(clippy::indexing_slicing)]
#[must_use]
pub const fn schema_id(names: &[&str], fields: &[FieldKind]) -> u64 {
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    let mut i = 0;
    while i < names.len() {
        let b = names[i].as_bytes();
        let mut j = 0;
        while j < b.len() {
            h ^= b[j] as u64;
            h = h.wrapping_mul(PRIME);
            j += 1;
        }
        // Separator, so ("ab","c") and ("a","bc") hash differently.
        h ^= 0xff;
        h = h.wrapping_mul(PRIME);
        i += 1;
    }
    let mut i = 0;
    while i < fields.len() {
        h ^= match fields[i] {
            FieldKind::Scalar64 => 1u64,
            FieldKind::Str => 2,
            FieldKind::StrList => 3,
        };
        h = h.wrapping_mul(PRIME);
        i += 1;
    }
    h
}

// =====================================================================
// Writer
// =====================================================================

/// Builds a typed buffer.
#[cfg(feature = "alloc")]
#[derive(Debug)]
pub struct TypedWriter<T: FlatSchema> {
    records: Vec<u8>,
    blob: Vec<u8>,
    offsets: Vec<u32>,
    count: u64,
    seen: Interner<Box<str>>,
    intern: bool,
    _marker: core::marker::PhantomData<fn() -> T>,
}

#[cfg(feature = "alloc")]
impl<T: FlatSchema> Default for TypedWriter<T> {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(feature = "alloc")]
impl<T: FlatSchema> TypedWriter<T> {
    #[must_use]
    pub fn new() -> Self {
        TypedWriter {
            records: Vec::new(),
            blob: Vec::new(),
            offsets: Vec::new(),
            count: 0,
            seen: Interner::new(),
            intern: true,
            _marker: core::marker::PhantomData,
        }
    }

    /// Deduplicate identical strings in the blob. On by default: a large win
    /// for low-cardinality fields, and nil otherwise.
    #[must_use]
    pub fn intern(mut self, yes: bool) -> Self {
        self.intern = yes;
        self
    }

    /// Start a record. Fields must then be written in declaration order.
    pub fn record(&mut self) -> RecordWriter<'_, T> {
        let base = self.records.len();
        self.records.resize(base + T::STRIDE, 0);
        self.count += 1;
        RecordWriter {
            w: self,
            base,
            slot: 0,
        }
    }

    fn put_str(&mut self, s: &str) -> (u32, u32) {
        if self.intern {
            if let Some(&hit) = self.seen.get(s) {
                return hit;
            }
        }
        let off = u32::try_from(self.blob.len()).unwrap_or(u32::MAX);
        let len = u32::try_from(s.len()).unwrap_or(u32::MAX);
        self.blob.extend_from_slice(s.as_bytes());
        if self.intern {
            self.seen.insert(s.into(), (off, len));
        }
        (off, len)
    }

    /// Serialize.
    #[must_use]
    pub fn finish(self) -> Vec<u8> {
        let offsets_off = RECORDS_OFF + self.records.len();
        let blob_off = offsets_off + self.offsets.len() * 4;
        let total = blob_off + self.blob.len();

        let u32_or_max = |v: usize| u32::try_from(v).unwrap_or(u32::MAX);

        let mut out = Vec::with_capacity(total);
        out.extend_from_slice(&MAGIC.to_le_bytes()); // 0
        out.extend_from_slice(&VERSION.to_le_bytes()); // 4
        out.extend_from_slice(&super::FLAG_TYPED.to_le_bytes()); // 6
        out.extend_from_slice(&u32_or_max(T::FIELDS.len()).to_le_bytes()); // 8
        out.extend_from_slice(&0u32.to_le_bytes()); // 12 root, unused
        out.extend_from_slice(&u32_or_max(offsets_off).to_le_bytes()); // 16
        out.extend_from_slice(&u32_or_max(blob_off).to_le_bytes()); // 20
        out.extend_from_slice(&0u32.to_le_bytes()); // 24 unused
        out.extend_from_slice(&u32_or_max(total).to_le_bytes()); // 28
        debug_assert_eq!(out.len(), HEADER);

        out.extend_from_slice(&T::SCHEMA_ID.to_le_bytes());
        out.extend_from_slice(&self.count.to_le_bytes());
        out.extend_from_slice(&self.records);
        for o in &self.offsets {
            out.extend_from_slice(&o.to_le_bytes());
        }
        out.extend_from_slice(&self.blob);
        debug_assert_eq!(out.len(), total);
        out
    }
}

/// Cursor for one record's fields, written in order.
#[cfg(feature = "alloc")]
#[derive(Debug)]
pub struct RecordWriter<'a, T: FlatSchema> {
    w: &'a mut TypedWriter<T>,
    base: usize,
    slot: usize,
}

#[cfg(feature = "alloc")]
impl<T: FlatSchema> RecordWriter<'_, T> {
    fn write8(&mut self, v: u64) {
        let at = self.base + self.slot * SLOT;
        self.slot += 1;
        if let Some(dst) = self.w.records.get_mut(at..at + 8) {
            dst.copy_from_slice(&v.to_le_bytes());
        }
    }

    /// Any 64-bit scalar, already encoded.
    pub fn scalar(&mut self, v: u64) -> &mut Self {
        self.write8(v);
        self
    }
    pub fn u64(&mut self, v: u64) -> &mut Self {
        self.scalar(v)
    }
    pub fn i64(&mut self, v: i64) -> &mut Self {
        self.scalar(v as u64)
    }
    pub fn u32(&mut self, v: u32) -> &mut Self {
        self.scalar(u64::from(v))
    }
    pub fn i32(&mut self, v: i32) -> &mut Self {
        self.scalar(i64::from(v) as u64)
    }
    pub fn f64(&mut self, v: f64) -> &mut Self {
        self.scalar(v.to_bits())
    }
    pub fn bool(&mut self, v: bool) -> &mut Self {
        self.scalar(u64::from(v))
    }

    pub fn str(&mut self, s: &str) -> &mut Self {
        let (off, len) = self.w.put_str(s);
        self.write8(u64::from(off) | (u64::from(len) << 32));
        self
    }

    pub fn str_list<'s, I: IntoIterator<Item = &'s str>>(&mut self, items: I) -> &mut Self {
        let table_start = u32::try_from(self.w.offsets.len() / 2).unwrap_or(u32::MAX);
        let mut n = 0u32;
        for s in items {
            let (off, len) = self.w.put_str(s);
            self.w.offsets.push(off);
            self.w.offsets.push(len);
            n += 1;
        }
        self.write8(u64::from(table_start) | (u64::from(n) << 32));
        self
    }
}

// =====================================================================
// Reader
// =====================================================================

/// A validated, borrowed view over a typed buffer.
///
/// Opening is O(1): header check plus a schema-id comparison. Field access
/// is a load at a computed offset — no search, no decoding, no allocation.
///
/// `Clone`/`Copy` are implemented by hand: deriving them would add a
/// `T: Copy` bound through the `PhantomData`, and a schema marker has no
/// reason to be `Copy`.
#[derive(Debug)]
pub struct TypedView<'a, T: FlatSchema> {
    records: &'a [u8],
    offsets: &'a [u8],
    blob: &'a [u8],
    count: usize,
    _marker: core::marker::PhantomData<fn() -> T>,
}

impl<T: FlatSchema> Clone for TypedView<'_, T> {
    fn clone(&self) -> Self {
        *self
    }
}
impl<T: FlatSchema> Copy for TypedView<'_, T> {}

#[inline]
fn rd_u16(b: &[u8], off: usize) -> Option<u16> {
    let s = b.get(off..off + 2)?;
    Some(u16::from_le_bytes(<[u8; 2]>::try_from(s).ok()?))
}
#[inline]
fn rd_u32(b: &[u8], off: usize) -> Option<u32> {
    let s = b.get(off..off + 4)?;
    Some(u32::from_le_bytes(<[u8; 4]>::try_from(s).ok()?))
}
#[inline]
fn rd_u64(b: &[u8], off: usize) -> Option<u64> {
    let s = b.get(off..off + 8)?;
    Some(u64::from_le_bytes(<[u8; 8]>::try_from(s).ok()?))
}

impl<'a, T: FlatSchema> TypedView<'a, T> {
    /// Validate the header, the layout flag and the schema id.
    pub fn new(buf: &'a [u8]) -> Result<Self, Error> {
        if buf.len() < RECORDS_OFF {
            return Err(Error::TooShort);
        }
        if rd_u32(buf, 0) != Some(MAGIC) {
            return Err(Error::BadMagic);
        }
        let version = rd_u16(buf, 4).ok_or(Error::TooShort)?;
        if version != VERSION {
            return Err(Error::UnsupportedVersion(version));
        }
        // A dynamic buffer read through a schema would be silent nonsense.
        if rd_u16(buf, 6).ok_or(Error::TooShort)? & super::FLAG_TYPED == 0 {
            return Err(Error::LayoutMismatch);
        }
        if rd_u32(buf, 8).ok_or(Error::TooShort)? as usize != T::FIELDS.len() {
            return Err(Error::SchemaMismatch);
        }
        if rd_u64(buf, SCHEMA_OFF).ok_or(Error::TooShort)? != T::SCHEMA_ID {
            return Err(Error::SchemaMismatch);
        }

        let offsets_off = rd_u32(buf, 16).ok_or(Error::TooShort)? as usize;
        let blob_off = rd_u32(buf, 20).ok_or(Error::TooShort)? as usize;
        let total = rd_u32(buf, 28).ok_or(Error::TooShort)? as usize;

        if total != buf.len() {
            return Err(Error::LengthMismatch);
        }
        if RECORDS_OFF > offsets_off || offsets_off > blob_off || blob_off > total {
            return Err(Error::OutOfBounds);
        }

        let count = rd_u64(buf, COUNT_OFF).ok_or(Error::TooShort)? as usize;
        let records = buf
            .get(RECORDS_OFF..offsets_off)
            .ok_or(Error::OutOfBounds)?;
        if count.checked_mul(T::STRIDE) != Some(records.len()) {
            return Err(Error::DanglingReference);
        }

        Ok(TypedView {
            records,
            offsets: buf.get(offsets_off..blob_off).ok_or(Error::OutOfBounds)?,
            blob: buf.get(blob_off..total).ok_or(Error::OutOfBounds)?,
            count,
            _marker: core::marker::PhantomData,
        })
    }

    /// Number of records.
    #[inline]
    #[must_use]
    pub fn len(&self) -> usize {
        self.count
    }

    #[inline]
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    /// Raw slot for record `i`, field `f`. **O(1)**, one multiply-add.
    #[inline]
    #[must_use]
    pub fn slot(&self, i: usize, f: usize) -> Option<u64> {
        if i >= self.count || f >= T::FIELDS.len() {
            return None;
        }
        rd_u64(self.records, i * T::STRIDE + f * SLOT)
    }

    /// String field, borrowed straight out of the buffer.
    #[inline]
    #[must_use]
    pub fn str_at(&self, i: usize, f: usize) -> Option<&'a str> {
        let packed = self.slot(i, f)?;
        self.blob_str(packed as u32, (packed >> 32) as u32)
    }

    #[inline]
    fn blob_str(&self, off: u32, len: u32) -> Option<&'a str> {
        let s = self
            .blob
            .get(off as usize..(off as usize).checked_add(len as usize)?)?;
        core::str::from_utf8(s).ok()
    }

    /// Handle to a `[str]` field.
    #[inline]
    #[must_use]
    pub fn list_at(&self, i: usize, f: usize) -> StrList<'a> {
        let packed = self.slot(i, f).unwrap_or(0);
        StrList {
            offsets: self.offsets,
            blob: self.blob,
            start: packed as u32 as usize,
            len: (packed >> 32) as usize,
        }
    }
}

/// A list of strings inside a typed buffer.
///
/// Returned by list-field accessors instead of a `_len` / `_get` pair,
/// which would need identifier concatenation to generate.
#[derive(Debug, Clone, Copy)]
pub struct StrList<'a> {
    offsets: &'a [u8],
    blob: &'a [u8],
    start: usize,
    len: usize,
}

impl<'a> StrList<'a> {
    #[inline]
    #[must_use]
    pub fn len(&self) -> usize {
        self.len
    }

    #[inline]
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Item `k`, borrowed from the buffer.
    #[inline]
    #[must_use]
    pub fn get(&self, k: usize) -> Option<&'a str> {
        if k >= self.len {
            return None;
        }
        let base = (self.start + k) * 8;
        let off = rd_u32(self.offsets, base)?;
        let len = rd_u32(self.offsets, base + 4)?;
        let s = self
            .blob
            .get(off as usize..(off as usize).checked_add(len as usize)?)?;
        core::str::from_utf8(s).ok()
    }

    pub fn iter(&self) -> impl Iterator<Item = &'a str> + '_ {
        (0..self.len).filter_map(move |k| self.get(k))
    }
}

impl<'a> IntoIterator for StrList<'a> {
    type Item = &'a str;
    type IntoIter = StrListIter<'a>;
    fn into_iter(self) -> StrListIter<'a> {
        StrListIter { list: self, k: 0 }
    }
}

/// Iterator over a [`StrList`].
#[derive(Debug, Clone)]
pub struct StrListIter<'a> {
    list: StrList<'a>,
    k: usize,
}

impl<'a> Iterator for StrListIter<'a> {
    type Item = &'a str;
    fn next(&mut self) -> Option<&'a str> {
        if self.k >= self.list.len {
            return None;
        }
        let v = self.list.get(self.k);
        self.k += 1;
        v
    }
    fn size_hint(&self) -> (usize, Option<usize>) {
        let rem = self.list.len - self.k;
        (rem, Some(rem))
    }
}

// =====================================================================
// Macro
// =====================================================================

/// Declare a flat schema, plus an extension trait of named accessors.
///
/// ```
/// use dacode_json::flat_struct;
/// use dacode_json::flat::typed::{TypedView, TypedWriter};
///
/// flat_struct! {
///     /// One row of the records corpus.
///     pub struct Record : RecordFields {
///         id: u64,
///         active: bool,
///         name: str,
///         tags: [str],
///     }
/// }
///
/// let mut w = TypedWriter::<Record>::new();
/// w.record().u64(7).bool(true).str("alpha").str_list(["x", "y"]);
/// let buf = w.finish();
///
/// let v = TypedView::<Record>::new(&buf).expect("valid");
/// assert_eq!(v.id(0), Some(7));
/// assert_eq!(v.active(0), Some(true));
/// assert_eq!(v.name(0), Some("alpha"));
/// assert_eq!(v.tags(0).len(), 2);
/// assert_eq!(v.tags(0).get(1), Some("y"));
/// assert_eq!(v.tags(0).into_iter().collect::<Vec<_>>(), ["x", "y"]);
/// ```
///
/// # Why two names
///
/// The accessors have to live on `TypedView<'a, Schema>`, and an *inherent*
/// `impl` on a type from another crate is not allowed — so they go in an
/// extension trait, which needs its own name. `macro_rules!` cannot build
/// `RecordFields` from `Record` (that needs `format_ident!`, i.e. a
/// proc-macro), so the name is given explicitly. A `derive` behind a
/// `derive` feature would generate exactly this and pick the name for you;
/// this keeps the crate free of `syn`/`quote` in the meantime.
///
/// The trait must be in scope to call the accessors, like `std::io::Read`.
#[macro_export]
macro_rules! flat_struct {
    (
        $(#[$meta:meta])*
        $vis:vis struct $name:ident : $acc:ident {
            $($field:ident : $kind:tt),* $(,)?
        }
    ) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
        $vis struct $name;

        impl $crate::flat::typed::FlatSchema for $name {
            const FIELDS: &'static [$crate::flat::typed::FieldKind] =
                &[$($crate::flat_struct!(@kind $kind)),*];
            const NAMES: &'static [&'static str] = &[$(stringify!($field)),*];
        }

        #[doc = concat!("Named field accessors for [`", stringify!($name), "`].")]
        #[doc = ""]
        #[doc = "Must be in scope to call the accessors."]
        $vis trait $acc<'a> {
            $crate::flat_struct!(@decl 0usize; $($field : $kind,)*);
        }

        impl<'a> $acc<'a> for $crate::flat::typed::TypedView<'a, $name> {
            $crate::flat_struct!(@impl 0usize; $($field : $kind,)*);
        }
    };

    // --- field kind -> FieldKind ---
    (@kind u64)   => { $crate::flat::typed::FieldKind::Scalar64 };
    (@kind i64)   => { $crate::flat::typed::FieldKind::Scalar64 };
    (@kind u32)   => { $crate::flat::typed::FieldKind::Scalar64 };
    (@kind i32)   => { $crate::flat::typed::FieldKind::Scalar64 };
    (@kind f64)   => { $crate::flat::typed::FieldKind::Scalar64 };
    (@kind bool)  => { $crate::flat::typed::FieldKind::Scalar64 };
    (@kind str)   => { $crate::flat::typed::FieldKind::Str };
    (@kind [str]) => { $crate::flat::typed::FieldKind::StrList };

    // --- field kind -> accessor return type ---
    (@ret u64)   => { ::core::option::Option<u64> };
    (@ret i64)   => { ::core::option::Option<i64> };
    (@ret u32)   => { ::core::option::Option<u32> };
    (@ret i32)   => { ::core::option::Option<i32> };
    (@ret f64)   => { ::core::option::Option<f64> };
    (@ret bool)  => { ::core::option::Option<bool> };
    (@ret str)   => { ::core::option::Option<&'a str> };
    (@ret [str]) => { $crate::flat::typed::StrList<'a> };

    // --- trait declarations ---
    (@decl $i:expr;) => {};
    (@decl $i:expr; $f:ident : $k:tt, $($rest:tt)*) => {
        #[doc = concat!("Field `", stringify!($f), "` of record `i`.")]
        fn $f(&self, i: usize) -> $crate::flat_struct!(@ret $k);
        $crate::flat_struct!(@decl $i + 1usize; $($rest)*);
    };

    // --- trait impls, recursing to carry the slot index ---
    (@impl $i:expr;) => {};

    (@impl $i:expr; $f:ident : u64, $($rest:tt)*) => {
        #[inline]
        fn $f(&self, i: usize) -> ::core::option::Option<u64> { self.slot(i, $i) }
        $crate::flat_struct!(@impl $i + 1usize; $($rest)*);
    };
    (@impl $i:expr; $f:ident : i64, $($rest:tt)*) => {
        #[inline]
        fn $f(&self, i: usize) -> ::core::option::Option<i64> {
            self.slot(i, $i).map(|v| v as i64)
        }
        $crate::flat_struct!(@impl $i + 1usize; $($rest)*);
    };
    (@impl $i:expr; $f:ident : u32, $($rest:tt)*) => {
        #[inline]
        fn $f(&self, i: usize) -> ::core::option::Option<u32> {
            self.slot(i, $i).map(|v| v as u32)
        }
        $crate::flat_struct!(@impl $i + 1usize; $($rest)*);
    };
    (@impl $i:expr; $f:ident : i32, $($rest:tt)*) => {
        #[inline]
        fn $f(&self, i: usize) -> ::core::option::Option<i32> {
            self.slot(i, $i).map(|v| v as i32)
        }
        $crate::flat_struct!(@impl $i + 1usize; $($rest)*);
    };
    (@impl $i:expr; $f:ident : f64, $($rest:tt)*) => {
        #[inline]
        fn $f(&self, i: usize) -> ::core::option::Option<f64> {
            self.slot(i, $i).map(f64::from_bits)
        }
        $crate::flat_struct!(@impl $i + 1usize; $($rest)*);
    };
    (@impl $i:expr; $f:ident : bool, $($rest:tt)*) => {
        #[inline]
        fn $f(&self, i: usize) -> ::core::option::Option<bool> {
            self.slot(i, $i).map(|v| v != 0)
        }
        $crate::flat_struct!(@impl $i + 1usize; $($rest)*);
    };
    (@impl $i:expr; $f:ident : str, $($rest:tt)*) => {
        #[inline]
        fn $f(&self, i: usize) -> ::core::option::Option<&'a str> { self.str_at(i, $i) }
        $crate::flat_struct!(@impl $i + 1usize; $($rest)*);
    };
    (@impl $i:expr; $f:ident : [str], $($rest:tt)*) => {
        #[inline]
        fn $f(&self, i: usize) -> $crate::flat::typed::StrList<'a> { self.list_at(i, $i) }
        $crate::flat_struct!(@impl $i + 1usize; $($rest)*);
    };
}

// =====================================================================
// serde bridge
// =====================================================================

/// Zero-copy `serde` deserialization from a typed buffer.
///
/// This is the piece that makes the format usable as *deserialization* and
/// not just as an accessor API. Given
///
/// ```ignore
/// #[derive(Deserialize)]
/// struct Row<'a> { id: u64, name: &'a str }
/// ```
///
/// a row can be filled with **no copying at all**: numbers come straight
/// out of their slot and `&'a str` borrows the buffer's blob. There is no
/// intermediate `Value`, no `String` allocation, and no parsing.
///
/// The serializing direction is deliberately absent. A writer must produce
/// bytes, so it cannot be zero-copy by definition; [`TypedWriter`] is
/// already the minimal form of it — one pass, one output buffer, interned
/// strings.
#[cfg(feature = "serde")]
pub mod de {
    use super::{FieldKind, FlatSchema, TypedView};
    use alloc::format;
    use alloc::string::{String, ToString};
    use alloc::vec::Vec;
    use core::fmt;
    use serde::de::{self, DeserializeSeed, IntoDeserializer, MapAccess, SeqAccess, Visitor};

    /// Deserialisation failure.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct Error(String);

    impl fmt::Display for Error {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str(&self.0)
        }
    }
    impl core::error::Error for Error {}
    impl de::Error for Error {
        fn custom<T: fmt::Display>(msg: T) -> Self {
            Error(msg.to_string())
        }
    }

    type Result<T> = core::result::Result<T, Error>;

    /// Deserialize record `i` of `view` into `T`.
    pub fn from_row<'de, T, S>(view: &TypedView<'de, S>, i: usize) -> Result<T>
    where
        T: serde::Deserialize<'de>,
        S: FlatSchema,
    {
        if i >= view.len() {
            return Err(Error(format!("row {i} out of range ({} rows)", view.len())));
        }
        T::deserialize(RowDeserializer {
            view: *view,
            row: i,
        })
    }

    /// Deserialize every record into a `Vec<T>`.
    pub fn from_all<'de, T, S>(view: &TypedView<'de, S>) -> Result<Vec<T>>
    where
        T: serde::Deserialize<'de>,
        S: FlatSchema,
    {
        (0..view.len()).map(|i| from_row(view, i)).collect()
    }

    /// One record, presented to serde as a map keyed by the schema's field
    /// names.
    #[derive(Debug)]
    pub struct RowDeserializer<'de, S: FlatSchema> {
        view: TypedView<'de, S>,
        row: usize,
    }

    impl<S: FlatSchema> Clone for RowDeserializer<'_, S> {
        fn clone(&self) -> Self {
            *self
        }
    }
    impl<S: FlatSchema> Copy for RowDeserializer<'_, S> {}

    impl<'de, S: FlatSchema> de::Deserializer<'de> for RowDeserializer<'de, S> {
        type Error = Error;

        fn deserialize_any<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value> {
            visitor.visit_map(Fields {
                view: self.view,
                row: self.row,
                field: 0,
            })
        }

        serde::forward_to_deserialize_any! {
            bool i8 i16 i32 i64 i128 u8 u16 u32 u64 u128 f32 f64 char str string
            bytes byte_buf option unit unit_struct newtype_struct seq tuple
            tuple_struct map struct enum identifier ignored_any
        }
    }

    struct Fields<'de, S: FlatSchema> {
        view: TypedView<'de, S>,
        row: usize,
        field: usize,
    }

    impl<'de, S: FlatSchema> MapAccess<'de> for Fields<'de, S> {
        type Error = Error;

        fn next_key_seed<K: DeserializeSeed<'de>>(&mut self, seed: K) -> Result<Option<K::Value>> {
            let Some(name) = S::NAMES.get(self.field) else {
                return Ok(None);
            };
            // Field names are `&'static str`, so this borrows for `'de`.
            seed.deserialize(BorrowedStr(name)).map(Some)
        }

        fn next_value_seed<V: DeserializeSeed<'de>>(&mut self, seed: V) -> Result<V::Value> {
            let f = self.field;
            self.field += 1;
            seed.deserialize(SlotDeserializer {
                view: self.view,
                row: self.row,
                field: f,
            })
        }

        fn size_hint(&self) -> Option<usize> {
            Some(S::NAMES.len().saturating_sub(self.field))
        }
    }

    /// One field's value.
    #[derive(Debug)]
    struct SlotDeserializer<'de, S: FlatSchema> {
        view: TypedView<'de, S>,
        row: usize,
        field: usize,
    }

    impl<S: FlatSchema> Clone for SlotDeserializer<'_, S> {
        fn clone(&self) -> Self {
            *self
        }
    }
    impl<S: FlatSchema> Copy for SlotDeserializer<'_, S> {}

    impl<'de, S: FlatSchema> de::Deserializer<'de> for SlotDeserializer<'de, S> {
        type Error = Error;

        fn deserialize_any<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value> {
            let kind = S::FIELDS
                .get(self.field)
                .copied()
                .ok_or_else(|| Error("field index out of range".into()))?;

            match kind {
                // A 64-bit slot carries no signedness, so hand serde the
                // widest signed form and let the target type narrow. `u64`
                // targets still work because serde tries `visit_u64` on
                // overflow-free values.
                FieldKind::Scalar64 => {
                    let v = self.view.slot(self.row, self.field).unwrap_or(0);
                    visitor.visit_u64(v)
                }
                FieldKind::Str => {
                    let s = self
                        .view
                        .str_at(self.row, self.field)
                        .ok_or_else(|| Error("string field is not valid UTF-8".into()))?;
                    // Borrowed for `'de`: this is the zero-copy path.
                    visitor.visit_borrowed_str(s)
                }
                FieldKind::StrList => visitor.visit_seq(ListAccess {
                    list: self.view.list_at(self.row, self.field),
                    k: 0,
                }),
            }
        }

        /// `bool` needs the slot reinterpreted rather than widened.
        fn deserialize_bool<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value> {
            visitor.visit_bool(self.view.slot(self.row, self.field).unwrap_or(0) != 0)
        }

        /// Signed targets: reinterpret the slot rather than clamp it, so
        /// negative values survive.
        fn deserialize_i64<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value> {
            visitor.visit_i64(self.view.slot(self.row, self.field).unwrap_or(0) as i64)
        }
        fn deserialize_i32<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value> {
            visitor.visit_i32(self.view.slot(self.row, self.field).unwrap_or(0) as i32)
        }
        fn deserialize_i16<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value> {
            visitor.visit_i16(self.view.slot(self.row, self.field).unwrap_or(0) as i16)
        }
        fn deserialize_i8<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value> {
            visitor.visit_i8(self.view.slot(self.row, self.field).unwrap_or(0) as i8)
        }

        /// Floats are stored as bits.
        fn deserialize_f64<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value> {
            visitor.visit_f64(f64::from_bits(
                self.view.slot(self.row, self.field).unwrap_or(0),
            ))
        }
        fn deserialize_f32<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value> {
            visitor
                .visit_f32(f64::from_bits(self.view.slot(self.row, self.field).unwrap_or(0)) as f32)
        }

        serde::forward_to_deserialize_any! {
            u8 u16 u32 u64 u128 i128 char str string bytes byte_buf option unit
            unit_struct newtype_struct seq tuple tuple_struct map struct enum
            identifier ignored_any
        }
    }

    struct ListAccess<'de> {
        list: super::StrList<'de>,
        k: usize,
    }

    impl<'de> SeqAccess<'de> for ListAccess<'de> {
        type Error = Error;

        fn next_element_seed<T: DeserializeSeed<'de>>(
            &mut self,
            seed: T,
        ) -> Result<Option<T::Value>> {
            if self.k >= self.list.len() {
                return Ok(None);
            }
            let s = self
                .list
                .get(self.k)
                .ok_or_else(|| Error("list item is not valid UTF-8".into()))?;
            self.k += 1;
            seed.deserialize(BorrowedStr(s)).map(Some)
        }

        fn size_hint(&self) -> Option<usize> {
            Some(self.list.len().saturating_sub(self.k))
        }
    }

    /// A `&'de str` that keeps its lifetime through serde, so derived impls
    /// can borrow rather than allocate.
    struct BorrowedStr<'de>(&'de str);

    impl<'de> de::Deserializer<'de> for BorrowedStr<'de> {
        type Error = Error;
        fn deserialize_any<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value> {
            visitor.visit_borrowed_str(self.0)
        }
        serde::forward_to_deserialize_any! {
            bool i8 i16 i32 i64 i128 u8 u16 u32 u64 u128 f32 f64 char str string
            bytes byte_buf option unit unit_struct newtype_struct seq tuple
            tuple_struct map struct enum identifier ignored_any
        }
    }

    impl<'de> IntoDeserializer<'de, Error> for BorrowedStr<'de> {
        type Deserializer = Self;
        fn into_deserializer(self) -> Self {
            self
        }
    }
}
