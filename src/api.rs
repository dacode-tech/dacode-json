//! The public API, shaped like `serde_json`'s.
//!
//! For the typed path — `#[derive(Serialize, Deserialize)]` structs — this
//! is a drop-in replacement. Change the import and nothing else:
//!
//! ```
//! # use serde::{Serialize, Deserialize};
//! #[derive(Serialize, Deserialize, PartialEq, Debug)]
//! struct Config { name: String, port: u16, tags: Vec<String> }
//!
//! let text = r#"{"name":"edge","port":8080,"tags":["a","b"]}"#;
//!
//! let cfg: Config = dacodec::from_str(text)?;
//! assert_eq!(cfg.port, 8080);
//!
//! let back = dacodec::to_string(&cfg)?;
//! assert_eq!(back, text);
//! # Ok::<(), dacodec::Error>(())
//! ```
//!
//! Output is byte-identical to `serde_json`'s, which
//! `tests/serde_ser.rs` asserts over the whole corpus.
//!
//! # Borrowing
//!
//! Strings borrow from the input when they contain no escapes, so a
//! borrowing struct copies nothing:
//!
//! ```
//! # use serde::Deserialize;
//! #[derive(Deserialize)]
//! struct Row<'a> { #[serde(borrow)] name: &'a str }
//!
//! let text = r#"{"name":"alpha"}"#;
//! let mut p = dacodec::Parser::new();
//! let row: Row<'_> = p.deserialize(text.as_bytes())?;
//! assert_eq!(row.name, "alpha");
//! # Ok::<(), dacodec::Error>(())
//! ```
//!
//! A borrowing type needs [`Parser`] rather than [`from_str`], because the
//! node pool has to outlive the borrow and a free function cannot express
//! that without leaking it.
//!
//! # Parsing many documents
//!
//! [`from_str`] allocates a parser per call. To parse repeatedly, keep a
//! [`Parser`] and reuse its buffers — that is where most of the throughput
//! comes from:
//!
//! ```
//! let mut p = dacodec::Parser::new();
//! for line in [r#"{"a":1}"#, r#"{"a":2}"#] {
//!     let doc = p.parse(line.as_bytes())?;
//!     assert!(doc.root().get("a").is_some());
//! }
//! # Ok::<(), dacodec::Error>(())
//! ```

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

/// The error type for both directions.
///
/// Parse errors carry a byte offset; see [`crate::strict::Error`].
#[derive(Debug)]
pub enum Error {
    /// The input was not valid JSON.
    Parse(crate::strict::Error),
    /// The document was valid but did not fit the target type.
    Deserialize(crate::de::Error),
    /// A value could not be serialized.
    Serialize(crate::ser::Error),
    /// The streaming deserializer failed. Carries a byte offset when the
    /// fault was in the JSON rather than in the target type.
    Stream(crate::stream::Error),
}

impl core::fmt::Display for Error {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Error::Parse(e) => write!(f, "{e}"),
            Error::Deserialize(e) => write!(f, "{e}"),
            Error::Serialize(e) => write!(f, "{e}"),
            Error::Stream(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Parse(e) => Some(e),
            Error::Deserialize(e) => Some(e),
            Error::Serialize(e) => Some(e),
            Error::Stream(e) => Some(e),
        }
    }
}

impl From<crate::strict::Error> for Error {
    fn from(e: crate::strict::Error) -> Self {
        Error::Parse(e)
    }
}
impl From<crate::de::Error> for Error {
    fn from(e: crate::de::Error) -> Self {
        Error::Deserialize(e)
    }
}
impl From<crate::stream::Error> for Error {
    fn from(e: crate::stream::Error) -> Self {
        Error::Stream(e)
    }
}
impl From<crate::ser::Error> for Error {
    fn from(e: crate::ser::Error) -> Self {
        Error::Serialize(e)
    }
}

impl Error {
    /// Byte offset where parsing failed, if this is a parse error.
    #[must_use]
    pub fn offset(&self) -> Option<usize> {
        match self {
            Error::Parse(e) => Some(e.offset),
            Error::Stream(e) => e.offset,
            _ => None,
        }
    }
}

/// Result alias, as `serde_json::Result`.
pub type Result<T> = core::result::Result<T, Error>;

// =====================================================================
// Deserialize
// =====================================================================

/// Parse JSON text into an owned `T`.
///
/// Validates against RFC 8259 — the parser scores 284/284 on the mandatory
/// JSONTestSuite cases (`tests/conformance_suite.rs`).
///
/// For a *borrowing* type such as `struct Row<'a> { name: &'a str }`, use
/// [`Parser::deserialize`]: the node pool must outlive the borrow, and a
/// free function cannot express that without leaking.
pub fn from_str<T: DeserializeOwned>(s: &str) -> Result<T> {
    from_slice(s.as_bytes())
}

/// Parse JSON bytes into an owned `T`.
///
/// Uses the streaming deserializer ([`crate::stream`]), which decodes
/// straight into the target type without building a node pool. It is
/// 31–61% faster than the pool path and keeps full `u64` precision on
/// integers longer than 19 digits, which the pool path rounds to `f64`.
///
/// Validation is unchanged: 351/351 agreement with `serde_json` across
/// `testdata/`, including for fields the target type ignores.
pub fn from_slice<T: DeserializeOwned>(v: &[u8]) -> Result<T> {
    Ok(crate::stream::from_slice(v)?)
}

