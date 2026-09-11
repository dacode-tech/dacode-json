//! RFC 8259-conformant parser over the same pool layout.
//!
//! This is the control arm of the experiment. It keeps Vela tier 3's data
//! structures and Stage 1 verbatim — same structural index, same 16-byte
//! flat pool, same contiguous pre-order layout, same query API — and
//! replaces only Stage 2 with a validating state machine. Benchmarking the
//! two against each other isolates the cost of correctness from the cost of
//! the representation.
//!
//! What it adds over [`crate::builder`]:
//!
//! * grammar validation with a byte offset on failure ([`Error`])
//! * bracket matching (`{"a":1]` is an error, not a mislabelled array)
//! * RFC 8259 numbers — `i64` when exact, [`Type::Float`] otherwise
//! * `true`/`false`/`null` are actually checked
//! * a real depth limit instead of silent truncation
//! * optional string validation (escape syntax, no raw control bytes, UTF-8)
//!
//! What it keeps: strings are still stored as raw `(offset, len)` slices and
//! decoded lazily by [`crate::query::Value::as_str`]. That is the good part
//! of the design and there is no reason to give it up.

use alloc::vec::Vec;

use crate::builder::workspace_pool_capacity_for;
use crate::pool::{Level, Pool, STACK_MAX};
use crate::query::Doc;
use crate::scalar::skip_ws;
use crate::scan::{scan_into, Scanner, StructuralIndex, MAX_INPUT_LEN};
use crate::tag::Type;

/// Why a parse failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Error {
    /// Byte offset in the input where the problem was detected.
    pub offset: usize,
    pub kind: ErrorKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ErrorKind {
    /// Input is empty or contains only whitespace.
    Empty,
    /// A value was required here.
    ExpectedValue,
    /// An object key (a quoted string) was required here.
    ExpectedKey,
    /// A `:` was required here.
    ExpectedColon,
    /// A `,` or a closing bracket was required here.
    ExpectedCommaOrClose,
    /// A `]` closed an object, or a `}` closed an array.
    MismatchedBracket,
    /// A closing bracket with no matching opener.
    UnexpectedClose,
    /// End of input inside a container.
    UnexpectedEof,
    /// A string literal was never closed.
    UnterminatedString,
    /// Content after the top-level value.
    TrailingContent,
    /// Number did not match the RFC 8259 grammar.
    InvalidNumber,
    /// Not `true`, `false` or `null`.
    InvalidLiteral,
    /// Bad escape sequence, raw control byte, or invalid UTF-8 in a string.
    InvalidString,
    /// Nesting deeper than [`STACK_MAX`].
    DepthLimitExceeded,
    /// Input longer than [`MAX_INPUT_LEN`].
    InputTooLarge,
}

impl ErrorKind {
    /// The message this kind prints as.
    ///
    /// Separate from [`Display`](core::fmt::Display) so a caller that
    /// wraps a parse error can keep the text without formatting it into
    /// an allocation — see [`crate::de::Error`].
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            ErrorKind::Empty => "empty input",
            ErrorKind::ExpectedValue => "expected a value",
            ErrorKind::ExpectedKey => "expected an object key",
            ErrorKind::ExpectedColon => "expected ':'",
            ErrorKind::ExpectedCommaOrClose => "expected ',' or a closing bracket",
            ErrorKind::MismatchedBracket => "mismatched bracket",
            ErrorKind::UnexpectedClose => "unmatched closing bracket",
            ErrorKind::UnexpectedEof => "unexpected end of input",
            ErrorKind::UnterminatedString => "unterminated string",
            ErrorKind::TrailingContent => "trailing content after the top-level value",
            ErrorKind::InvalidNumber => "invalid number",
            ErrorKind::InvalidLiteral => "invalid literal",
            ErrorKind::InvalidString => "invalid string",
            ErrorKind::DepthLimitExceeded => "nesting too deep",
            ErrorKind::InputTooLarge => "input exceeds i32::MAX bytes",
        }
    }
}

impl core::fmt::Display for Error {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{} at byte {}", self.kind.as_str(), self.offset)
    }
}

impl core::error::Error for Error {}

/// What the grammar allows at the current position.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Expect {
    /// A value is required.
    Value,
    /// A value or `]` (immediately after `[`).
    ValueOrClose,
    /// A key or `}` (immediately after `{`).
    KeyOrClose,
    /// A key (immediately after `,` inside an object).
    Key,
    /// `:` after a key.
    Colon,
    /// `,` or a closing bracket after a completed value.
    CommaOrClose,
    /// The root value is complete; only whitespace may follow.
    Done,
}

