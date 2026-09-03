//! Single-pass DOM builder — no structural index. Requires the
//! `vela-compat` feature.
//!
//! A reference implementation, not the recommended parser: it does not
//! validate and it truncates floats. Use [`crate::strict`] or the crate
//! root for anything real.
//!
//! Port of `tier3/parse_onepass.vl` (404 lines), Vela's task D1. It
//! produces a byte-identical [`Pool`] to [`crate::builder`] but reads the
//! document **once**, walking bytes directly instead of building a
//! structural index first and then walking that.
//!
//! # Why this exists
//!
//! `docs/PROFILING.md` §3b measured Stage 2 at 63–82% of parse time and
//! then failed twice to speed it up. The remaining hypothesis is that the
//! two-stage design is itself the disadvantage against yyjson, which is
//! single-pass:
//!
//! * two-stage reads the input once sequentially (Stage 1) and then again
//!   at scattered structural offsets (Stage 2);
//! * yyjson reads it once.
//!
//! For the indexed path to match a single-pass parser, *each* of its two
//! stages must run at roughly twice the single-pass throughput. Stage 1
//! manages 2.0–3.2 GiB/s; Stage 2 manages 0.57–1.2 GiB/s. So the arithmetic
//! says it cannot — unless the single pass is itself much slower per byte,
//! which is exactly what this module measures.
//!
//! # Differences from the indexed builder
//!
//! Same output, different failure modes on malformed input, because there
//! is no index to disagree with. Notably:
//!
//! * no SIMD anywhere — whitespace and string skipping are byte loops
//!   (Vela calls `__json_skip_ws` / `__json_skip_string`, which *are* SIMD
//!   in its runtime; equivalents are provided here);
//! * `}` and `]` close whatever container is open, without checking type —
//!   the same quirk the indexed builder has;
//! * an unexpected byte where an object key was expected is skipped one
//!   byte at a time (`parse_onepass.vl:216`).
//!
//! `tests/onepass.rs` asserts the two builders agree on every valid
//! document, which is what Vela's `t848_json_onepass.vl` checks.

use crate::pool::{Level, Pool, STACK_MAX};
use crate::scalar::parse_scalar_fast;
use crate::tag::Type;

/// Skip whitespace from `pos`. `__json_skip_ws`.
///
/// Vela's runtime version processes 16 bytes at a time with NEON. Most JSON
/// has no whitespace between tokens at all, so the loop almost always exits
/// on the first byte; a vector version costs more than it saves here and
/// measured slower.
#[inline]
fn skip_ws(input: &[u8], mut pos: usize) -> usize {
    while let Some(&b) = input.get(pos) {
        if b != b' ' && b != b'\t' && b != b'\n' && b != b'\r' {
            break;
        }
        pos += 1;
    }
    pos
}

/// Skip a string literal. `pos` points at the opening quote; returns the
/// index one past the closing quote. `__json_skip_string`.
///
/// Escape handling matches Vela's runtime (`json_pool_write.ll:526-613`): a
/// backslash advances two bytes. `\uXXXX` needs no special case because `u`
/// is not a quote.
#[inline]
fn skip_string(input: &[u8], pos: usize) -> usize {
    let len = input.len();
    let mut p = pos + 1;

    // 16 bytes at a time looking for a quote or backslash. Strings are the
    // one place in this parser where a vector scan pays, because they are
    // long enough to amortise it.
    while p + 16 <= len {
        let Some(chunk) = input.get(p..p + 16) else { break };
        let mut hit = 16usize;
        for (k, &b) in chunk.iter().enumerate() {
            if b == b'"' || b == b'\\' {
                hit = k;
                break;
            }
        }
        if hit == 16 {
            p += 16;
            continue;
        }
        p += hit;
        match input.get(p) {
            Some(b'"') => return p + 1,
            Some(b'\\') => p += 2,
            _ => return p,
        }
    }

    while p < len {
        match input.get(p) {
            Some(b'"') => return p + 1,
            Some(b'\\') => p += 2,
            Some(_) => p += 1,
            None => break,
        }
    }
    len
}

/// End of a bare scalar token. `op_find_scalar_end`
/// (`parse_onepass.vl:62`) — stops at `,`, `}`, `]` or whitespace.
#[inline]
fn find_scalar_end(input: &[u8], mut p: usize) -> usize {
    while let Some(&b) = input.get(p) {
        if matches!(b, b',' | b'}' | b']' | b' ' | b'\t' | b'\n' | b'\r') {
            return p;
        }
        p += 1;
    }
    p
}

/// Container nesting stack. Same shape as [`crate::builder::Stack`].
#[derive(Debug, Clone)]
struct Ctx {
    levels: Vec<Level>,
}

