//! The tape — simdjson Stage 2 as Vela implements it.
//!
//! Port of `tier2/tape.vl` (320 lines).
//!
//! > *"Walks structural positions from the indexer to produce a flat tape of
//! > (type_tag, payload) pairs for zero-allocation DOM navigation."*
//! > — `tier2/tape.vl:1-3`
//!
//! # Layout
//!
//! Vela's tape is `[count: i64][ (tag: i64, payload: i64) ; count ]` — 16
//! bytes per entry (`tape.vl:31-37`). Here it is a `Vec<Entry>` with the
//! same two fields.
//!
//! Containers are recorded as **matched open/close pairs** with each side
//! pointing at the other, back-patched when the closer is seen
//! (`tape.vl:216`). That is the classic simdjson tape shape, and it differs
//! from tier 3's pool, which stores a child *count* on the opener and no
//! closer at all.
//!
//! ```text
//! {"a":1}
//!   0  ObjOpen  -> 4        (patched when '}' is reached)
//!   1  Key      -> offset of 'a'
//!   2  Number   -> offset of '1'
//!   3  ...
//!   4  ObjClose -> 0
//! ```
//!
//! # Consequences of that shape
//!
//! Navigation must *walk* the tape to find a sibling, because an entry does
//! not record how many children it has — you follow the open/close links.
//! [`Tape::skip_value`] does that. Tier 3's pool stores the count directly,
//! which is why its navigation is simpler.
//!
//! # Fidelity note: scalars have no structural characters
//!
//! Numbers, `true`, `false` and `null` produce no entry in the structural
//! index, so the tape builder has to sniff for them at three specific
//! places: after `[`, after `:`, and after `,` inside an array
//! (`tape.vl:235`, `:273`, `:286`). `try_emit_scalar` checks only the first
//! byte — `t` emits `True`, `f` emits `False`, `n` emits `Null`, with no
//! verification. Reproduced.

use crate::scan::{scan_into, Scanner, StructuralIndex};
use crate::tiers::common::skip_whitespace;

/// Tape entry tags — `tape.vl:19-29`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Tag {
    ObjOpen = 1,
    ObjClose = 2,
    ArrOpen = 3,
    ArrClose = 4,
    /// String value. Payload is the byte offset of the first character
    /// **after** the opening quote.
    Str = 5,
    /// Number. Payload is the byte offset of its first character.
    Number = 6,
    True = 7,
    False = 8,
    Null = 9,
    /// Object key. Payload is the offset after the opening quote.
    Key = 10,
    /// Declared at `tape.vl:29` and never emitted by the builder.
    Root = 11,
}

impl Tag {
    #[must_use]
    pub const fn from_code(c: u8) -> Option<Tag> {
        Some(match c {
            1 => Tag::ObjOpen,
            2 => Tag::ObjClose,
            3 => Tag::ArrOpen,
            4 => Tag::ArrClose,
            5 => Tag::Str,
            6 => Tag::Number,
            7 => Tag::True,
            8 => Tag::False,
            9 => Tag::Null,
            10 => Tag::Key,
            11 => Tag::Root,
            _ => return None,
        })
    }

    /// Is this a scalar (not a container delimiter or key)?
    #[must_use]
    pub const fn is_scalar(self) -> bool {
        matches!(
            self,
            Tag::Str | Tag::Number | Tag::True | Tag::False | Tag::Null
        )
    }
}

/// One `(tag, payload)` pair.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Entry {
    pub tag: Tag,
    /// Byte offset for scalars and keys; matching tape index for container
    /// delimiters.
    pub payload: usize,
}

/// Nesting context — `CTX_OBJECT` / `CTX_ARRAY`, `tape.vl:122-123`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Ctx {
    Object,
    Array,
}

/// A built tape.
#[derive(Debug, Clone, Default)]
pub struct Tape {
    entries: Vec<Entry>,
}

impl Tape {
    /// `json_tape_build` — `tape.vl:300`. Scans the structural index and
    /// builds the tape in one call.
    ///
    /// Note this allocates a fresh index every time, which is what Vela
    /// does. [`TapeBuilder`] reuses both buffers.
    #[must_use]
    pub fn build(input: &[u8]) -> Tape {
        let mut b = TapeBuilder::new();
        b.build(input);
        b.into_tape()
    }

    /// `json_tape_count` — `tape.vl:306`.
    #[inline]
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    #[inline]
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    #[inline]
    #[must_use]
    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    /// `json_tape_tag` — `tape.vl:311`.
    #[inline]
    #[must_use]
    pub fn tag(&self, idx: usize) -> Option<Tag> {
        self.entries.get(idx).map(|e| e.tag)
    }

    /// `json_tape_payload` — `tape.vl:317`.
    #[inline]
    #[must_use]
    pub fn payload(&self, idx: usize) -> Option<usize> {
        self.entries.get(idx).map(|e| e.payload)
    }

