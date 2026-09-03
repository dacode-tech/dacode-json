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
//! | [`pull`], [`mod@write`] | read and write JSON with no allocator at all |
//! | `tiers`, `onepass` | reference implementations, for measurement (`vela-compat` feature) |
//!
//! # `no_std`
//!
//! `--no-default-features` gives a crate that builds for a bare-metal
//! target with no OS and no allocator — [`pull`] to read fields out of a
//! document, [`mod@write`] to build one into a `&mut [u8]`, and the byte
//! classifiers. Neither allocates, including on the error path.
//!
//! ```toml
//! dacodec = { version = "0.1", default-features = false }            # core
//! dacodec = { version = "0.1", default-features = false, features = ["alloc"] }
//! ```
//!
//! `alloc` adds the node pool and the [`flat`] builders; `std` adds
//! [`from_reader`] and the measurement machinery; `serde` adds the typed
//! API. See `docs/NOSTD.md` for what is in each tier and why.
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
    clippy::unreachable,
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
        clippy::unreachable,
        clippy::indexing_slicing,
        clippy::unwrap_in_result
    )
)]

#![cfg_attr(not(feature = "std"), no_std)]

#[cfg(feature = "alloc")]
extern crate alloc;

mod errmsg;

// =====================================================================
// Modules, grouped by what they need
// =====================================================================
//
// The grouping is the point, not decoration: `--no-default-features`
// leaves the first group and nothing else, and that is the surface an
// embedded target gets. See `docs/NOSTD.md`.

// --- neither an allocator nor an OS ----------------------------------

// No `///` summaries on these declarations. An outer doc comment on a
// `pub mod` is concatenated with the module's own `//!` docs but
// resolved in *this* module's scope, so every intra-doc link in the
// merged text breaks. Each module introduces itself.
pub mod pull;
pub mod scalar;
pub mod scan;
pub mod stream;
pub mod tag;
pub mod unescape;
pub mod write;

// --- an allocator, but no OS -----------------------------------------

#[cfg(feature = "alloc")]
pub mod builder;
#[cfg(feature = "alloc")]
pub mod corpus;
#[cfg(feature = "alloc")]
pub mod flat;
#[cfg(feature = "alloc")]
pub mod pool;
#[cfg(feature = "alloc")]
pub mod query;
#[cfg(feature = "alloc")]
pub mod strict;
#[cfg(feature = "alloc")]
pub mod workspace;

#[cfg(all(feature = "alloc", feature = "vela-compat"))]
pub mod onepass;
#[cfg(all(feature = "alloc", feature = "vela-compat"))]
pub mod tiers;

// --- `serde`, which implies an allocator -----------------------------

#[cfg(feature = "serde")]
pub mod api;
#[cfg(feature = "serde")]
pub mod de;
#[cfg(feature = "serde")]
pub mod direct;
#[cfg(feature = "serde")]
pub mod ser;

// --- an OS ------------------------------------------------------------

#[cfg(all(feature = "std", feature = "cbench"))]
pub mod cbench;
// `fuzz` cross-checks the reference tiers, so it needs `vela-compat`,
// which `fuzzing` pulls in. Gating it on `profiling` alone broke
// `--features profiling` once the tiers became optional.
#[cfg(all(feature = "std", feature = "fuzzing"))]
pub mod fuzz;
#[cfg(feature = "std")]
pub mod memstat;

// =====================================================================
// Re-exports
// =====================================================================

#[cfg(feature = "serde")]
pub use api::{
    from_slice, from_str, to_string, to_vec, to_writer, validate, Document, Error, Parser, Result,
    ValueRef,
};
#[cfg(all(feature = "serde", feature = "std"))]
pub use api::from_reader;

#[cfg(feature = "alloc")]
pub use pool::{Node, Pool};
#[cfg(feature = "alloc")]
pub use query::{Doc, Value};
#[cfg(feature = "alloc")]
pub use scan::StructuralIndex;
#[cfg(feature = "alloc")]
pub use workspace::Workspace;
pub use scan::Scanner;
pub use tag::Type;

/// Parse into an owned [`Pool`] with a fresh allocation — Vela's
/// `json_pool_parse_fast`.
///
/// Prefer [`Workspace`] when parsing more than once.
#[cfg(feature = "alloc")]
#[must_use]
pub fn parse_to_pool(input: &[u8]) -> Pool {
    workspace::parse_to_pool(input)
}