/// Tuning knobs.
#[derive(Debug, Clone, Copy)]
pub struct Options {
    /// Validate escape syntax, reject raw control bytes, and check UTF-8
    /// inside string literals. Costs a pass over string bytes.
    pub validate_strings: bool,
    /// Maximum container nesting.
    pub max_depth: usize,
    /// Stage 1 implementation.
    pub scanner: Scanner,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            validate_strings: true,
            max_depth: STACK_MAX,
            scanner: Scanner::default(),
        }
    }
}

/// A validating parser with reusable buffers.
#[derive(Debug, Clone)]
pub struct StrictParser {
    si: StructuralIndex,
    pool: Pool,
    stack: Vec<Level>,
    kinds: Vec<Type>,
    opts: Options,
}

impl Default for StrictParser {
    fn default() -> Self {
        Self::new()
    }
}

impl StrictParser {
    #[must_use]
    pub fn new() -> Self {
        Self::with_options(Options::default())
    }

    #[must_use]
    pub fn with_options(opts: Options) -> Self {
        StrictParser {
            si: StructuralIndex::default(),
            pool: Pool::default(),
            stack: Vec::with_capacity(opts.max_depth.min(STACK_MAX)),
            kinds: Vec::with_capacity(opts.max_depth.min(STACK_MAX)),
            opts,
        }
    }

    /// Pre-size for inputs up to `max_input_len` bytes.
    #[must_use]
    pub fn with_capacity(max_input_len: usize) -> Self {
        let capped = max_input_len.min(MAX_INPUT_LEN);
        let mut p = Self::new();
        p.si = StructuralIndex::with_capacity(capped + 1 + crate::scan::SPILL);
        p.pool = Pool::with_capacity(workspace_pool_capacity_for(capped));
        p
    }

    #[must_use]
    pub fn options(&self) -> Options {
        self.opts
    }

    pub fn set_options(&mut self, opts: Options) {
        self.opts = opts;
    }

    #[inline]
    #[must_use]
    pub fn pool(&self) -> &Pool {
        &self.pool
    }

    /// Parse and validate `input`.
    ///
    /// On success the returned [`Doc`] borrows both `input` and `self`, so —
    /// exactly as with [`crate::Workspace`] — the borrow checker prevents a
    /// second parse while the result is alive.
    pub fn parse<'a>(&'a mut self, input: &'a [u8]) -> Result<Doc<'a>, Error> {
        self.run(input)?;
        Ok(Doc::new(input, &self.pool))
    }

    /// Validate without keeping the result.
    pub fn validate(&mut self, input: &[u8]) -> Result<(), Error> {
        self.run(input)
    }

    fn run(&mut self, input: &[u8]) -> Result<(), Error> {
        if input.len() > MAX_INPUT_LEN {
            return Err(Error {
                offset: MAX_INPUT_LEN,
                kind: ErrorKind::InputTooLarge,
            });
        }

        self.si.clear();
        self.pool.reset(input.len());
        self.stack.clear();
        self.kinds.clear();

        scan_into(self.opts.scanner, input, &mut self.si);

        let mut st = State {
            input,
            pool: &mut self.pool,
            stack: &mut self.stack,
            kinds: &mut self.kinds,
            opts: &self.opts,
            expect: Expect::Value,
            prev_end: 0,
        };
        st.run(self.si.positions())
    }
}

struct State<'a> {
    input: &'a [u8],
    pool: &'a mut Pool,
    stack: &'a mut Vec<Level>,
    kinds: &'a mut Vec<Type>,
    opts: &'a Options,
    expect: Expect,
    prev_end: usize,
}

