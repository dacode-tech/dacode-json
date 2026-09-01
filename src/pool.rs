//! The flat node pool — Vela tier-3's DOM representation.
//!
//! Vela lays this out as raw bytes obtained from `__vela_page_alloc`:
//!
//! ```text
//! offset  0  count      : i64
//! offset  8  capacity   : i64
//! offset 16  input_len  : i64
//! offset 24  root_idx   : i64
//! offset 32  node[0] .. node[i] at 32 + i*16
//!
//! node: +0 tag : i64
//!       +8 payload : i64
//! ```
//!
//! (`tier3/parse.vl:27-33`, `tier3/parse_indexed.vl:49-54`,
//! `runtime/json_pool_write.ll:4-9`.)
//!
//! In Rust we keep the same 16-byte node but let `Vec` own the storage. The
//! header fields become struct fields; `capacity` is `Vec::capacity`.
//!
//! # Layout invariant
//!
//! Children are stored contiguously in pre-order immediately after their
//! container node, so `first_child == container_idx + 1` always. Object
//! children alternate `Key, value, Key, value, ...`. `pool_skip_subtree`
//! (see [`crate::query`]) depends on this.

use alloc::vec::Vec;

use crate::tag::{make_tag, tag_aux, tag_type, Type};

/// One 16-byte pool node.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(C)]
pub struct Node {
    /// `aux << 8 | type`.
    pub tag: u64,
    /// Meaning depends on the type — see [`Type`].
    pub payload: u64,
}

impl Node {
    #[inline]
    #[must_use]
    pub const fn new(typ: Type, aux: u64, payload: u64) -> Self {
        Node {
            tag: make_tag(typ, aux),
            payload,
        }
    }

    #[inline]
    #[must_use]
    pub const fn typ(self) -> Type {
        tag_type(self.tag)
    }

    #[inline]
    #[must_use]
    pub const fn aux(self) -> u64 {
        tag_aux(self.tag)
    }
}

/// A parsed document: the node pool plus the input length it was built from.
///
/// Strings are *not* materialised — [`Type::String`] and [`Type::Key`] nodes
/// hold `(offset, len)` into the original input, with escape sequences
/// untouched. Resolve them with [`crate::query::Doc`].
#[derive(Debug, Clone, Default)]
pub struct Pool {
    pub(crate) nodes: Vec<Node>,
    pub(crate) input_len: usize,
    pub(crate) root_idx: usize,
}

impl Pool {
    /// Allocate a pool with room for `capacity` nodes.
    ///
    /// Mirrors `idx_pool_alloc` (`parse_indexed.vl:74-79`), which computes
    /// `32 + capacity * 16` bytes.
    #[inline]
    #[must_use]
    pub fn with_capacity(capacity: usize) -> Self {
        Pool {
            nodes: Vec::with_capacity(capacity),
            input_len: 0,
            root_idx: 0,
        }
    }

    /// `__json_pool_init(pool, capacity, input_len)` — reset for reuse.
    ///
    /// Keeps the existing allocation (this is what makes the workspace
    /// zero-allocation across repeated parses).
    #[inline]
    pub fn reset(&mut self, input_len: usize) {
        self.nodes.clear();
        self.input_len = input_len;
        self.root_idx = 0;
    }

    /// `__json_pool_push` — append a node, return its index.
    #[inline]
    pub fn push(&mut self, typ: Type, aux: u64, payload: u64) -> usize {
        let idx = self.nodes.len();
        self.nodes.push(Node::new(typ, aux, payload));
        idx
    }

    /// Append a pre-built node.
    #[inline]
    pub fn push_node(&mut self, node: Node) -> usize {
        let idx = self.nodes.len();
        self.nodes.push(node);
        idx
    }

    /// `__json_pool_get_count`.
    #[inline]
    #[must_use]
    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    #[inline]
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    #[inline]
    #[must_use]
    pub fn capacity(&self) -> usize {
        self.nodes.capacity()
    }

    /// Byte length of the input this pool was built from.
    #[inline]
    #[must_use]
    pub fn input_len(&self) -> usize {
        self.input_len
    }

    /// `pool[24]` — index of the root node. Always 0 in practice, since
    /// `__json_pool_set_root(pool, 0)` is the only call site.
    #[inline]
    #[must_use]
    pub fn root_idx(&self) -> usize {
        self.root_idx
    }

    #[inline]
    pub fn set_root(&mut self, idx: usize) {
        self.root_idx = idx;
    }

    /// All nodes, in pre-order.
    #[inline]
    #[must_use]
    pub fn nodes(&self) -> &[Node] {
        &self.nodes
    }

    /// Bounds-checked node read.
    ///
    /// Vela's `__json_pool_get_tag` performs no bounds check and would read
    /// out of bounds on a malformed index; we return `None` instead. Every
    /// in-tree caller has already validated the index.
    #[inline]
    #[must_use]
    pub fn node(&self, idx: usize) -> Option<Node> {
        self.nodes.get(idx).copied()
    }

    /// The root node, or a synthetic `Null` node when the pool is empty
    /// (matching `pool_root_tag`, `tier3/parse.vl:525-528`).
    #[inline]
    #[must_use]
    pub fn root(&self) -> Node {
        match self.nodes.get(self.root_idx) {
            Some(n) => *n,
            None => Node::new(Type::Null, 0, 0),
        }
    }

    /// `__json_pool_set_tag`.
    #[inline]
    pub fn set_tag(&mut self, idx: usize, typ: Type, aux: u64) {
        if let Some(n) = self.nodes.get_mut(idx) {
            n.tag = make_tag(typ, aux);
        }
    }

    /// `__json_pool_set_payload`.
    #[inline]
    pub fn set_payload(&mut self, idx: usize, payload: u64) {
        if let Some(n) = self.nodes.get_mut(idx) {
            n.payload = payload;
        }
    }
}

/// One level of the container nesting stack.
///
/// Vela stores this as 24 bytes per level in a flat buffer
/// (`parse_indexed.vl:96-99`, `runtime/json_pool_write.ll` ctx ops).
#[derive(Debug, Clone, Copy, Default)]
pub struct Level {
    /// Pool index of the container node being built.
    pub node_idx: u32,
    /// Number of *values* seen so far (object keys do not count).
    pub child_count: u32,
    /// `node_idx + 1`.
    pub first_child: u32,
}

/// `IDX_STACK_MAX()` — `parse_indexed.vl:66`.
///
/// Vela's `__json_ctx_push` silently no-ops past this depth, truncating
/// deeper documents rather than reporting an error. The faithful builder
/// reproduces that; [`crate::strict`] reports `DepthLimitExceeded`.
pub const STACK_MAX: usize = 256;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn node_size_matches_vela() {
        assert_eq!(core::mem::size_of::<Node>(), 16);
        assert_eq!(core::mem::align_of::<Node>(), 8);
    }

    #[test]
    fn empty_pool_root_is_null() {
        let p = Pool::default();
        assert_eq!(p.root().typ(), Type::Null);
    }

    #[test]
    fn reset_keeps_capacity() {
        let mut p = Pool::with_capacity(128);
        let cap = p.capacity();
        p.push(Type::Number, 0, 7);
        p.reset(0);
        assert_eq!(p.len(), 0);
        assert_eq!(p.capacity(), cap);
    }
}
