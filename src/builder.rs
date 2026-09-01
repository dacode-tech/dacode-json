//! Stage 2 — build the flat pool DOM from the structural index.
//!
//! Port of `__json_build_from_si`
//! (`tier3/parse_indexed.vl:456-591`) plus the fused container/string
//! operations it calls in `runtime/json_pool_write.ll`:
//! `__json_open_container`, `__json_close_container`, `__json_push_string`,
//! `__json_ctx_push/pop/inc_count`.
//!
//! The walk is iterative with an explicit 256-level stack — no recursion,
//! no byte scanning except inside the gaps between structural positions.
//!
//! # Fidelity warnings
//!
//! * Depth beyond [`STACK_MAX`] is silently dropped, not reported
//!   (`__json_ctx_push` no-ops past the limit).
//! * A closing bracket does not verify that it matches the container that
//!   was opened — `__json_close_container` takes the type code from the
//!   caller's byte, so `{"a":1]` mislabels the object as an array.
//! * Nothing is validated. Malformed input yields a well-formed but wrong
//!   pool rather than an error. Use [`crate::strict`] if you need errors.

use alloc::vec::Vec;

use crate::pool::{Level, Pool, STACK_MAX};
use crate::scalar::{parse_scalar_fast, skip_ws};
use crate::scan::StructuralIndex;
use crate::tag::Type;

/// The container nesting stack.
///
/// Vela allocates `STACK_MAX * 24` bytes once and never grows it. We do the
/// same so the workspace stays allocation-free across parses.
#[derive(Debug, Clone)]
pub struct Stack {
    levels: Vec<Level>,
}

impl Default for Stack {
    fn default() -> Self {
        Self::new()
    }
}

impl Stack {
    #[must_use]
    pub fn new() -> Self {
        Stack {
            levels: Vec::with_capacity(STACK_MAX),
        }
    }

    #[inline]
    fn clear(&mut self) {
        self.levels.clear();
    }

    /// Current nesting depth.
    #[inline]
    #[must_use]
    pub fn depth(&self) -> usize {
        self.levels.len()
    }

    /// `__json_ctx_push` — a no-op at the depth limit, exactly like Vela.
    #[inline]
    fn push(&mut self, node_idx: usize, first_child: usize) {
        if self.levels.len() < STACK_MAX {
            self.levels.push(Level {
                node_idx: node_idx as u32,
                child_count: 0,
                first_child: first_child as u32,
            });
        }
    }

    /// `__json_ctx_inc_count(ctx, depth - 1)` — bump the innermost open
    /// container's value count.
    #[inline]
    fn inc_top(&mut self) {
        if let Some(top) = self.levels.last_mut() {
            top.child_count += 1;
        }
    }

    #[inline]
    fn top(&self) -> Option<Level> {
        self.levels.last().copied()
    }
}

/// `__json_open_container(pool, ctx, depth_ptr, type_tag)` —
/// `json_pool_write.ll:288-323`.
///
/// Pushes a placeholder node with `aux = 0, payload = 0`; both are patched
/// in by [`close_container`] once the child count is known.
#[inline]
fn open_container(pool: &mut Pool, stack: &mut Stack, typ: Type) {
    let idx = pool.push(typ, 0, 0);
    stack.push(idx, idx + 1);
}

/// `__json_close_container(pool, ctx, depth_ptr, type_code)` —
/// `json_pool_write.ll:329-377`.
#[inline]
fn close_container(pool: &mut Pool, stack: &mut Stack, typ: Type) {
    let Some(level) = stack.levels.pop() else {
        // Unbalanced closer at depth 0. Vela's `__json_ctx_pop` clamps the
        // depth at 0 and then reads `ctx[-1]`-adjacent garbage; we just
        // ignore it.
        return;
    };
    let idx = level.node_idx as usize;
    pool.set_tag(idx, typ, u64::from(level.child_count));
    pool.set_payload(idx, u64::from(level.first_child));

    // A closed container counts as one value in its own parent.
    stack.inc_top();
}

