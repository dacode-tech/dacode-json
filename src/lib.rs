//! A fast, allocation-light JSON library for Rust.
//!
//! ```
//! # use serde::{Serialize, Deserialize};
//! #[derive(Serialize, Deserialize)]
//! struct Config { name: String, port: u16 }
//!
//! let cfg: Config = dacodec::from_str(r#"{"name":"edge","port":8080}"#)?;
//! let json = dacodec::to_string(&cfg)?;
//! # Ok::<(), dacodec::Error>(())
//! ```
//!
//! For the typed path this is a drop-in replacement for `serde_json`:
//! change the import and nothing else. Output is byte-identical, and the
//! parser scores 284/284 on the mandatory JSONTestSuite cases.
//!
//! # What is here
//!
//! | | |
//! |---|---|
//! | [`from_str`], [`to_string`] and friends | the `serde_json`-shaped API |
//! | [`Parser`] | reusable buffers; no allocation in steady state |
//! | [`flat`] | a zero-copy wire format — read a field in nanoseconds, no parsing |
//! | `tiers`, `onepass` | reference implementations, for measurement (`vela-compat` feature) |
//!
//! # Zero-copy
//!
//! [`flat`] stores a document in a self-contained, `mmap`-able buffer.
//! Opening one is O(1) and reading a field allocates nothing:
//!
//! | reading a 10.4 MiB document | allocations | bytes |
//! |---|---|---|
//! | `flat::typed` | **2** | **36 B** |
//! | `serde_json` (must parse) | 1 310 661 | 82.99 MiB |
//!
//! See `docs/ZEROCOPY.md` and `docs/MEMORY.md`.
//!
//! # Origins
//!
//! The parser began as a port of the JSON tiers in the
//! [Vela](https://github.com/) compiler's standard library, themselves
//! modelled on [yyjson](https://github.com/ibireme/yyjson) (a flat node
//! pool) and [simdjson](https://github.com/simdjson/simdjson) (a SIMD
//! structural index). Those ports are kept behind `vela-compat` and measured
//! against the C originals in `docs/CBASELINE.md`; the shipping parser is
//! [`strict`], which adds RFC 8259 validation and correctly-rounded
//! numbers.
//!
//! # Panics
//!
//! There are none. No `unwrap`, `expect`, `panic!` or slice indexing exists
//! on any path reachable from the public API, enforced by `clippy::deny`
//! and checked by fuzzing. See `docs/UNWRAP_FREE.md`.

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

#[cfg(feature = "cbench")]
pub mod cbench;
// `fuzz` cross-checks the reference tiers, so it needs `vela-compat`,
// which `fuzzing` pulls in. Gating it on `profiling` alone broke
// `--features profiling` once the tiers became optional.
#[cfg(feature = "fuzzing")]
pub mod fuzz;
pub mod builder;
pub mod corpus;
pub mod memstat;
pub mod flat;
#[cfg(feature = "serde")]
pub mod de;
#[cfg(feature = "serde")]
pub mod ser;
/// Vela's single-pass builder. Requires the `vela-compat` feature.
///
/// A reference implementation, not the recommended parser: it does not
/// validate and truncates floats. Use [`strict`] or the crate root.
#[cfg(feature = "vela-compat")]
pub mod onepass;
pub mod pool;
pub mod query;
pub mod scalar;
pub mod scan;
/// A no-index streaming deserializer, for measuring what Stage 1 is
/// worth. See `docs/RESULTS.md`.
#[cfg(feature = "serde")]
pub mod direct;
/// On-demand extraction: pull named fields, skip the rest wholesale.
/// Allocates nothing. See the module docs for the validation trade.
#[cfg(feature = "serde")]
pub mod pull;
pub mod stream;
pub mod strict;
/// The Vela JSON tiers, ported faithfully. Requires the `vela-compat`
/// feature.
///
/// Reference implementations kept for measurement. They do not validate
/// and they truncate floats, because Vela's do. Use [`strict`] or the
/// crate root for anything real; see `docs/TIERS.md`.
#[cfg(feature = "vela-compat")]
pub mod tiers;
pub mod unescape;
pub mod workspace;

pub mod api;
pub use api::{
    from_reader, from_slice, from_str, to_string, to_vec, to_writer, validate, Document,
    Error, Parser, Result, ValueRef,
};

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