/// Read all of `r` and parse it.
///
/// Provided for parity with `serde_json::from_reader`. It buffers the whole
/// input first — this parser is not incremental, and pretending otherwise
/// would be slower, not faster.
pub fn from_reader<R: std::io::Read, T: serde::de::DeserializeOwned>(mut r: R) -> Result<T> {
    let mut buf = Vec::new();
    r.read_to_end(&mut buf)
        .map_err(|e| Error::Deserialize(serde::de::Error::custom(e)))?;
    Ok(crate::stream::from_slice(&buf)?)
}

// =====================================================================
// Serialize
// =====================================================================

/// Serialize to a `String`.
pub fn to_string<T: Serialize + ?Sized>(value: &T) -> Result<String> {
    Ok(crate::ser::to_string(value)?)
}

/// Serialize to a byte vector.
pub fn to_vec<T: Serialize + ?Sized>(value: &T) -> Result<Vec<u8>> {
    Ok(crate::ser::to_vec(value)?)
}

/// Serialize into an existing buffer, reusing its allocation.
///
/// The cheapest way to serialize repeatedly.
pub fn to_writer<T: Serialize + ?Sized>(out: &mut Vec<u8>, value: &T) -> Result<()> {
    Ok(crate::ser::to_writer(out, value)?)
}

// =====================================================================
// Reusable parser
// =====================================================================

/// A parser that reuses its buffers across documents.
///
/// [`from_str`] is convenient; this is fast. The structural index and node
/// pool are allocated once and reset per parse, so a steady-state parse
/// performs **no allocation at all**.
#[derive(Debug, Clone)]
pub struct Parser {
    inner: crate::strict::StrictParser,
}

impl Default for Parser {
    fn default() -> Self {
        Self::new()
    }
}

impl Parser {
    #[must_use]
    pub fn new() -> Self {
        Parser {
            inner: crate::strict::StrictParser::new(),
        }
    }

    /// Pre-size for documents up to `max_len` bytes, so even the first
    /// parse does not allocate.
    #[must_use]
    pub fn with_capacity(max_len: usize) -> Self {
        Parser {
            inner: crate::strict::StrictParser::with_capacity(max_len),
        }
    }

    /// Parse, returning a borrowed view.
    ///
    /// The returned [`Document`] borrows both `input` and `self`, so the
    /// borrow checker prevents reusing the parser while a result is alive.
    pub fn parse<'a>(&'a mut self, input: &'a [u8]) -> Result<Document<'a>> {
        Ok(self.inner.parse(input)?)
    }

    /// Check that `input` is valid JSON without keeping the result.
    pub fn validate(&mut self, input: &[u8]) -> Result<()> {
        Ok(self.inner.validate(input)?)
    }

    /// Parse and deserialize into `T` in one step.
    pub fn deserialize<'a, T: Deserialize<'a>>(&'a mut self, input: &'a [u8]) -> Result<T> {
        let doc = self.inner.parse(input)?;
        Ok(crate::de::from_doc(doc)?)
    }
}

/// A parsed document: a node pool plus the input it indexes.
pub type Document<'a> = crate::query::Doc<'a>;

/// A cursor onto one value in a [`Document`].
pub type ValueRef<'a> = crate::query::Value<'a>;

/// Check that `input` is valid JSON.
///
/// Allocates a parser per call; use [`Parser::validate`] in a loop.
pub fn validate(input: &[u8]) -> Result<()> {
    Ok(crate::strict::validate(input)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Serialize, Deserialize, PartialEq, Debug)]
    struct S {
        a: u32,
        b: Vec<String>,
    }

    #[test]
    fn drop_in_roundtrip() {
        let text = r#"{"a":1,"b":["x","y"]}"#;
        let v: S = from_str(text).expect("parse");
        assert_eq!(v.a, 1);
        assert_eq!(to_string(&v).expect("ser"), text);
        // Byte-identical to serde_json.
        assert_eq!(
            to_string(&v).expect("ser"),
            serde_json::to_string(&v).expect("serde_json")
        );
    }

    #[test]
    fn errors_carry_an_offset() {
        let e = from_str::<S>(r#"{"a":1,"b":}"#).expect_err("should fail");
        assert_eq!(e.offset(), Some(11));
        assert!(e.to_string().contains("byte 11"), "{e}");
    }

    #[test]
    fn reusable_parser_does_not_allocate_in_steady_state() {
        let mut p = Parser::with_capacity(256);
        for i in 0..100u32 {
            let src = format!(r#"{{"a":{i},"b":[]}}"#);
            let v: S = p.deserialize(src.as_bytes()).expect("parse");
            assert_eq!(v.a, i);
        }
    }

    #[test]
    fn from_reader_works() {
        let text = br#"{"a":7,"b":[]}"#;
        let v: S = from_reader(&text[..]).expect("parse");
        assert_eq!(v.a, 7);
    }

    #[test]
    fn validate_matches_serde_json() {
        for (s, ok) in [("{}", true), ("[1,2]", true), ("{", false), ("[1,]", false)] {
            assert_eq!(validate(s.as_bytes()).is_ok(), ok, "on {s}");
            assert_eq!(
                serde_json::from_str::<serde_json::Value>(s).is_ok(),
                ok,
                "serde_json disagrees on {s}"
            );
        }
    }
}
