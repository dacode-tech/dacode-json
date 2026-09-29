//! `jsonflat` — a YaFF/FlatBuffers-style zero-copy wire format for JSON.
//!
//! **Reading a buffer needs no allocator.** [`View`], [`Ref`],
//! [`typed::TypedView`] and [`typed::StrList`] are arithmetic on a
//! borrowed slice — `View::new` is the entire "parse" step, and it is
//! O(1) validation of a header. So this module is available with
//! `--no-default-features`, and the buffer can come straight from
//! `mmap`, from flash, or from a `&'static [u8]` linked into the image.
//!
//! *Building* a buffer needs one, because the output length is not known
//! until the walk is done. [`Builder`], [`encode`] and
//! [`typed::TypedWriter`] are therefore gated on `alloc`. Building on a
//! host and reading on a device is the intended split, and it is what
//! makes this the best embedded read path in the crate.
//!
//! # The question this answers
//!
//! [YaFF](https://github.com/yandex/yaff) gives Protobuf a zero-copy
//! physical layout: an mmap-compatible buffer that is read through a
//! proto-like interface with no parsing step. How feasible is the same thing
//! for JSON, in Rust?
//!
//! Very. The interesting part is *why*: Vela's tier-3 pool is already 80% of
//! the way there. It is a flat array of fixed-size nodes, addressed by index
//! rather than pointer, laid out in contiguous pre-order. Three things stand
//! between it and a wire format:
//!
//! | | tier-3 pool | jsonflat |
//! |---|---|---|
//! | strings | `(offset, len)` into a **separate** input buffer | inside the buffer |
//! | escapes | decoded on every read | decoded once, at build time |
//! | validity | assumed | checked once by [`View::new`] |
//! | key lookup | linear scan | binary search |
//! | node size | 16 bytes | 8 bytes |
//!
//! The first is the only structural change: a wire format must be
//! self-contained, so the strings have to move inside. Everything else is
//! refinement.
//!
//! # What it costs and what it buys
//!
//! Zero-copy formats trade space for access time, and JSON is a bad case for
//! that trade because JSON is already compact. Expect the buffer to be
//! larger than the JSON it came from — see `docs/ZEROCOPY.md` for measured
//! sizes.
//!
//! What you get is that [`View::new`] is O(1) plus an optional O(n)
//! validation pass with no allocation, no parsing and no decoding. Reads are
//! slice arithmetic. It amortises over repeated access, which is exactly the
//! YaFF use case: write once to a cache or over a wire, read many times.
//!
//! # Layout
//!
//! Everything is little-endian and 8-byte aligned.
//!
//! ```text
//! offset  size  field
//!      0     4  magic  = "JFL1"
//!      4     2  version
//!      6     2  flags   (bit 0: object children sorted by key)
//!      8     4  node_count
//!     12     4  root index
//!     16     4  nodes offset
//!     20     4  numbers offset
//!     24     4  strings offset
//!     28     4  total length
//!     32  ...   nodes    (node_count * 8 bytes)
//!    ...   ...   numbers  (8 bytes each: i64 or f64 bits)
//!    ...   ...   strings  (raw UTF-8, already unescaped)
//! ```
//!
//! A node is eight bytes:
//!
//! ```text
//! tag:     u32   bits 0..3   kind
//!                bits 4..31  aux (child count, or string length)
//! payload: u32   kind-dependent
//! ```

pub mod typed;

use crate::tag::Type;

#[cfg(feature = "alloc")]
use alloc::borrow::{Cow, ToOwned};
#[cfg(feature = "alloc")]
use alloc::string::String;
#[cfg(feature = "alloc")]
use alloc::vec::Vec;

#[cfg(feature = "alloc")]
use crate::query::{Doc, Value};