    /// Index one past the value at `idx`.
    ///
    /// For a container this follows the back-patched link to its closer;
    /// for a scalar it is `idx + 1`. This is `tape_skip_value` from
    /// `tier2/parse.vl`.
    #[must_use]
    pub fn skip_value(&self, idx: usize) -> usize {
        match self.entries.get(idx) {
            Some(e) if matches!(e.tag, Tag::ObjOpen | Tag::ArrOpen) => {
                // payload is the closer's index; step past it. Guard
                // against a malformed/unpatched link going backwards.
                if e.payload > idx {
                    e.payload + 1
                } else {
                    idx + 1
                }
            }
            Some(_) => idx + 1,
            None => idx + 1,
        }
    }
}

/// Reusable tape + structural index.
///
/// Vela has no equivalent — `json_tape_build` allocates both buffers per
/// call, and every tier-2 public function calls it. Keeping them lets the
/// benchmark separate "the tape algorithm" from "Vela's allocator".
#[derive(Debug, Clone)]
pub struct TapeBuilder {
    si: StructuralIndex,
    tape: Tape,
    scope: Vec<usize>,
    ctx: Vec<Ctx>,
    scanner: Scanner,
}

impl Default for TapeBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl TapeBuilder {
    #[must_use]
    pub fn new() -> Self {
        TapeBuilder {
            si: StructuralIndex::default(),
            tape: Tape::default(),
            scope: Vec::new(),
            ctx: Vec::new(),
            scanner: Scanner::default(),
        }
    }

    /// Pre-size for inputs up to `max_input_len` bytes.
    #[must_use]
    pub fn with_capacity(max_input_len: usize) -> Self {
        let mut b = Self::new();
        b.si = StructuralIndex::with_capacity(max_input_len + 1 + crate::scan::SPILL);
        b.tape = Tape {
            entries: Vec::with_capacity(max_input_len / 2 + 8),
        };
        b
    }

    /// Choose the Stage 1 scanner.
    ///
    /// Vela gates this at runtime through `structural_gate.vl`, which
    /// defaults SIMD **off** — so stock tier 2 runs the scalar scanner
    /// unless something calls `json_simd_accel_enable()`.
    #[must_use]
    pub fn with_scanner(mut self, s: Scanner) -> Self {
        self.scanner = s;
        self
    }

    #[inline]
    #[must_use]
    pub fn tape(&self) -> &Tape {
        &self.tape
    }

    #[must_use]
    pub fn into_tape(self) -> Tape {
        self.tape
    }

    /// `json_tape_from_structural` — `tape.vl:170`.
    pub fn build(&mut self, input: &[u8]) -> &Tape {
        self.si.clear();
        self.tape.entries.clear();
        self.scope.clear();
        self.ctx.clear();

        scan_into(self.scanner, input, &mut self.si);
        let positions = self.si.positions();

        // No structural characters: the whole document is one scalar.
        if positions.is_empty() {
            if !input.is_empty() {
                emit_scalar(&mut self.tape.entries, input, 0);
            }
            return &self.tape;
        }

        let mut expecting_key = false;
        let mut si = 0usize;

        while si < positions.len() {
            let pos = positions.get(si).copied().unwrap_or(0) as usize;
            let Some(&ch) = input.get(pos) else {
                si += 1;
                continue;
            };

            match ch {
                b'{' => {
                    let idx = self.tape.entries.len();
                    self.tape.entries.push(Entry {
                        tag: Tag::ObjOpen,
                        payload: 0,
                    });
                    self.scope.push(idx);
                    self.ctx.push(Ctx::Object);
                    expecting_key = true;
                    si += 1;
                }

                b'}' => {
                    let open_idx = self.scope.pop().unwrap_or(0);
                    self.ctx.pop();
                    let close_idx = self.tape.entries.len();
                    self.tape.entries.push(Entry {
                        tag: Tag::ObjClose,
                        payload: open_idx,
                    });
                    if let Some(e) = self.tape.entries.get_mut(open_idx) {
                        e.payload = close_idx;
                    }
                    expecting_key = self.ctx.last() == Some(&Ctx::Object);
                    si += 1;
                }

                b'[' => {
                    let idx = self.tape.entries.len();
                    self.tape.entries.push(Entry {
                        tag: Tag::ArrOpen,
                        payload: 0,
                    });
                    self.scope.push(idx);
                    self.ctx.push(Ctx::Array);
                    expecting_key = false;
                    // A first element that is a bare scalar has no
                    // structural character of its own.
                    emit_scalar(&mut self.tape.entries, input, pos + 1);
                    si += 1;
                }

                b']' => {
                    let open_idx = self.scope.pop().unwrap_or(0);
                    self.ctx.pop();
                    let close_idx = self.tape.entries.len();
                    self.tape.entries.push(Entry {
                        tag: Tag::ArrClose,
                        payload: open_idx,
                    });
                    if let Some(e) = self.tape.entries.get_mut(open_idx) {
                        e.payload = close_idx;
                    }
                    expecting_key = self.ctx.last() == Some(&Ctx::Object);
                    si += 1;
                }

                b'"' => {
                    self.tape.entries.push(Entry {
                        tag: if expecting_key { Tag::Key } else { Tag::Str },
                        payload: pos + 1,
                    });
                    expecting_key = false;
                    // Step over the closing quote's structural entry too.
                    si += 2;
                }

                b':' => {
                    emit_scalar(&mut self.tape.entries, input, pos + 1);
                    si += 1;
                }

                b',' => {
                    if self.ctx.last() == Some(&Ctx::Object) {
                        expecting_key = true;
                    } else {
                        expecting_key = false;
                        emit_scalar(&mut self.tape.entries, input, pos + 1);
                    }
                    si += 1;
                }

                _ => si += 1,
            }
        }

        &self.tape
    }
}

