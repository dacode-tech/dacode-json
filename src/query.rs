//! Navigating a built pool.
//!
//! Port of the query half of `tier3/parse.vl:520-701`:
//! `json_pool_object_get`, `json_pool_object_get_int/bool`,
//! `json_pool_object_count/keys`, `json_pool_array_get/count`,
//! `pool_skip_subtree` and `pool_val_to_json`.
//!
//! Vela passes `(input, pool)` as two loose values with nothing tying them
//! together — hand it the wrong string and you get silent garbage. [`Doc`]
//! bundles them so the borrow checker enforces the pairing.

use alloc::borrow::Cow;
use alloc::string::String;

use crate::pool::{Node, Pool};
use crate::tag::Type;

/// A pool plus the input it indexes into.
#[derive(Debug, Clone, Copy)]
pub struct Doc<'a> {
    input: &'a [u8],
    pool: &'a Pool,
}

impl<'a> Doc<'a> {
    #[inline]
    #[must_use]
    pub fn new(input: &'a [u8], pool: &'a Pool) -> Self {
        Doc { input, pool }
    }

    #[inline]
    #[must_use]
    pub fn pool(&self) -> &'a Pool {
        self.pool
    }

    #[inline]
    #[must_use]
    pub fn input(&self) -> &'a [u8] {
        self.input
    }

    /// The document root.
    #[inline]
    #[must_use]
    pub fn root(&self) -> Value<'a> {
        Value {
            doc: *self,
            idx: self.pool.root_idx(),
            node: self.pool.root(),
        }
    }

    #[inline]
    #[must_use]
    fn value_at(&self, idx: usize) -> Option<Value<'a>> {
        self.pool.node(idx).map(|node| Value {
            doc: *self,
            idx,
            node,
        })
    }

    /// `pool_skip_subtree(pool, idx)` — `tier3/parse.vl:728-763`.
    ///
    /// Returns the index one past the end of the subtree rooted at `idx`.
    /// Relies on the contiguous pre-order layout invariant.
    #[must_use]
    pub fn skip_subtree(&self, idx: usize) -> usize {
        let Some(node) = self.pool.node(idx) else {
            return idx + 1;
        };
        match node.typ() {
            Type::Object => {
                let mut ci = idx + 1;
                for _ in 0..node.aux() {
                    ci += 1; // the Key node
                    ci = self.skip_subtree(ci);
                }
                ci
            }
            Type::Array => {
                let mut ci = idx + 1;
                for _ in 0..node.aux() {
                    ci = self.skip_subtree(ci);
                }
                ci
            }
            // Null, Bool, Number, String, Key are all leaves.
            _ => idx + 1,
        }
    }
}

/// A cursor onto one node.
#[derive(Debug, Clone, Copy)]
pub struct Value<'a> {
    doc: Doc<'a>,
    idx: usize,
    node: Node,
}

impl<'a> Value<'a> {
    #[inline]
    #[must_use]
    pub fn typ(&self) -> Type {
        self.node.typ()
    }

    #[inline]
    #[must_use]
    pub fn index(&self) -> usize {
        self.idx
    }

    #[inline]
    #[must_use]
    pub fn node(&self) -> Node {
        self.node
    }