/// The builders' string-interning table: key -> `(offset, len)` in the
/// blob.
///
/// A `HashMap` where there is a `std` to take one from, a `BTreeMap`
/// otherwise. Only the *builders* touch it, and only once per key;
/// reading a flat buffer never does. But building is measured
/// (`docs/ZEROCOPY.md`), so the faster map is used where it exists rather
/// than moving everyone to `BTreeMap` for the benefit of the target that
/// has no choice.
#[cfg(feature = "std")]
pub(crate) type Interner<K> = std::collections::HashMap<K, (u32, u32)>;
#[cfg(all(feature = "alloc", not(feature = "std")))]
pub(crate) type Interner<K> = alloc::collections::BTreeMap<K, (u32, u32)>;

/// `"JFL1"` little-endian.
pub const MAGIC: u32 = u32::from_le_bytes(*b"JFL1");
/// Format version.
pub const VERSION: u16 = 1;
/// Header size in bytes.
pub const HEADER: usize = 32;
/// Bytes per node.
pub const NODE: usize = 8;

/// Set when each object's children are sorted by key, enabling binary
/// search in [`Ref::get`].
pub const FLAG_SORTED_KEYS: u16 = 1 << 0;

/// Set when the buffer uses the schema-driven layout of [`typed`].
///
/// The layout is chosen by which type wrote the buffer, not by a build
/// flag, so the reader has to be told which one it is looking at. Without
/// this bit a dynamic buffer read as typed would silently produce garbage.
pub const FLAG_TYPED: u16 = 1 << 1;

/// Largest representable child count or string length: `aux` is 28 bits.
pub const MAX_AUX: u32 = (1 << 28) - 1;

// --- node kinds -------------------------------------------------------

const K_NULL: u32 = 0;
const K_FALSE: u32 = 1;
const K_TRUE: u32 = 2;
/// Integer small enough to live in the payload.
const K_INT_INLINE: u32 = 3;
/// Integer stored in the numbers table.
const K_INT_TABLE: u32 = 4;
/// `f64` stored in the numbers table.
const K_FLOAT: u32 = 5;
const K_STR: u32 = 6;
const K_KEY: u32 = 7;
const K_ARRAY: u32 = 8;
const K_OBJECT: u32 = 9;

/// Only the writer composes a tag; a reader only takes them apart.
#[cfg(feature = "alloc")]
#[inline]
const fn make_tag(kind: u32, aux: u32) -> u32 {
    (aux << 4) | kind
}
#[inline]
const fn tag_kind(tag: u32) -> u32 {
    tag & 0xF
}
#[inline]
const fn tag_aux(tag: u32) -> u32 {
    tag >> 4
}

/// Why a buffer could not be read as a `jsonflat` document.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Error {
    /// Fewer than [`HEADER`] bytes.
    TooShort,
    /// First four bytes are not [`MAGIC`].
    BadMagic,
    /// Version this build does not understand.
    UnsupportedVersion(u16),
    /// A section offset or length runs past the end of the buffer.
    OutOfBounds,
    /// `total length` disagrees with the buffer length.
    LengthMismatch,
    /// A node references a child, string or number that does not exist.
    DanglingReference,
    /// A string is not valid UTF-8.
    BadUtf8,
    /// Document exceeds a `u32` count or a 28-bit `aux`.
    TooLarge,
    /// A dynamic buffer was opened as typed, or the reverse.
    LayoutMismatch,
    /// The buffer was written for a different schema. See
    /// [`typed::FlatSchema::SCHEMA_ID`].
    SchemaMismatch,
}

impl core::fmt::Display for Error {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Error::TooShort => write!(f, "buffer shorter than the header"),
            Error::BadMagic => write!(f, "bad magic"),
            Error::UnsupportedVersion(v) => write!(f, "unsupported version {v}"),
            Error::OutOfBounds => write!(f, "section out of bounds"),
            Error::LengthMismatch => write!(f, "length field disagrees with the buffer"),
            Error::DanglingReference => write!(f, "dangling node reference"),
            Error::BadUtf8 => write!(f, "string is not valid UTF-8"),
            Error::TooLarge => write!(f, "document too large for the format"),
            Error::LayoutMismatch => {
                write!(
                    f,
                    "buffer layout does not match the reader (dynamic vs typed)"
                )
            }
            Error::SchemaMismatch => write!(f, "buffer was written for a different schema"),
        }
    }
}

