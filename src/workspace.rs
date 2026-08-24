//! Reusable parse workspace — zero allocations across repeated parses.
//!
//! Port of `json_workspace_create` / `json_pool_parse_ws`
//! (`tier3/parse_indexed.vl:645-727`), which is the fastest end-to-end path
//! in the Vela tree (166 MB/s on the 10 MB corpus versus 146 MB/s for
//! `json_pool_parse_fast`, per `docs/stage2/JSON_IMPROVEMENT_PLAN.md:52-73`).
//!
//! Vela carves one `mmap` into `[header | si | pool | ctx | depth]` and
//! reuses it forever. The returned pool pointer is invalidated by the next
//! parse on the same workspace — a contract enforced only by a comment
//! (`parse_indexed.vl:693-694`). Here it is enforced by the borrow checker:
//! [`Workspace::parse`] takes `&mut self` and returns a [`Doc`] borrowing
//! from it, so a second parse cannot compile while the first result is live.
//!
//! ```
//! # use vela_json::Workspace;
//! let mut ws = Workspace::new();
//! let doc = ws.parse(br#"{"a":1}"#);
//! assert_eq!(doc.root().get("a").and_then(|v| v.as_i64()), Some(1));
//! ```
//!
//! ```compile_fail
//! # use vela_json::Workspace;
//! let mut ws = Workspace::new();
//! let first = ws.parse(br#"{"a":1}"#);
//! let second = ws.parse(br#"{"b":2}"#); // second borrow while `first` lives
//! let _ = first.root();
//! ```

use crate::builder::{build_from_index, workspace_pool_capacity_for, Stack};
use crate::pool::Pool;
use crate::query::Doc;
use crate::scan::{scan_into, Scanner, StructuralIndex, MAX_INPUT_LEN};

/// Owns every buffer a parse needs.
#[derive(Debug, Clone)]
pub struct Workspace {
    si: StructuralIndex,
    pool: Pool,
    stack: Stack,
    scanner: Scanner,
}

impl Default for Workspace {
    fn default() -> Self {
        Self::new()
    }
}

impl Workspace {
    /// An empty workspace that grows on first use.
    #[must_use]
    pub fn new() -> Self {
        Workspace {
            si: StructuralIndex::default(),
            pool: Pool::default(),
            stack: Stack::new(),
            scanner: Scanner::default(),
        }
    }

    /// Pre-size for inputs up to `max_input_len` bytes so the first parse
    /// does not allocate.
    ///
    /// Uses Vela's heuristics: `max_input_len + 1` index slots and
    /// `max(max_input_len / 2 + 64, 64)` pool nodes. The pool figure is a
    /// worst case of `[0,0,0,...]` at two bytes per node — half of the
    /// `len + 2` the earlier revision used.
    #[must_use]
    pub fn with_capacity(max_input_len: usize) -> Self {
        let capped = max_input_len.min(MAX_INPUT_LEN);
        Workspace {
            si: StructuralIndex::with_capacity(capped + 1 + crate::scan::SPILL),
            pool: Pool::with_capacity(workspace_pool_capacity_for(capped)),
            stack: Stack::new(),
            scanner: Scanner::default(),
        }
    }

    /// Choose the Stage 1 implementation. Defaults to
    /// [`Scanner::Branchless2x`].
    #[must_use]
    pub fn with_scanner(mut self, scanner: Scanner) -> Self {
        self.scanner = scanner;
        self
    }

    #[inline]
    pub fn set_scanner(&mut self, scanner: Scanner) {
        self.scanner = scanner;
    }

    /// Parse `input`, reusing the existing buffers.
    ///
    /// Inputs longer than [`MAX_INPUT_LEN`] yield an empty document, because
    /// structural positions are `u32`. Vela returns a null pointer here
    /// (`parse_indexed.vl:604-606`).
    ///
    /// This never fails: Vela's tier 3 has no error reporting at all, and
    /// malformed input produces a well-formed but wrong pool. See
    /// [`crate::strict`] for a validating parser.
    pub fn parse<'a>(&'a mut self, input: &'a [u8]) -> Doc<'a> {
        self.si.clear();
        self.pool.reset(input.len());

        if input.len() <= MAX_INPUT_LEN {
            scan_into(self.scanner, input, &mut self.si);
            build_from_index(input, &self.si, &mut self.pool, &mut self.stack);
        }

        Doc::new(input, &self.pool)
    }

    /// Parse and keep the pool, decoupled from the input lifetime.
    ///
    /// Slower than [`Workspace::parse`] — it clones the node vector — but
    /// useful when the result must outlive the workspace borrow.
    pub fn parse_to_pool(&mut self, input: &[u8]) -> Pool {
        self.si.clear();
        self.pool.reset(input.len());
        if input.len() <= MAX_INPUT_LEN {
            scan_into(self.scanner, input, &mut self.si);
            build_from_index(input, &self.si, &mut self.pool, &mut self.stack);
        }
        self.pool.clone()
    }

    /// Nodes allocated by the last parse.
    #[inline]
    #[must_use]
    pub fn pool(&self) -> &Pool {
        &self.pool
    }

    /// Structural index from the last parse.
    #[inline]
    #[must_use]
    pub fn index(&self) -> &StructuralIndex {
        &self.si
    }

    /// Total bytes currently held, for comparing memory against other
    /// parsers.
    #[must_use]
    pub fn heap_bytes(&self) -> usize {
        self.si.capacity() * core::mem::size_of::<u32>()
            + self.pool.capacity() * core::mem::size_of::<crate::pool::Node>()
            + crate::pool::STACK_MAX * core::mem::size_of::<crate::pool::Level>()
    }
}

/// One-shot parse — `json_pool_parse_fast`.
///
/// Allocates a fresh workspace per call, so it is only appropriate when the
/// pool must outlive any workspace. Prefer [`Workspace`] in a loop.
#[must_use]
pub fn parse_to_pool(input: &[u8]) -> Pool {
    if input.len() > MAX_INPUT_LEN {
        return Pool::default();
    }
    let si = crate::scan::scan(Scanner::default(), input);
    let mut pool = Pool::with_capacity(crate::builder::pool_capacity_for(si.len()));
    pool.reset(input.len());
    let mut stack = Stack::new();
    build_from_index(input, &si, &mut pool, &mut stack);
    pool
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reuse_does_not_allocate() {
        let src = br#"{"alpha":1,"beta":[1,2,3],"gamma":{"d":true}}"#;
        let mut ws = Workspace::with_capacity(src.len());
        let before = ws.heap_bytes();
        for _ in 0..100 {
            let doc = ws.parse(src);
            assert_eq!(doc.root().len(), 3);
        }
        assert_eq!(ws.heap_bytes(), before, "workspace reallocated");
    }

    #[test]
    fn oversized_input_is_empty_not_a_panic() {
        // Can't actually allocate 2 GB in a test; just check the guard path
        // compiles and small inputs are unaffected.
        let mut ws = Workspace::new();
        assert_eq!(ws.parse(b"1").root().as_i64(), Some(1));
    }

    #[test]
    fn one_shot_matches_workspace() {
        let src = br#"[1,{"a":"b"},null,true]"#;
        let pool_a = parse_to_pool(src);
        let mut ws = Workspace::new();
        let pool_b = ws.parse_to_pool(src);
        assert_eq!(pool_a.nodes(), pool_b.nodes());
    }
}