impl Ctx {
    fn new() -> Self {
        Ctx {
            levels: Vec::with_capacity(STACK_MAX),
        }
    }
    #[inline]
    fn clear(&mut self) {
        self.levels.clear();
    }
    #[inline]
    fn depth(&self) -> usize {
        self.levels.len()
    }
    /// `__json_open_container`. No-ops past the depth cap, like Vela.
    #[inline]
    fn open(&mut self, pool: &mut Pool, typ: Type) {
        let idx = pool.push(typ, 0, 0);
        if self.levels.len() < STACK_MAX {
            self.levels.push(Level {
                node_idx: idx as u32,
                child_count: 0,
                first_child: (idx + 1) as u32,
            });
        }
    }
    /// `__json_close_container`. Takes the type from the caller's byte and
    /// does not verify it matches — reproducing the quirk.
    #[inline]
    fn close(&mut self, pool: &mut Pool, typ: Type) {
        let Some(l) = self.levels.pop() else { return };
        let idx = l.node_idx as usize;
        pool.set_tag(idx, typ, u64::from(l.child_count));
        pool.set_payload(idx, u64::from(l.first_child));
        if let Some(top) = self.levels.last_mut() {
            top.child_count += 1;
        }
    }
    #[inline]
    fn inc_top(&mut self) {
        if let Some(top) = self.levels.last_mut() {
            top.child_count += 1;
        }
    }
    /// Is the innermost open container an object?
    #[inline]
    fn in_object(&self, pool: &Pool) -> bool {
        match self.levels.last() {
            Some(l) => {
                pool.node(l.node_idx as usize).map(|n| n.typ()) == Some(Type::Object)
            }
            None => false,
        }
    }
}

/// `__json_push_string`. Keys do not bump the parent count; the following
/// value does.
#[inline]
fn push_string(pool: &mut Pool, ctx: &mut Ctx, start: usize, len: usize, is_key: bool) {
    let typ = if is_key { Type::Key } else { Type::String };
    pool.push(typ, len as u64, start as u64);
    if !is_key {
        ctx.inc_top();
    }
}

/// `op_parse_value` (`parse_onepass.vl:244`) — one value at `pos`, returns
/// the index just past it. Containers only open; the main loop closes them.
#[inline]
fn parse_value(input: &[u8], pool: &mut Pool, ctx: &mut Ctx, pos: usize, ch: u8) -> usize {
    match ch {
        b'{' => {
            ctx.open(pool, Type::Object);
            pos + 1
        }
        b'[' => {
            ctx.open(pool, Type::Array);
            pos + 1
        }
        b'"' => {
            let end = skip_string(input, pos);
            push_string(pool, ctx, pos + 1, end.saturating_sub(pos + 2), false);
            end
        }
        _ => {
            let end = find_scalar_end(input, pos);
            parse_scalar_fast(input, pool, pos, end);
            ctx.inc_top();
            end
        }
    }
}

/// Reusable single-pass parser.
///
/// `json_pool_parse_onepass_ws` (`parse_onepass.vl:279`), which reuses only
/// the pool and stack — there is no index to reuse, which is the point.
#[derive(Debug, Clone)]
pub struct OnePass {
    pool: Pool,
    ctx: Ctx,
}

impl Default for OnePass {
    fn default() -> Self {
        Self::new()
    }
}

impl OnePass {
    #[must_use]
    pub fn new() -> Self {
        OnePass {
            pool: Pool::default(),
            ctx: Ctx::new(),
        }
    }

    /// Pre-size for inputs up to `max_input_len`.
    #[must_use]
    pub fn with_capacity(max_input_len: usize) -> Self {
        OnePass {
            pool: Pool::with_capacity(crate::builder::workspace_pool_capacity_for(max_input_len)),
            ctx: Ctx::new(),
        }
    }

    /// The pool from the last parse.
    #[inline]
    #[must_use]
    pub fn pool(&self) -> &Pool {
        &self.pool
    }

    /// Parse, reusing the buffers.
    pub fn parse<'a>(&'a mut self, input: &'a [u8]) -> crate::query::Doc<'a> {
        self.run(input);
        crate::query::Doc::new(input, &self.pool)
    }