impl core::error::Error for Error {}

// =====================================================================
// Builder
// =====================================================================

/// Builds a `jsonflat` buffer from a parsed document.
#[cfg(feature = "alloc")]
#[derive(Debug, Clone)]
pub struct Builder {
    sort_keys: bool,
    intern: Intern,
}

/// Which strings to deduplicate in the blob.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Intern {
    /// Write every occurrence. Fastest to build, largest output.
    None,
    /// Deduplicate object keys only.
    ///
    /// This is the setting that matters. An array of records repeats the
    /// same handful of key strings once per element, and a schema-driven
    /// format like `rkyv` does not store them at all — interning recovers
    /// most of that difference for a hash lookup per key.
    #[default]
    Keys,
    /// Deduplicate keys and string values. Best size, slowest build; worth
    /// it for low-cardinality data such as enum-like fields.
    All,
}

#[cfg(feature = "alloc")]
impl Default for Builder {
    fn default() -> Self {
        Builder {
            sort_keys: true,
            intern: Intern::default(),
        }
    }
}

#[cfg(feature = "alloc")]
impl Builder {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Sort each object's children by key so [`Ref::get`] can binary search.
    ///
    /// Costs a sort at build time and loses the document's original key
    /// order. On by default, because the point of this format is fast
    /// reads.
    #[must_use]
    pub fn sort_keys(mut self, yes: bool) -> Self {
        self.sort_keys = yes;
        self
    }

    /// Which strings to deduplicate. See [`Intern`].
    #[must_use]
    pub fn intern(mut self, mode: Intern) -> Self {
        self.intern = mode;
        self
    }

    /// Encode `doc` into a fresh buffer.
    pub fn build(&self, doc: Doc<'_>) -> Result<Vec<u8>, Error> {
        let mut w = Writer {
            nodes: Vec::new(),
            numbers: Vec::new(),
            strings: Vec::new(),
            sort_keys: self.sort_keys,
            intern: self.intern,
            seen: Interner::new(),
        };
        // Reserve node 0 for the root, then fill it in place.
        w.nodes.push((0, 0));
        w.write_into(0, doc.root())?;

        let node_count = u32::try_from(w.nodes.len()).map_err(|_| Error::TooLarge)?;
        let nodes_off = HEADER;
        let numbers_off = nodes_off + w.nodes.len() * NODE;
        let strings_off = numbers_off + w.numbers.len() * 8;
        let total = strings_off + w.strings.len();

        let mut out = Vec::with_capacity(total);
        out.extend_from_slice(&MAGIC.to_le_bytes());
        out.extend_from_slice(&VERSION.to_le_bytes());
        out.extend_from_slice(&(if self.sort_keys { FLAG_SORTED_KEYS } else { 0 }).to_le_bytes());
        out.extend_from_slice(&node_count.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes()); // root is always node 0
        out.extend_from_slice(
            &u32::try_from(nodes_off)
                .map_err(|_| Error::TooLarge)?
                .to_le_bytes(),
        );
        out.extend_from_slice(
            &u32::try_from(numbers_off)
                .map_err(|_| Error::TooLarge)?
                .to_le_bytes(),
        );
        out.extend_from_slice(
            &u32::try_from(strings_off)
                .map_err(|_| Error::TooLarge)?
                .to_le_bytes(),
        );
        out.extend_from_slice(
            &u32::try_from(total)
                .map_err(|_| Error::TooLarge)?
                .to_le_bytes(),
        );

        for n in &w.nodes {
            out.extend_from_slice(&n.0.to_le_bytes());
            out.extend_from_slice(&n.1.to_le_bytes());
        }
        for n in &w.numbers {
            out.extend_from_slice(&n.to_le_bytes());
        }
        out.extend_from_slice(&w.strings);

        debug_assert_eq!(out.len(), total);
        Ok(out)
    }
}