/// `try_emit_scalar` — `tape.vl:129`.
///
/// Sniffs the first non-whitespace byte at or after `from`. Only the first
/// byte is checked, so `txyz` emits `True`.
#[inline]
fn emit_scalar(out: &mut Vec<Entry>, input: &[u8], from: usize) {
    if from >= input.len() {
        return;
    }
    let pos = skip_whitespace(input, from);
    let Some(&ch) = input.get(pos) else { return };

    let tag = match ch {
        // Handled by their own structural characters.
        b'"' | b'{' | b'[' => return,
        b't' => Tag::True,
        b'f' => Tag::False,
        b'n' => Tag::Null,
        b'-' | b'0'..=b'9' => Tag::Number,
        _ => return,
    };

    out.push(Entry {
        tag,
        payload: if tag == Tag::Number { pos } else { 0 },
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tags(src: &str) -> Vec<Tag> {
        Tape::build(src.as_bytes())
            .entries()
            .iter()
            .map(|e| e.tag)
            .collect()
    }

    #[test]
    fn flat_object() {
        assert_eq!(
            tags(r#"{"a":1}"#),
            vec![Tag::ObjOpen, Tag::Key, Tag::Number, Tag::ObjClose]
        );
    }

    #[test]
    fn open_and_close_point_at_each_other() {
        let t = Tape::build(br#"{"a":1}"#);
        assert_eq!(t.tag(0), Some(Tag::ObjOpen));
        let close = t.payload(0).expect("payload");
        assert_eq!(t.tag(close), Some(Tag::ObjClose));
        assert_eq!(t.payload(close), Some(0));
    }

    #[test]
    fn arrays_and_nesting() {
        assert_eq!(
            tags("[1,2]"),
            vec![Tag::ArrOpen, Tag::Number, Tag::Number, Tag::ArrClose]
        );
        assert_eq!(tags("[]"), vec![Tag::ArrOpen, Tag::ArrClose]);
        assert_eq!(tags("{}"), vec![Tag::ObjOpen, Tag::ObjClose]);
        assert_eq!(
            tags(r#"{"a":{"b":[true,null]}}"#),
            vec![
                Tag::ObjOpen,
                Tag::Key,
                Tag::ObjOpen,
                Tag::Key,
                Tag::ArrOpen,
                Tag::True,
                Tag::Null,
                Tag::ArrClose,
                Tag::ObjClose,
                Tag::ObjClose,
            ]
        );
    }

    #[test]
    fn keys_versus_string_values() {
        assert_eq!(
            tags(r#"{"k":"v"}"#),
            vec![Tag::ObjOpen, Tag::Key, Tag::Str, Tag::ObjClose]
        );
        assert_eq!(
            tags(r#"["a","b"]"#),
            vec![Tag::ArrOpen, Tag::Str, Tag::Str, Tag::ArrClose]
        );
    }

    #[test]
    fn top_level_scalar_has_no_structurals() {
        assert_eq!(tags("42"), vec![Tag::Number]);
        assert_eq!(tags("true"), vec![Tag::True]);
        assert_eq!(tags("null"), vec![Tag::Null]);
        assert!(tags("").is_empty());
    }

    #[test]
    fn skip_value_follows_container_links() {
        let t = Tape::build(br#"[[1,2],3]"#);
        // 0 ArrOpen(outer) 1 ArrOpen(inner) 2 Num 3 Num 4 ArrClose 5 Num 6 ArrClose
        assert_eq!(t.tag(1), Some(Tag::ArrOpen));
        assert_eq!(t.skip_value(1), 5, "inner array should skip to the 3");
        assert_eq!(t.tag(5), Some(Tag::Number));
        assert_eq!(t.skip_value(5), 6);
    }

    #[test]
    fn builder_reuse_matches_one_shot() {
        let mut b = TapeBuilder::with_capacity(64);
        for src in [&br#"{"a":1}"#[..], b"[1,2,3]", br#"{"x":{"y":[1]}}"#, b"[]"] {
            let reused: Vec<Entry> = b.build(src).entries().to_vec();
            let fresh: Vec<Entry> = Tape::build(src).entries().to_vec();
            assert_eq!(reused, fresh, "{:?}", String::from_utf8_lossy(src));
        }
    }
}