impl State<'_> {
    fn err<T>(&self, offset: usize, kind: ErrorKind) -> Result<T, Error> {
        Err(Error { offset, kind })
    }

    #[inline]
    fn byte(&self, i: usize) -> u8 {
        self.input.get(i).copied().unwrap_or(0)
    }

    /// A value node has just been emitted; account for it and move on.
    #[inline]
    fn value_complete(&mut self) {
        if let Some(top) = self.stack.last_mut() {
            top.child_count += 1;
            self.expect = Expect::CommaOrClose;
        } else {
            self.expect = Expect::Done;
        }
    }

    fn open(&mut self, typ: Type, offset: usize) -> Result<(), Error> {
        if self.stack.len() >= self.opts.max_depth {
            return self.err(offset, ErrorKind::DepthLimitExceeded);
        }
        let idx = self.pool.push(typ, 0, 0);
        self.stack.push(Level {
            node_idx: idx as u32,
            child_count: 0,
            first_child: (idx + 1) as u32,
        });
        self.kinds.push(typ);
        self.expect = if typ == Type::Object {
            Expect::KeyOrClose
        } else {
            Expect::ValueOrClose
        };
        Ok(())
    }

    fn close(&mut self, typ: Type, offset: usize) -> Result<(), Error> {
        let (Some(level), Some(kind)) = (self.stack.pop(), self.kinds.pop()) else {
            return self.err(offset, ErrorKind::UnexpectedClose);
        };
        if kind != typ {
            return self.err(offset, ErrorKind::MismatchedBracket);
        }
        let idx = level.node_idx as usize;
        self.pool
            .set_tag(idx, typ, u64::from(level.child_count));
        self.pool.set_payload(idx, u64::from(level.first_child));
        self.value_complete();
        Ok(())
    }

    /// Handle the bytes between two structural characters.
    ///
    /// In a value position the gap may hold exactly one scalar; everywhere
    /// else it must be whitespace.
    fn gap(&mut self, to: usize) -> Result<(), Error> {
        let from = self.prev_end;
        let start = skip_ws(self.input, from, to);
        if start >= to {
            return Ok(());
        }

        if !matches!(self.expect, Expect::Value | Expect::ValueOrClose) {
            return self.err(start, self.expect_err());
        }

        let end = self.scalar(start, to)?;

        // Anything after the scalar in the same gap must be whitespace.
        if skip_ws(self.input, end, to) < to {
            return self.err(end, ErrorKind::ExpectedCommaOrClose);
        }
        self.value_complete();
        Ok(())
    }

    /// Parse one scalar starting at `start`, bounded by `to`. Returns the
    /// index just past it.
    fn scalar(&mut self, start: usize, to: usize) -> Result<usize, Error> {
        match self.byte(start) {
            b't' => {
                if self.input.get(start..start + 4) != Some(b"true".as_slice()) {
                    return self.err(start, ErrorKind::InvalidLiteral);
                }
                self.pool.push(Type::Bool, 0, 1);
                Ok(start + 4)
            }
            b'f' => {
                if self.input.get(start..start + 5) != Some(b"false".as_slice()) {
                    return self.err(start, ErrorKind::InvalidLiteral);
                }
                self.pool.push(Type::Bool, 0, 0);
                Ok(start + 5)
            }
            b'n' => {
                if self.input.get(start..start + 4) != Some(b"null".as_slice()) {
                    return self.err(start, ErrorKind::InvalidLiteral);
                }
                self.pool.push(Type::Null, 0, 0);
                Ok(start + 4)
            }
            b'-' | b'0'..=b'9' => self.number(start, to),
            _ => self.err(start, ErrorKind::ExpectedValue),
        }
    }

    /// RFC 8259 number: `-? (0 | [1-9][0-9]*) (. [0-9]+)? ([eE][+-]?[0-9]+)?`
    ///
    /// Stored as [`Type::Number`] (`i64` payload) when the literal is an
    /// exact integer that fits, otherwise as [`Type::Float`] (`f64` bits).
    fn number(&mut self, start: usize, to: usize) -> Result<usize, Error> {
        let mut p = start;
        let neg = self.byte(p) == b'-';
        if neg {
            p += 1;
        }

        // Integer part. Digits are accumulated as we validate them, so the
        // common case never touches `str::parse`.
        let int_start = p;
        let mut acc: u64 = 0;
        let mut digits: u32 = 0;
        match self.byte(p) {
            b'0' => {
                p += 1;
                digits = 1;
                // Leading zeros are not allowed: `01` is invalid, not two
                // tokens.
                if p < to && self.byte(p).is_ascii_digit() {
                    return self.err(int_start, ErrorKind::InvalidNumber);
                }
            }
            b'1'..=b'9' => {
                while p < to {
                    let b = self.byte(p);
                    if !b.is_ascii_digit() {
                        break;
                    }
                    acc = acc.wrapping_mul(10).wrapping_add(u64::from(b - b'0'));
                    digits += 1;
                    p += 1;
                }
            }
            _ => return self.err(p, ErrorKind::InvalidNumber),
        }

        let mut is_float = false;

        if p < to && self.byte(p) == b'.' {
            is_float = true;
            p += 1;
            let frac_start = p;
            while p < to && self.byte(p).is_ascii_digit() {
                p += 1;
            }
            if p == frac_start {
                return self.err(p, ErrorKind::InvalidNumber);
            }
        }

        if p < to && matches!(self.byte(p), b'e' | b'E') {
            is_float = true;
            p += 1;
            if p < to && matches!(self.byte(p), b'+' | b'-') {
                p += 1;
            }
            let exp_start = p;
            while p < to && self.byte(p).is_ascii_digit() {
                p += 1;
            }
            if p == exp_start {
                return self.err(p, ErrorKind::InvalidNumber);
            }
        }

        // Negative zero. RFC 8259 has one number type, so `-0` could be
        // reported as the integer 0 — but that discards the sign, and
        // `serde_json` keeps it by making the value `-0.0`. Matching that
        // preserves round-tripping: emitting `0` for an input of `-0`
        // changes the document.
        //
        // Note this cannot be caught by comparing values numerically:
        // `0.0 == -0.0` is true in IEEE 754. It took the `y_number_minus_zero`
        // case from JSONTestSuite to surface it.
        //
        // `digits == 1` is load-bearing. `acc` is a wrapping accumulator,
        // so a long literal can land on zero without being zero:
        // `-92233720368547758080` is 2^63 * 10, which is exactly 0 mod
        // 2^64, and this returned `-0.0` for it. Leading zeros are already
        // rejected, so the only single-digit literal reaching `acc == 0`
        // is a genuine `0`. Found by differential fuzzing against
        // `serde_json`.
        if neg && acc == 0 && digits == 1 && !is_float {
            self.pool.push(Type::Float, 0, (-0.0f64).to_bits());
            return Ok(p);
        }

        if !is_float {
            // 18 digits always fit (10^18 < i64::MAX). 19 might. 20+ never
            // do, and `acc` may already have wrapped, so check the digit
            // count before the value.
            let limit = if neg { 1u64 << 63 } else { i64::MAX as u64 };
            if digits < 19 || (digits == 19 && acc <= limit) {
                let v = if neg { acc.wrapping_neg() } else { acc } as i64;
                self.pool.push(Type::Number, 0, v as u64);
                return Ok(p);
            }
            // An integer literal that overflows i64 degrades to f64,
            // matching serde_json without `arbitrary_precision`.
        }

        let Some(text) = self.input.get(start..p) else {
            return self.err(start, ErrorKind::InvalidNumber);
        };
        let Ok(text) = core::str::from_utf8(text) else {
            return self.err(start, ErrorKind::InvalidNumber);
        };

        match text.parse::<f64>() {
            // A literal that overflows to infinity is an error, matching
            // serde_json — `1e308` is fine, `1e8808` is not.
            Ok(v) if v.is_finite() => {
                self.pool.push(Type::Float, 0, v.to_bits());
                Ok(p)
            }
            _ => self.err(start, ErrorKind::InvalidNumber),
        }
    }

    /// Check escape syntax, reject raw control bytes, and check UTF-8.
    fn check_string(&self, start: usize, end: usize) -> Result<(), Error> {
        let Some(raw) = self.input.get(start..end) else {
            return Err(Error {
                offset: start,
                kind: ErrorKind::InvalidString,
            });
        };

        // One branchless pass over the content answers all three cheap
        // questions at once. Writing it as an OR reduction with no early
        // exit is what lets LLVM vectorise it; an `iter().any()` would not,
        // and a per-16-byte "does this chunk need attention?" probe is worse
        // still on escape-dense data because it gets re-run after every
        // escape.
        let Flags {
            has_control,
            has_escape,
            has_non_ascii,
        } = scan_flags(raw);

        // Pure ASCII is valid UTF-8 by construction, so the common case
        // skips the second pass entirely.
        if has_non_ascii && core::str::from_utf8(raw).is_err() {
            return Err(Error {
                offset: start,
                kind: ErrorKind::InvalidString,
            });
        }

        if has_control {
            let at = raw.iter().position(|&b| b < 0x20).unwrap_or(0);
            return Err(Error {
                offset: start + at,
                kind: ErrorKind::InvalidString,
            });
        }
        if !has_escape {
            // The overwhelmingly common case: plain text, already known to
            // be valid UTF-8 with no control bytes.
            return Ok(());
        }

        // Slow path: validate escape sequences.
        let mut i = 0usize;
        while i < raw.len() {
            let b = raw.get(i).copied().unwrap_or(0);
            if b != b'\\' {
                i += 1;
                continue;
            }
            i += 1;
            match raw.get(i) {
                Some(b'"' | b'\\' | b'/' | b'b' | b'f' | b'n' | b'r' | b't') => i += 1,
                Some(b'u') => {
                    let bad = Error {
                        offset: start + i,
                        kind: ErrorKind::InvalidString,
                    };
                    let Some(hi) = hex4(raw.get(i + 1..i + 5)) else {
                        return Err(bad);
                    };
                    i += 5;

                    // Surrogates must be well paired. Syntax-checking the
                    // four hex digits is not enough: a lone `\ud83d` is
                    // rejected by every conformant parser, and skipping this
                    // is how the first version of this function disagreed
                    // with serde_json.
                    if (0xD800..0xDC00).contains(&hi) {
                        if raw.get(i..i + 2) != Some(br"\u".as_slice()) {
                            return Err(bad);
                        }
                        let Some(lo) = hex4(raw.get(i + 2..i + 6)) else {
                            return Err(bad);
                        };
                        if !(0xDC00..0xE000).contains(&lo) {
                            return Err(bad);
                        }
                        i += 6;
                    } else if (0xDC00..0xE000).contains(&hi) {
                        // A low surrogate with no high surrogate before it.
                        return Err(bad);
                    }
                }
                _ => {
                    return Err(Error {
                        offset: start + i,
                        kind: ErrorKind::InvalidString,
                    })
                }
            }
        }
        Ok(())
    }

    fn run(&mut self, positions: &[u32]) -> Result<(), Error> {
        let len = self.input.len();

        // A document with no structural characters is a bare scalar.
        if positions.is_empty() {
            let start = skip_ws(self.input, 0, len);
            if start >= len {
                return self.err(0, ErrorKind::Empty);
            }
            let end = self.scalar(start, len)?;
            if skip_ws(self.input, end, len) < len {
                return self.err(end, ErrorKind::TrailingContent);
            }
            self.pool.set_root(0);
            return Ok(());
        }

        let mut i = 0usize;
        while i < positions.len() {
            let pos = positions.get(i).copied().unwrap_or(0) as usize;
            let ch = self.byte(pos);

            self.gap(pos)?;

            match ch {
                b'{' => {
                    if !matches!(self.expect, Expect::Value | Expect::ValueOrClose) {
                        return self.err(pos, self.expect_err());
                    }
                    self.open(Type::Object, pos)?;
                    self.prev_end = pos + 1;
                    i += 1;
                }
                b'[' => {
                    if !matches!(self.expect, Expect::Value | Expect::ValueOrClose) {
                        return self.err(pos, self.expect_err());
                    }
                    self.open(Type::Array, pos)?;
                    self.prev_end = pos + 1;
                    i += 1;
                }
                b'}' => {
                    if !matches!(self.expect, Expect::KeyOrClose | Expect::CommaOrClose) {
                        return self.err(pos, self.expect_err());
                    }
                    self.close(Type::Object, pos)?;
                    self.prev_end = pos + 1;
                    i += 1;
                }
                b']' => {
                    if !matches!(self.expect, Expect::ValueOrClose | Expect::CommaOrClose) {
                        return self.err(pos, self.expect_err());
                    }
                    self.close(Type::Array, pos)?;
                    self.prev_end = pos + 1;
                    i += 1;
                }
                b':' => {
                    if self.expect != Expect::Colon {
                        return self.err(pos, ErrorKind::ExpectedColon);
                    }
                    self.expect = Expect::Value;
                    self.prev_end = pos + 1;
                    i += 1;
                }
                b',' => {
                    if self.expect != Expect::CommaOrClose {
                        return self.err(pos, self.expect_err());
                    }
                    self.expect = match self.kinds.last() {
                        Some(Type::Object) => Expect::Key,
                        Some(_) => Expect::Value,
                        // A comma at depth 0 is trailing content.
                        None => return self.err(pos, ErrorKind::TrailingContent),
                    };
                    self.prev_end = pos + 1;
                    i += 1;
                }
                b'"' => {
                    // Stage 1 emits unescaped quotes in pairs, so the next
                    // structural entry is this string's closer.
                    let Some(&close) = positions.get(i + 1) else {
                        return self.err(pos, ErrorKind::UnterminatedString);
                    };
                    let close = close as usize;
                    if self.byte(close) != b'"' {
                        return self.err(pos, ErrorKind::UnterminatedString);
                    }
                    let start = pos + 1;
                    let content_len = close.saturating_sub(start);

                    if self.opts.validate_strings {
                        self.check_string(start, close)?;
                    }

                    match self.expect {
                        Expect::Key | Expect::KeyOrClose => {
                            self.pool
                                .push(Type::Key, content_len as u64, start as u64);
                            self.expect = Expect::Colon;
                        }
                        Expect::Value | Expect::ValueOrClose => {
                            self.pool
                                .push(Type::String, content_len as u64, start as u64);
                            self.value_complete();
                        }
                        _ => return self.err(pos, self.expect_err()),
                    }

                    self.prev_end = close + 1;
                    i += 2;
                }
                _ => i += 1,
            }
        }

        // The bytes after the last structural character. This is a normal
        // gap, not just trailing whitespace: `[1,2` ends with a value that
        // no closing bracket follows, and `42` with a stray `{` earlier
        // relies on the same path.
        self.gap(len)?;

        if !self.stack.is_empty() {
            return self.err(len, ErrorKind::UnexpectedEof);
        }
        if self.expect != Expect::Done {
            return self.err(len, ErrorKind::UnexpectedEof);
        }

        self.pool.set_root(0);
        Ok(())
    }

    fn expect_err(&self) -> ErrorKind {
        match self.expect {
            Expect::Value | Expect::ValueOrClose => ErrorKind::ExpectedValue,
            Expect::Key | Expect::KeyOrClose => ErrorKind::ExpectedKey,
            Expect::Colon => ErrorKind::ExpectedColon,
            Expect::CommaOrClose => ErrorKind::ExpectedCommaOrClose,
            Expect::Done => ErrorKind::TrailingContent,
        }
    }
}