#[cfg(feature = "alloc")]
struct Writer {
    nodes: Vec<(u32, u32)>,
    numbers: Vec<u64>,
    strings: Vec<u8>,
    sort_keys: bool,
    intern: Intern,
    /// Interned string -> (offset, len) in the blob.
    seen: Interner<String>,
}

#[cfg(feature = "alloc")]
impl Writer {
    /// Append a string to the blob, deduplicating if configured.
    ///
    /// Strings are stored **already unescaped**, which is the whole point:
    /// a reader never decodes anything.
    fn put_str(&mut self, s: &str, dedup: bool) -> Result<(u32, u32), Error> {
        if dedup {
            if let Some(&hit) = self.seen.get(s) {
                return Ok(hit);
            }
        }
        let off = u32::try_from(self.strings.len()).map_err(|_| Error::TooLarge)?;
        let len = u32::try_from(s.len()).map_err(|_| Error::TooLarge)?;
        if len > MAX_AUX {
            return Err(Error::TooLarge);
        }
        self.strings.extend_from_slice(s.as_bytes());
        if dedup {
            self.seen.insert(s.to_owned(), (off, len));
        }
        Ok((off, len))
    }

    fn push_number(&mut self, bits: u64) -> Result<u32, Error> {
        let idx = u32::try_from(self.numbers.len()).map_err(|_| Error::TooLarge)?;
        self.numbers.push(bits);
        Ok(idx)
    }

    /// Write the value's header into `slot` and append its descendants.
    ///
    /// Children of a container occupy a contiguous reserved run, so an
    /// array element is reachable by `first_child + i` with no scanning.
    /// That is the one structural difference from the tier-3 pool, where
    /// elements are variable-width subtrees and indexing costs a
    /// `skip_subtree` walk per preceding element.
    fn write_into(&mut self, slot: usize, v: Value<'_>) -> Result<(), Error> {
        let node = match v.typ() {
            Type::Null => (make_tag(K_NULL, 0), 0),
            Type::Bool => {
                let k = if v.as_bool() == Some(true) {
                    K_TRUE
                } else {
                    K_FALSE
                };
                (make_tag(k, 0), 0)
            }
            Type::Number => {
                let n = v.as_i64().unwrap_or(0);
                // Most JSON integers are small; inlining them avoids an
                // 8-byte table entry and an indirection on every read.
                if let Ok(small) = i32::try_from(n) {
                    (make_tag(K_INT_INLINE, 0), small as u32)
                } else {
                    (make_tag(K_INT_TABLE, 0), self.push_number(n as u64)?)
                }
            }
            Type::Float => {
                let bits = v.as_f64().unwrap_or(0.0).to_bits();
                (make_tag(K_FLOAT, 0), self.push_number(bits)?)
            }
            Type::String | Type::Key => {
                let s = v.as_str().ok_or(Error::BadUtf8)?;
                let (off, len) = self.put_str(&s, self.intern == Intern::All)?;
                let k = if v.typ() == Type::Key { K_KEY } else { K_STR };
                (make_tag(k, len), off)
            }

            Type::Array => {
                let count = u32::try_from(v.len()).map_err(|_| Error::TooLarge)?;
                if count > MAX_AUX {
                    return Err(Error::TooLarge);
                }
                let first = self.nodes.len();
                self.nodes.resize(first + count as usize, (0, 0));
                self.set(
                    slot,
                    (
                        make_tag(K_ARRAY, count),
                        u32::try_from(first).map_err(|_| Error::TooLarge)?,
                    ),
                )?;

                for (i, child) in v.elements().enumerate() {
                    self.write_into(first + i, child)?;
                }
                return Ok(());
            }

            Type::Object => {
                let count = u32::try_from(v.len()).map_err(|_| Error::TooLarge)?;
                if count > MAX_AUX {
                    return Err(Error::TooLarge);
                }

                // Decode keys once here rather than on every read, and sort
                // so `Ref::get` can binary search.
                let mut pairs: Vec<(Cow<'_, str>, Value<'_>)> = Vec::with_capacity(count as usize);
                for (k, val) in v.entries() {
                    pairs.push((crate::unescape::unescape(k).ok_or(Error::BadUtf8)?, val));
                }
                if self.sort_keys {
                    pairs.sort_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));
                }

                let first = self.nodes.len();
                self.nodes.resize(first + count as usize * 2, (0, 0));
                self.set(
                    slot,
                    (
                        make_tag(K_OBJECT, count),
                        u32::try_from(first).map_err(|_| Error::TooLarge)?,
                    ),
                )?;

                let dedup = self.intern != Intern::None;
                for (i, (key, val)) in pairs.into_iter().enumerate() {
                    let (off, len) = self.put_str(&key, dedup)?;
                    self.set(first + i * 2, (make_tag(K_KEY, len), off))?;
                    self.write_into(first + i * 2 + 1, val)?;
                }
                return Ok(());
            }
        };

        self.set(slot, node)
    }

    #[inline]
    fn set(&mut self, slot: usize, node: (u32, u32)) -> Result<(), Error> {
        match self.nodes.get_mut(slot) {
            Some(dst) => {
                *dst = node;
                Ok(())
            }
            None => Err(Error::DanglingReference),
        }
    }
}

