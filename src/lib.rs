//! A Rust port of Vela stage2's **tier 3** JSON parser.
//!
//! Vela ships four JSON tiers under
//! `bootstrap/stage2/src/stdlib/encoding/json/`. Tier 3 is the fastest
//! end-to-end path: a yyjson-style flat 16-byte node pool fed by a
//! simdjson-style structural index (Vela's own tier 2 Stage 1).
//! `docs/stage2/JSON_IMPROVEMENT_PLAN.md:52-73` measures the shipped
//! implementation at 166 MB/s for a full DOM build on a 10 MB corpus, with
//! Stage 1 alone hitting 914 MB/s — so the DOM builder is the bottleneck,
//! which is what makes it interesting to re-host on rustc/LLVM.
//!
//! # Two parsers
//!
//! * [`Workspace`] — the **faithful** port. Bit-compatible with Vela,
//!   including every quirk (see below). No validation, no errors.
//! * [`strict`] — same data structures and same algorithm, but RFC
//!   8259-conformant: real `f64` numbers, structural validation, and a
//!   [`strict::Error`] with a byte offset.
//!
//! # Faithful-mode quirks
//!
//! These are all real behaviours of the Vela implementation, reproduced
//! deliberately. Each is documented at the site that implements it.
//!
//! | Quirk | Where |
//! |---|---|
//! | Numbers are `i64`; `3.14` parses as `314`, `1e3` as `13` | [`scalar::parse_number`] |
//! | `null` is never validated — `nope` parses as null | [`scalar::parse_scalar_fast`] |
//! | Any unrecognised scalar becomes null | [`scalar::parse_scalar_fast`] |
//! | Strings are raw slices, never unescaped | [`query::Value::as_raw_str`] |
//! | Key lookup compares raw (still-escaped) bytes | [`query::Value::get`] |
//! | Depth past 256 is silently dropped | [`builder`] |
//! | Closing brackets are not matched against openers | [`builder`] |
//! | Inputs above `i32::MAX` are rejected | [`scan::MAX_INPUT_LEN`] |
//! | No errors: malformed input yields a wrong-but-valid pool | [`builder`] |
//!
//! # Example
//!
//! ```
//! use vela_json::Workspace;
//!
//! let mut ws = Workspace::new();
//! let doc = ws.parse(br#"{"name":"vela","tiers":[0,1,2,3]}"#);
//!
//! let root = doc.root();
//! assert_eq!(root.get("name").and_then(|v| v.as_str()).as_deref(), Some("vela"));
//! assert_eq!(root.get("tiers").map(|v| v.len()), Some(4));
//! ```
//!
//! # Panics
//!
//! The faithful parser contains no `unwrap`, `expect`, `panic!` or slice
//! indexing on any path reachable from [`Workspace::parse`]. Malformed input
//! cannot panic it. See `docs/UNWRAP_FREE.md` in the repository for how far
//! that idea generalises.

#![forbid(unsafe_op_in_unsafe_fn)]
#![warn(missing_debug_implementations, rust_2018_idioms)]
// Panic-freedom is a property of this crate, so it is enforced rather than
// asserted. See `docs/UNWRAP_FREE.md`.
#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::unwrap_in_result,
    clippy::exit
)]
// ...but only for library code. Panic-freedom is a property of what ships;
// a test asserting with `assert_eq!` is already a `panic!`, and contorting
// test code to avoid one buys nothing. See `docs/UNWRAP_FREE.md` §2.
#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::indexing_slicing,
        clippy::unwrap_in_result
    )
)]

pub mod builder;
pub mod corpus;
pub mod flat;
#[cfg(feature = "serde")]
pub mod de;
#[cfg(feature = "serde")]
pub mod ser;
pub mod pool;
pub mod query;
pub mod scalar;
pub mod scan;
pub mod strict;
pub mod tiers;
pub mod unescape;
pub mod workspace;

pub use pool::{Node, Pool};
pub use query::{Doc, Value};
pub use scan::{Scanner, StructuralIndex};
pub use tag::Type;
pub use workspace::Workspace;

pub mod tag;

/// Parse into an owned [`Pool`] with a fresh allocation — Vela's
/// `json_pool_parse_fast`.
///
/// Prefer [`Workspace`] when parsing more than once.
#[must_use]
pub fn parse_to_pool(input: &[u8]) -> Pool {
    workspace::parse_to_pool(input)
}