/// Cheap properties of a string literal's raw content.
#[derive(Debug, Clone, Copy)]
struct Flags {
    /// Contains a byte below 0x20, which JSON forbids unescaped.
    has_control: bool,
    /// Contains a backslash, so escape sequences must be validated.
    has_escape: bool,
    /// Contains a byte >= 0x80, so UTF-8 validation is actually needed.
    has_non_ascii: bool,
}

/// Compute [`Flags`] in a single pass.
///
/// Deliberately has no early exit: the OR reduction over fixed-size chunks
/// is what makes LLVM emit vector compares. Bailing out early would be
/// asymptotically better and measurably slower.
#[inline]
fn scan_flags(raw: &[u8]) -> Flags {
    let mut control = 0u8;
    let mut escape = 0u8;
    let mut high = 0u8;

    let (chunks, tail) = raw.as_chunks::<16>();
    for c in chunks {
        for &b in c {
            control |= u8::from(b < 0x20);
            escape |= u8::from(b == b'\\');
            high |= b;
        }
    }
    for &b in tail {
        control |= u8::from(b < 0x20);
        escape |= u8::from(b == b'\\');
        high |= b;
    }

    Flags {
        has_control: control != 0,
        has_escape: escape != 0,
        has_non_ascii: high & 0x80 != 0,
    }
}

/// Decode exactly four hex digits.
#[inline]
fn hex4(bytes: Option<&[u8]>) -> Option<u16> {
    let bytes = bytes?;
    if bytes.len() != 4 {
        return None;
    }
    let mut v = 0u16;
    for &b in bytes {
        let d = match b {
            b'0'..=b'9' => b - b'0',
            b'a'..=b'f' => b - b'a' + 10,
            b'A'..=b'F' => b - b'A' + 10,
            _ => return None,
        };
        v = (v << 4) | u16::from(d);
    }
    Some(v)
}

/// One-shot validating parse into an owned pool.
pub fn parse_to_pool(input: &[u8]) -> Result<Pool, Error> {
    let mut p = StrictParser::with_capacity(input.len());
    p.run(input)?;
    Ok(p.pool.clone())
}

/// Validate without building anything the caller keeps.
pub fn validate(input: &[u8]) -> Result<(), Error> {
    StrictParser::new().validate(input)
}