/// Encode a parsed document with default options.
#[cfg(feature = "alloc")]
pub fn encode(doc: Doc<'_>) -> Result<Vec<u8>, Error> {
    Builder::new().build(doc)
}

// =====================================================================
// Reader
// =====================================================================

/// A validated, borrowed view over a `jsonflat` buffer.
///
/// Constructing one is the entire "parse" step. After that, every accessor
/// is arithmetic on the borrowed slice — no allocation, no decoding, no
/// ownership transfer. The buffer can come from `mmap`.
#[derive(Debug, Clone, Copy)]
pub struct View<'a> {
    buf: &'a [u8],
    nodes: &'a [u8],
    numbers: &'a [u8],
    strings: &'a [u8],
    node_count: u32,
    root: u32,
    sorted: bool,
}

#[inline]
fn rd_u32(b: &[u8], off: usize) -> Option<u32> {
    let s = b.get(off..off + 4)?;
    Some(u32::from_le_bytes(<[u8; 4]>::try_from(s).ok()?))
}

#[inline]
fn rd_u16(b: &[u8], off: usize) -> Option<u16> {
    let s = b.get(off..off + 2)?;
    Some(u16::from_le_bytes(<[u8; 2]>::try_from(s).ok()?))
}

#[inline]
fn rd_u64(b: &[u8], off: usize) -> Option<u64> {
    let s = b.get(off..off + 8)?;
    Some(u64::from_le_bytes(<[u8; 8]>::try_from(s).ok()?))
}