/// `__json_push_string(pool, ctx, depth_ptr, start, len, is_key)` —
/// `json_pool_write.ll:383-426`.
///
/// Keys deliberately do *not* bump the parent's count: the aux of an object
/// is a *pair* count, and the following value node does the increment.
#[inline]
fn push_string(pool: &mut Pool, stack: &mut Stack, start: usize, len: usize, is_key: bool) {
    let typ = if is_key { Type::Key } else { Type::String };
    pool.push(typ, len as u64, start as u64);
    if !is_key {
        stack.inc_top();
    }
}

/// Recommended pool capacity for an input of `input_len` bytes, given a
/// structural count.
///
/// `parse_indexed.vl:621-622` — `max(si_count + 1, 64)`.
#[inline]
#[must_use]
pub fn pool_capacity_for(si_count: usize) -> usize {
    (si_count + 1).max(64)
}

/// Workspace-sizing heuristic from `json_workspace_create`
/// (`parse_indexed.vl:664-667`): `max(len / 2 + 64, 64)`.
///
/// The worst case is `[0,0,0,...]` at two bytes per node.
#[inline]
#[must_use]
pub fn workspace_pool_capacity_for(max_input_len: usize) -> usize {
    (max_input_len / 2 + 64).max(64)
}

/// `__json_build_from_si` — the main state machine.
///
/// `pool` and `stack` must already be reset; `si` must already hold the
/// scan results for `input`.
pub fn build_from_index(input: &[u8], si: &StructuralIndex, pool: &mut Pool, stack: &mut Stack) {
    stack.clear();
    let input_len = input.len();
    let positions = si.positions();
    let si_count = positions.len();

    // No structural characters at all: the whole document is one scalar.
    if si_count == 0 {
        parse_scalar_fast(input, pool, 0, input_len);
        pool.set_root(0);
        return;
    }

    // Reading a position is infallible below because `si` indices are always
    // checked against `si_count` first; `pos_at` folds that into an Option.
    let pos_at = |i: usize| -> usize { positions.get(i).copied().unwrap_or(0) as usize };
    let byte_at = |i: usize| -> u8 { input.get(pos_at(i)).copied().unwrap_or(0) };

    // A top-level scalar that happens to be followed by structural noise.
    let first_ch = byte_at(0);
    if first_ch != b'{' && first_ch != b'[' && first_ch != b'"' {
        parse_scalar_fast(input, pool, 0, pos_at(0));
        pool.set_root(0);
        return;
    }

    let mut i = 0usize;
    while i < si_count {
        let pos = pos_at(i);
        let ch = byte_at(i);

        match ch {
            b'{' => {
                open_container(pool, stack, Type::Object);
                i += 1;
            }

            b'}' => {
                close_container(pool, stack, Type::Object);
                i += 1;
            }

            b'[' => {
                open_container(pool, stack, Type::Array);
                i += 1;

                // An array's first element may be a bare scalar sitting in
                // the gap before the next structural character. There is no
                // comma to trigger the usual scalar path, so it is handled
                // here.
                if i < si_count {
                    let next_pos = pos_at(i);
                    let next_ch = byte_at(i);
                    if next_ch == b']' {
                        // `[]` versus `[42]`: only parse if the gap holds
                        // something other than whitespace.
                        if skip_ws(input, pos + 1, next_pos) < next_pos {
                            parse_scalar_fast(input, pool, pos + 1, next_pos);
                            stack.inc_top();
                        }
                    } else if next_ch != b'"' && next_ch != b'{' && next_ch != b'[' {
                        parse_scalar_fast(input, pool, pos + 1, next_pos);
                        stack.inc_top();
                    }
                }
            }

            b']' => {
                close_container(pool, stack, Type::Array);
                i += 1;
            }

            b'"' => {
                let open_pos = pos;
                i += 1;
                if i >= si_count {
                    // Unterminated string: record a zero-length one and stop.
                    pool.push(Type::String, 0, (open_pos + 1) as u64);
                    break;
                }
                let close_pos = pos_at(i);
                let str_start = open_pos + 1;
                let str_len = close_pos.saturating_sub(open_pos + 1);
                i += 1;

                // A string is a key iff the next structural byte is ':'.
                let is_key = i < si_count && byte_at(i) == b':';
                push_string(pool, stack, str_start, str_len, is_key);
            }

            b':' => {
                i += 1;
                if i < si_count {
                    let next_ch = byte_at(i);
                    if next_ch != b'"' && next_ch != b'{' && next_ch != b'[' {
                        parse_scalar_fast(input, pool, pos + 1, pos_at(i));
                        stack.inc_top();
                    }
                }
            }

            b',' => {
                i += 1;
                if i < si_count {
                    let next_ch = byte_at(i);
                    if next_ch != b'"' && next_ch != b'{' && next_ch != b'[' {
                        // Only arrays get bare scalars after a comma;
                        // objects expect a key quote.
                        let parent_is_array = stack
                            .top()
                            .and_then(|l| pool.node(l.node_idx as usize))
                            .is_some_and(|n| n.typ() == Type::Array);
                        if parent_is_array {
                            parse_scalar_fast(input, pool, pos + 1, pos_at(i));
                            stack.inc_top();
                        }
                    }
                }
            }

            _ => i += 1,
        }
    }

    pool.set_root(0);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scan::{scan, Scanner};

    fn build(src: &str) -> Pool {
        let si = scan(Scanner::Scalar, src.as_bytes());
        let mut pool = Pool::with_capacity(pool_capacity_for(si.len()));
        pool.reset(src.len());
        let mut stack = Stack::new();
        build_from_index(src.as_bytes(), &si, &mut pool, &mut stack);
        pool
    }

    #[test]
    fn flat_object() {
        let p = build(r#"{"a":1,"b":true}"#);
        let root = p.root();
        assert_eq!(root.typ(), Type::Object);
        assert_eq!(root.aux(), 2);
        assert_eq!(root.payload, 1);
        assert_eq!(p.node(1).map(|n| n.typ()), Some(Type::Key));
        assert_eq!(p.node(2).map(|n| n.typ()), Some(Type::Number));
        assert_eq!(p.node(2).map(|n| n.payload), Some(1));
        assert_eq!(p.node(4).map(|n| n.typ()), Some(Type::Bool));
    }

    #[test]
    fn nested_containers_count_as_one_value() {
        let p = build(r#"{"a":{"b":1},"c":[1,2]}"#);
        assert_eq!(p.root().aux(), 2);
    }

    #[test]
    fn scalar_array_elements() {
        let p = build("[1,2,3]");
        assert_eq!(p.root().typ(), Type::Array);
        assert_eq!(p.root().aux(), 3);
    }

    #[test]
    fn empty_containers() {
        assert_eq!(build("[]").root().aux(), 0);
        assert_eq!(build("{}").root().aux(), 0);
        assert_eq!(build("[ ]").root().aux(), 0);
    }

    #[test]
    fn single_element_array() {
        let p = build("[42]");
        assert_eq!(p.root().aux(), 1);
        assert_eq!(p.node(1).map(|n| n.payload), Some(42));
    }

    #[test]
    fn top_level_scalar() {
        let p = build("42");
        assert_eq!(p.root().typ(), Type::Number);
        assert_eq!(p.root().payload, 42);
    }

    #[test]
    fn first_child_is_always_next_index() {
        // The layout invariant the query API relies on.
        let p = build(r#"{"a":[1,{"b":2}],"c":"d"}"#);
        for (i, n) in p.nodes().iter().enumerate() {
            if n.typ().is_container() && n.aux() > 0 {
                assert_eq!(n.payload as usize, i + 1, "node {i} {n:?}");
            }
        }
    }
}