    /// `json_pool_parse_onepass` — the state machine.
    fn run(&mut self, input: &[u8]) {
        let pool = &mut self.pool;
        let ctx = &mut self.ctx;
        pool.reset(input.len());
        ctx.clear();

        let len = input.len();
        let mut pos = skip_ws(input, 0);

        let Some(&first) = input.get(pos) else {
            // Empty or all whitespace.
            pool.push(Type::Null, 0, 0);
            pool.set_root(0);
            return;
        };

        // Top-level scalar or string.
        if first != b'{' && first != b'[' {
            if first == b'"' {
                let end = skip_string(input, pos);
                push_string(pool, ctx, pos + 1, end.saturating_sub(pos + 2), false);
            } else {
                let end = find_scalar_end(input, pos);
                parse_scalar_fast(input, pool, pos, end);
            }
            pool.set_root(0);
            return;
        }

        ctx.open(
            pool,
            if first == b'{' {
                Type::Object
            } else {
                Type::Array
            },
        );
        pool.set_root(0);
        pos += 1;

        while pos < len {
            pos = skip_ws(input, pos);
            let Some(&ch) = input.get(pos) else { break };

            // Closers. Note neither checks that the type matches what was
            // opened — same quirk as the indexed builder.
            if ch == b'}' || ch == b']' {
                ctx.close(
                    pool,
                    if ch == b'}' {
                        Type::Object
                    } else {
                        Type::Array
                    },
                );
                pos = skip_ws(input, pos + 1);
                if input.get(pos) == Some(&b',') {
                    pos += 1;
                }
                continue;
            }

            if ctx.depth() == 0 {
                break;
            }

            if ctx.in_object(pool) {
                // Object body: expect `"key" : value`.
                if ch != b'"' {
                    // Unexpected byte where a key was expected.
                    pos += 1;
                    continue;
                }
                let key_end = skip_string(input, pos);
                push_string(pool, ctx, pos + 1, key_end.saturating_sub(pos + 2), true);
                pos = skip_ws(input, key_end);
                if input.get(pos) == Some(&b':') {
                    pos += 1;
                }
                pos = skip_ws(input, pos);
                let Some(&val_ch) = input.get(pos) else { break };
                pos = parse_value(input, pool, ctx, pos, val_ch);
            } else {
                // Array body: expect a value.
                pos = parse_value(input, pool, ctx, pos, ch);
            }

            pos = skip_ws(input, pos);
            if input.get(pos) == Some(&b',') {
                pos += 1;
            }
        }
    }
}

/// One-shot parse into an owned pool.
#[must_use]
pub fn parse_to_pool(input: &[u8]) -> Pool {
    let mut p = OnePass::with_capacity(input.len());
    p.run(input);
    p.pool.clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nodes(src: &str) -> Vec<(Type, u64, u64)> {
        let p = parse_to_pool(src.as_bytes());
        p.nodes()
            .iter()
            .map(|n| (n.typ(), n.aux(), n.payload))
            .collect()
    }

    #[test]
    fn flat_object() {
        let p = parse_to_pool(br#"{"a":1,"b":true}"#);
        let root = p.root();
        assert_eq!(root.typ(), Type::Object);
        assert_eq!(root.aux(), 2);
        assert_eq!(p.node(1).map(|n| n.typ()), Some(Type::Key));
        assert_eq!(p.node(2).map(|n| n.payload), Some(1));
    }

    #[test]
    fn arrays_and_nesting() {
        assert_eq!(parse_to_pool(b"[1,2,3]").root().aux(), 3);
        assert_eq!(parse_to_pool(b"[]").root().aux(), 0);
        assert_eq!(parse_to_pool(b"{}").root().aux(), 0);
        assert_eq!(parse_to_pool(br#"{"a":{"b":1},"c":[1,2]}"#).root().aux(), 2);
    }

    #[test]
    fn top_level_scalars() {
        assert_eq!(parse_to_pool(b"42").root().payload, 42);
        assert_eq!(parse_to_pool(b"true").root().typ(), Type::Bool);
        assert_eq!(parse_to_pool(b"null").root().typ(), Type::Null);
        assert_eq!(parse_to_pool(b"").root().typ(), Type::Null);
        assert_eq!(parse_to_pool(br#""hi""#).root().typ(), Type::String);
    }

    #[test]
    fn skip_string_handles_escapes() {
        assert_eq!(skip_string(br#""ab""#, 0), 4);
        assert_eq!(skip_string(br#""a\"b""#, 0), 6);
        assert_eq!(skip_string(br#""a\\""#, 0), 5);
        // Long enough to take the 16-byte path, with an escape past it.
        let s = br#""aaaaaaaaaaaaaaaaaaaa\"b""#;
        assert_eq!(skip_string(s, 0), s.len());
    }

    #[test]
    fn whitespace_everywhere() {
        let p = parse_to_pool(b"  {\n \"a\" : [ 1 , 2 ] ,\t\"b\" : 3 }  ");
        assert_eq!(p.root().aux(), 2);
    }

    #[test]
    fn no_panic_on_garbage() {
        for s in [
            &b"{"[..], b"}", b"[", b"]", b"[,]", b"{,}", br#"{"a""#, br#"{"a":"#,
            b"\"unterminated", b"\\\\\\", b"[[[[[[", b"}}}}",
        ] {
            let _ = nodes(&String::from_utf8_lossy(s));
        }
    }
}