impl<'a> View<'a> {
    /// Read the header and check that every section lies inside the buffer.
    ///
    /// **O(1)** — it does not walk the nodes. Node references are checked
    /// lazily on access (every accessor returns `Option`), so a corrupt
    /// buffer yields `None` rather than a panic. Use
    /// [`View::validate_deep`] when the bytes are untrusted and you would
    /// rather pay once up front.
    pub fn new(buf: &'a [u8]) -> Result<Self, Error> {
        if buf.len() < HEADER {
            return Err(Error::TooShort);
        }
        if rd_u32(buf, 0) != Some(MAGIC) {
            return Err(Error::BadMagic);
        }
        let version = rd_u16(buf, 4).ok_or(Error::TooShort)?;
        if version != VERSION {
            return Err(Error::UnsupportedVersion(version));
        }
        let flags = rd_u16(buf, 6).ok_or(Error::TooShort)?;
        if flags & FLAG_TYPED != 0 {
            // Typed buffers have a different section layout entirely.
            return Err(Error::LayoutMismatch);
        }
        let node_count = rd_u32(buf, 8).ok_or(Error::TooShort)?;
        let root = rd_u32(buf, 12).ok_or(Error::TooShort)?;
        let nodes_off = rd_u32(buf, 16).ok_or(Error::TooShort)? as usize;
        let numbers_off = rd_u32(buf, 20).ok_or(Error::TooShort)? as usize;
        let strings_off = rd_u32(buf, 24).ok_or(Error::TooShort)? as usize;
        let total = rd_u32(buf, 28).ok_or(Error::TooShort)? as usize;

        if total != buf.len() {
            return Err(Error::LengthMismatch);
        }
        if nodes_off > numbers_off || numbers_off > strings_off || strings_off > total {
            return Err(Error::OutOfBounds);
        }
        let nodes_len = (node_count as usize)
            .checked_mul(NODE)
            .ok_or(Error::OutOfBounds)?;
        if nodes_off.checked_add(nodes_len) != Some(numbers_off) {
            return Err(Error::OutOfBounds);
        }
        if node_count > 0 && root >= node_count {
            return Err(Error::DanglingReference);
        }

        let nodes = buf.get(nodes_off..numbers_off).ok_or(Error::OutOfBounds)?;
        let numbers = buf
            .get(numbers_off..strings_off)
            .ok_or(Error::OutOfBounds)?;
        let strings = buf.get(strings_off..total).ok_or(Error::OutOfBounds)?;

        Ok(View {
            buf,
            nodes,
            numbers,
            strings,
            node_count,
            root,
            sorted: flags & FLAG_SORTED_KEYS != 0,
        })
    }

    /// Walk every node and check that all references resolve and all
    /// strings are UTF-8.
    ///
    /// O(n), still allocation-free. This is the "trusted vs untrusted
    /// input" knob: skip it for data you produced, run it for data off the
    /// network.
    pub fn validate_deep(&self) -> Result<(), Error> {
        for i in 0..self.node_count {
            let Some(r) = self.node(i) else {
                return Err(Error::DanglingReference);
            };
            match r.kind {
                K_STR | K_KEY => {
                    let s = self.str_at(r.payload, r.aux).ok_or(Error::OutOfBounds)?;
                    core::str::from_utf8(s).map_err(|_| Error::BadUtf8)?;
                }
                K_INT_TABLE | K_FLOAT if rd_u64(self.numbers, r.payload as usize * 8).is_none() => {
                    return Err(Error::DanglingReference);
                }
                K_ARRAY | K_OBJECT => {
                    let per = if r.kind == K_OBJECT { 2 } else { 1 };
                    let need = (r.aux as usize)
                        .checked_mul(per)
                        .ok_or(Error::OutOfBounds)?;
                    if (r.payload as usize).saturating_add(need) > self.node_count as usize {
                        return Err(Error::DanglingReference);
                    }
                }
                _ => {}
            }
        }
        Ok(())
    }

    /// The whole buffer.
    #[inline]
    #[must_use]
    pub fn as_bytes(&self) -> &'a [u8] {
        self.buf
    }

    /// Number of nodes.
    #[inline]
    #[must_use]
    pub fn len(&self) -> usize {
        self.node_count as usize
    }

    #[inline]
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.node_count == 0
    }

    /// Whether object children are sorted by key.
    #[inline]
    #[must_use]
    pub fn keys_sorted(&self) -> bool {
        self.sorted
    }

    /// The root value.
    #[inline]
    #[must_use]
    pub fn root(&self) -> Ref<'a> {
        Ref {
            view: *self,
            idx: self.root,
        }
    }

    #[inline]
    fn node(&self, idx: u32) -> Option<Raw> {
        let off = (idx as usize).checked_mul(NODE)?;
        let tag = rd_u32(self.nodes, off)?;
        let payload = rd_u32(self.nodes, off + 4)?;
        Some(Raw {
            kind: tag_kind(tag),
            aux: tag_aux(tag),
            payload,
        })
    }

    #[inline]
    fn str_at(&self, off: u32, len: u32) -> Option<&'a [u8]> {
        self.strings
            .get(off as usize..(off as usize).checked_add(len as usize)?)
    }
}