    /// `json_pool_type` — `"object"`, `"array"`, `"string"`, `"number"`,
    /// `"bool"`, `"null"`, or `"unknown"` for a bare key node.
    #[inline]
    #[must_use]
    pub fn type_name(&self) -> &'static str {
        self.node.typ().name()
    }

    /// The number payload, if this is an integer [`Type::Number`].
    ///
    /// Remember that the faithful parser is integer-only — `3.14` is stored
    /// here as `314`. See [`crate::scalar`]. [`crate::strict`] stores
    /// non-integers as [`Type::Float`] instead, so this returns `None` for
    /// them.
    #[inline]
    #[must_use]
    pub fn as_i64(&self) -> Option<i64> {
        (self.node.typ() == Type::Number).then_some(self.node.payload as i64)
    }

    /// Any number as `f64`.
    ///
    /// [`Type::Float`] is only ever produced by [`crate::strict`].
    #[inline]
    #[must_use]
    pub fn as_f64(&self) -> Option<f64> {
        match self.node.typ() {
            Type::Number => Some(self.node.payload as i64 as f64),
            Type::Float => Some(f64::from_bits(self.node.payload)),
            _ => None,
        }
    }

    #[inline]
    #[must_use]
    pub fn as_bool(&self) -> Option<bool> {
        (self.node.typ() == Type::Bool).then_some(self.node.payload != 0)
    }

    #[inline]
    #[must_use]
    pub fn is_null(&self) -> bool {
        self.node.typ() == Type::Null
    }

    /// The **raw** bytes of a string or key — escape sequences are *not*
    /// decoded, matching Vela, which stores only `(offset, len)`.
    #[inline]
    #[must_use]
    pub fn as_raw_str(&self) -> Option<&'a [u8]> {
        if !matches!(self.node.typ(), Type::String | Type::Key) {
            return None;
        }
        let start = self.node.payload as usize;
        let end = start.checked_add(self.node.aux() as usize)?;
        self.doc.input.get(start..end)
    }

    /// The string with JSON escapes decoded.
    ///
    /// This has **no counterpart in Vela tier 3** — the only unescaper in the
    /// tree is the legacy `json_extract_string`
    /// (`tier3/parse.vl:331-377`), which is lossy (`\b` and `\f` both become
    /// a space, `\uXXXX` becomes `?`). This one is correct: it decodes
    /// `\u` escapes including surrogate pairs.
    ///
    /// Borrows when there is nothing to unescape.
    #[must_use]
    pub fn as_str(&self) -> Option<Cow<'a, str>> {
        let raw = self.as_raw_str()?;
        crate::unescape::unescape(raw)
    }

    /// Number of pairs in an object or elements in an array; 0 otherwise.
    #[inline]
    #[must_use]
    pub fn len(&self) -> usize {
        if self.node.typ().is_container() {
            self.node.aux() as usize
        } else {
            0
        }
    }

    #[inline]
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Iterate `(key, value)` pairs. Empty unless this is an object.
    #[inline]
    #[must_use]
    pub fn entries(&self) -> Entries<'a> {
        Entries {
            doc: self.doc,
            remaining: if self.node.typ() == Type::Object {
                self.node.aux() as usize
            } else {
                0
            },
            cursor: self.idx + 1,
        }
    }

    /// Iterate array elements. Empty unless this is an array.
    #[inline]
    #[must_use]
    pub fn elements(&self) -> Elements<'a> {
        Elements {
            doc: self.doc,
            remaining: if self.node.typ() == Type::Array {
                self.node.aux() as usize
            } else {
                0
            },
            cursor: self.idx + 1,
        }
    }

    /// `json_pool_object_get` — linear scan comparing **raw** key bytes.
    ///
    /// An escaped key in the document will not match an unescaped `key`
    /// argument; that is Vela's behaviour too.
    #[must_use]
    pub fn get(&self, key: &str) -> Option<Value<'a>> {
        self.entries()
            .find(|(k, _)| *k == key.as_bytes())
            .map(|(_, v)| v)
    }

    /// `json_pool_array_get` — O(n) because it walks subtrees.
    #[must_use]
    pub fn at(&self, index: usize) -> Option<Value<'a>> {
        self.elements().nth(index)
    }

    /// `pool_val_to_json` — re-serialise this subtree.
    ///
    /// Strings are emitted as the raw slice inside quotes, so escapes
    /// round-trip verbatim.
    #[must_use]
    pub fn to_json(&self) -> String {
        let mut out = String::new();
        self.write_json(&mut out);
        out
    }

    fn write_json(&self, out: &mut String) {
        use core::fmt::Write as _;
        match self.node.typ() {
            Type::Null => out.push_str("null"),
            Type::Bool => out.push_str(if self.node.payload != 0 {
                "true"
            } else {
                "false"
            }),
            Type::Number => {
                let _ = write!(out, "{}", self.node.payload as i64);
            }
            Type::Float => {
                let _ = write!(out, "{}", f64::from_bits(self.node.payload));
            }
            Type::String | Type::Key => {
                out.push('"');
                if let Some(raw) = self.as_raw_str() {
                    out.push_str(&String::from_utf8_lossy(raw));
                }
                out.push('"');
            }
            Type::Array => {
                out.push('[');
                for (i, v) in self.elements().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    v.write_json(out);
                }
                out.push(']');
            }
            Type::Object => {
                out.push('{');
                for (i, (k, v)) in self.entries().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    out.push('"');
                    out.push_str(&String::from_utf8_lossy(k));
                    out.push_str("\":");
                    v.write_json(out);
                }
                out.push('}');
            }
        }
    }
}

/// Iterator over object `(raw key, value)` pairs.
#[derive(Debug, Clone)]
pub struct Entries<'a> {
    doc: Doc<'a>,
    remaining: usize,
    cursor: usize,
}

impl<'a> Iterator for Entries<'a> {
    type Item = (&'a [u8], Value<'a>);

    fn next(&mut self) -> Option<Self::Item> {
        if self.remaining == 0 {
            return None;
        }
        self.remaining -= 1;

        let key_node = self.doc.value_at(self.cursor)?;
        let key = key_node.as_raw_str().unwrap_or(&[]);
        let val_idx = self.cursor + 1;
        let val = self.doc.value_at(val_idx)?;
        self.cursor = self.doc.skip_subtree(val_idx);
        Some((key, val))
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.remaining, Some(self.remaining))
    }
}

/// Iterator over array elements.
#[derive(Debug, Clone)]
pub struct Elements<'a> {
    doc: Doc<'a>,
    remaining: usize,
    cursor: usize,
}

impl<'a> Iterator for Elements<'a> {
    type Item = Value<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.remaining == 0 {
            return None;
        }
        self.remaining -= 1;

        let val = self.doc.value_at(self.cursor)?;
        self.cursor = self.doc.skip_subtree(self.cursor);
        Some(val)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.remaining, Some(self.remaining))
    }
}