#[derive(Debug, Clone, Copy)]
struct Raw {
    kind: u32,
    aux: u32,
    payload: u32,
}

/// A cursor onto one node in a [`View`].
#[derive(Debug, Clone, Copy)]
pub struct Ref<'a> {
    view: View<'a>,
    idx: u32,
}

impl<'a> Ref<'a> {
    #[inline]
    fn raw(&self) -> Option<Raw> {
        self.view.node(self.idx)
    }

    /// The JSON type of this value.
    #[must_use]
    pub fn typ(&self) -> Type {
        match self.raw().map(|r| r.kind) {
            Some(K_FALSE | K_TRUE) => Type::Bool,
            Some(K_INT_INLINE | K_INT_TABLE) => Type::Number,
            Some(K_FLOAT) => Type::Float,
            Some(K_STR) => Type::String,
            Some(K_KEY) => Type::Key,
            Some(K_ARRAY) => Type::Array,
            Some(K_OBJECT) => Type::Object,
            _ => Type::Null,
        }
    }

    #[must_use]
    pub fn is_null(&self) -> bool {
        self.raw().map(|r| r.kind) == Some(K_NULL)
    }

    #[must_use]
    pub fn as_bool(&self) -> Option<bool> {
        match self.raw()?.kind {
            K_TRUE => Some(true),
            K_FALSE => Some(false),
            _ => None,
        }
    }

    #[must_use]
    pub fn as_i64(&self) -> Option<i64> {
        let r = self.raw()?;
        match r.kind {
            K_INT_INLINE => Some(i64::from(r.payload as i32)),
            K_INT_TABLE => Some(rd_u64(self.view.numbers, r.payload as usize * 8)? as i64),
            _ => None,
        }
    }

    #[must_use]
    pub fn as_f64(&self) -> Option<f64> {
        let r = self.raw()?;
        match r.kind {
            K_INT_INLINE => Some(f64::from(r.payload as i32)),
            K_INT_TABLE => Some(rd_u64(self.view.numbers, r.payload as usize * 8)? as i64 as f64),
            K_FLOAT => Some(f64::from_bits(rd_u64(
                self.view.numbers,
                r.payload as usize * 8,
            )?)),
            _ => None,
        }
    }

    /// The string, **borrowed directly from the buffer**.
    ///
    /// This is the payoff versus every JSON parser: no decoding, no
    /// allocation, no `Cow`. The bytes were unescaped once at build time.
    #[must_use]
    pub fn as_str(&self) -> Option<&'a str> {
        let r = self.raw()?;
        if r.kind != K_STR && r.kind != K_KEY {
            return None;
        }
        core::str::from_utf8(self.view.str_at(r.payload, r.aux)?).ok()
    }

    /// Raw string bytes, skipping the UTF-8 check.
    #[must_use]
    pub fn as_bytes(&self) -> Option<&'a [u8]> {
        let r = self.raw()?;
        if r.kind != K_STR && r.kind != K_KEY {
            return None;
        }
        self.view.str_at(r.payload, r.aux)
    }

    /// Element or pair count.
    #[must_use]
    pub fn len(&self) -> usize {
        match self.raw() {
            Some(r) if r.kind == K_ARRAY || r.kind == K_OBJECT => r.aux as usize,
            _ => 0,
        }
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Array element by index — **O(1)**.
    ///
    /// The tier-3 pool needs `skip_subtree` per preceding element, so this
    /// is O(n) there. Here children occupy fixed-size adjacent slots, so it
    /// is one multiply-add.
    #[must_use]
    pub fn at(&self, i: usize) -> Option<Ref<'a>> {
        let r = self.raw()?;
        if r.kind != K_ARRAY || i >= r.aux as usize {
            return None;
        }
        let idx = (r.payload as usize).checked_add(i)?;
        Some(Ref {
            view: self.view,
            idx: u32::try_from(idx).ok()?,
        })
    }

    /// Object field by key — **O(log n)** when the buffer was built with
    /// sorted keys, O(n) otherwise.
    #[must_use]
    pub fn get(&self, key: &str) -> Option<Ref<'a>> {
        let r = self.raw()?;
        if r.kind != K_OBJECT {
            return None;
        }
        let n = r.aux as usize;
        let base = r.payload as usize;

        if self.view.sorted {
            let (mut lo, mut hi) = (0usize, n);
            while lo < hi {
                let mid = lo + (hi - lo) / 2;
                let k = self.key_bytes(base, mid)?;
                match k.cmp(key.as_bytes()) {
                    core::cmp::Ordering::Less => lo = mid + 1,
                    core::cmp::Ordering::Greater => hi = mid,
                    core::cmp::Ordering::Equal => {
                        return Some(Ref {
                            view: self.view,
                            idx: u32::try_from(base + mid * 2 + 1).ok()?,
                        })
                    }
                }
            }
            return None;
        }

        for i in 0..n {
            if self.key_bytes(base, i)? == key.as_bytes() {
                return Some(Ref {
                    view: self.view,
                    idx: u32::try_from(base + i * 2 + 1).ok()?,
                });
            }
        }
        None
    }

    #[inline]
    fn key_bytes(&self, base: usize, i: usize) -> Option<&'a [u8]> {
        let node = self.view.node(u32::try_from(base + i * 2).ok()?)?;
        self.view.str_at(node.payload, node.aux)
    }

    /// Iterate array elements.
    #[must_use]
    pub fn elements(&self) -> Elements<'a> {
        let (base, n) = match self.raw() {
            Some(r) if r.kind == K_ARRAY => (r.payload as usize, r.aux as usize),
            _ => (0, 0),
        };
        Elements {
            view: self.view,
            base,
            i: 0,
            n,
        }
    }

    /// Iterate object `(key, value)` pairs.
    #[must_use]
    pub fn entries(&self) -> Entries<'a> {
        let (base, n) = match self.raw() {
            Some(r) if r.kind == K_OBJECT => (r.payload as usize, r.aux as usize),
            _ => (0, 0),
        };
        Entries {
            view: self.view,
            base,
            i: 0,
            n,
        }
    }
}

/// Iterator over array elements.
#[derive(Debug, Clone)]
pub struct Elements<'a> {
    view: View<'a>,
    base: usize,
    i: usize,
    n: usize,
}

impl<'a> Iterator for Elements<'a> {
    type Item = Ref<'a>;
    fn next(&mut self) -> Option<Ref<'a>> {
        if self.i >= self.n {
            return None;
        }
        let idx = u32::try_from(self.base + self.i).ok()?;
        self.i += 1;
        Some(Ref {
            view: self.view,
            idx,
        })
    }
    fn size_hint(&self) -> (usize, Option<usize>) {
        let rem = self.n - self.i;
        (rem, Some(rem))
    }
}

/// Iterator over object pairs.
#[derive(Debug, Clone)]
pub struct Entries<'a> {
    view: View<'a>,
    base: usize,
    i: usize,
    n: usize,
}

impl<'a> Iterator for Entries<'a> {
    type Item = (&'a str, Ref<'a>);
    fn next(&mut self) -> Option<(&'a str, Ref<'a>)> {
        if self.i >= self.n {
            return None;
        }
        let kidx = u32::try_from(self.base + self.i * 2).ok()?;
        let vidx = u32::try_from(self.base + self.i * 2 + 1).ok()?;
        self.i += 1;
        let knode = self.view.node(kidx)?;
        let key = core::str::from_utf8(self.view.str_at(knode.payload, knode.aux)?).ok()?;
        Some((
            key,
            Ref {
                view: self.view,
                idx: vidx,
            },
        ))
    }
    fn size_hint(&self) -> (usize, Option<usize>) {
        let rem = self.n - self.i;
        (rem, Some(rem))
    }
}
