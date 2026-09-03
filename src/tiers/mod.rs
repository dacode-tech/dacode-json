//! All four Vela JSON tiers, ported faithfully. Requires the
//! `vela-compat` feature.
//!
//! Reference implementations kept for measurement, not for use: they do
//! not validate and they truncate floats, because Vela's do. Use
//! [`crate::strict`] or the crate root for anything real, and see
//! `docs/TIERS.md`.
//!
//! Vela ships four interchangeable JSON backends under
//! `bootstrap/stage2/src/stdlib/encoding/json/`, selected at build time by
//! `--define json_tier=N` (`BUILD.bazel:99-150`). Exactly one is compiled
//! per link, which is why all four define the same `pub fn` names without
//! colliding.
//!
//! | tier | algorithm | Vela LOC | ported to |
//! |---|---|---|---|
//! | 0 | scalar detection only, no containers | 289 | [`tier0`] |
//! | 1 | recursive descent, raw-substring navigation (**default**) | 590 | [`tier1`] |
//! | 2 | simdjson-style structural index + tape DOM | 1619 | [`tier2`] |
//! | 3 | yyjson-style flat node pool | 2063 | [`tier3`] |
//!
//! # The contract
//!
//! `docs/stage2/P1_2_JSON_TIERS.md:109-141` specifies a common interface all
//! tiers must export "so consumers don't need to know which tier is active",
//! with tier 0 returning `""` / `0` / `"unknown"` for anything it cannot do.
//! [`JsonTier`] is that contract as a Rust trait, which is what lets
//! `benches/tiers.rs` and `tests/tier_contract.rs` treat the four
//! generically.
//!
//! # Two honest caveats about the contract
//!
//! **It is not actually uniform.** Tier 0's container functions are stubs
//! returning empty — a consumer that "doesn't need to know which tier is
//! active" will silently get wrong answers on tier 0. That is Vela's design,
//! reproduced here; [`JsonTier::HANDLES_CONTAINERS`] lets a caller find out.
//!
//! **The tiers disagree on malformed input.** None of tiers 0–3 validates
//! during navigation, and each fails differently. Where they agree — valid
//! JSON — `tests/tier_contract.rs` checks all four against each other on
//! every corpus and 20 000 random documents.
//!
//! # Ownership and copying
//!
//! Vela's navigation functions return `string`, which in Vela is a
//! heap-allocated value; `input.slice(a, b)` copies. The ports return
//! `&[u8]` borrowed from the input wherever Vela sliced, because that is
//! what the Vela comment claims ("zero-allocation navigation",
//! `tier1/parse.vl:3`) even though the implementation does not deliver it.
//! Where Vela genuinely builds a new string — `json_parse_string`,
//! `json_object_keys` — the port allocates too.

pub mod common;
pub mod tier0;
pub mod tier1;
pub mod tier2;
pub mod tier3;

use std::borrow::Cow;

/// Which tier an implementation is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Tier {
    /// Scalar detection only. Container operations are stubs.
    Zero,
    /// Recursive descent over raw bytes. Vela's default.
    One,
    /// Structural index + tape DOM.
    Two,
    /// Flat node pool.
    Three,
}

impl Tier {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Tier::Zero => "tier0_scalar",
            Tier::One => "tier1_descent",
            Tier::Two => "tier2_tape",
            Tier::Three => "tier3_pool",
        }
    }

    /// The `--define json_tier=N` value.
    #[must_use]
    pub const fn number(self) -> u8 {
        match self {
            Tier::Zero => 0,
            Tier::One => 1,
            Tier::Two => 2,
            Tier::Three => 3,
        }
    }

    pub const ALL: [Tier; 4] = [Tier::Zero, Tier::One, Tier::Two, Tier::Three];
}

/// What a JSON value is, as reported by `json_detect_type`.
///
/// Vela returns a `string` here; an enum is the same information without the
/// allocation. [`TypeName::as_str`] gives back Vela's exact spelling.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TypeName {
    String,
    Number,
    Bool,
    Null,
    Object,
    Array,
    /// Vela's `"unknown"` — empty input or an unrecognised first byte.
    Unknown,
}

impl TypeName {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            TypeName::String => "string",
            TypeName::Number => "number",
            TypeName::Bool => "bool",
            TypeName::Null => "null",
            TypeName::Object => "object",
            TypeName::Array => "array",
            TypeName::Unknown => "unknown",
        }
    }
}

/// The interface every tier exports, from
/// `docs/stage2/P1_2_JSON_TIERS.md:109-141`.
///
/// Implementors are zero-sized marker types ([`tier0::Tier0`] and friends),
/// so dispatch is monomorphised and a generic benchmark costs nothing.
pub trait JsonTier {
    /// Which tier this is.
    const TIER: Tier;

    /// `false` for tier 0, whose container functions are stubs.
    ///
    /// Tests and benchmarks use this to skip assertions tier 0 cannot
    /// satisfy, rather than pretending the contract is uniform.
    const HANDLES_CONTAINERS: bool = true;

    // --- scalars: implemented by every tier ---

    /// `json_parse_string` — decode the string literal at the start of
    /// `input`, or empty if there is not one.
    ///
    /// Lossy in every Vela tier: `\b` and `\f` decode to a space and
    /// `\uXXXX` decodes to `?`. See [`crate::unescape::unescape_vela_lossy`].
    fn parse_string(input: &[u8]) -> String;

    /// `json_parse_number` — leading integer, digits only. Stops at the
    /// first non-digit, so `3.14` yields `3` and `42abc` yields `42`.
    fn parse_number(input: &[u8]) -> i64;

    /// `json_parse_bool` — `true` if the input starts with `true`.
    ///
    /// Note this returns `false` both for `false` and for garbage; the two
    /// are indistinguishable. That is Vela's signature.
    fn parse_bool(input: &[u8]) -> bool;

    /// `json_validate_string` — is the leading token a well-formed string
    /// literal?
    fn validate_string(input: &[u8]) -> bool;

    /// `json_detect_type` — classify by first non-whitespace byte.
    fn detect_type(input: &[u8]) -> TypeName;

    // --- containers: stubs in tier 0 ---

    /// `json_skip_value` — index one past the value starting at `pos`.
    fn skip_value(input: &[u8], pos: usize) -> usize;

    /// `json_object_get` — raw JSON text of the value for `key`, or `None`.
    fn object_get<'a>(input: &'a [u8], key: &str) -> Option<&'a [u8]>;

    /// `json_object_count` — number of key/value pairs.
    fn object_count(input: &[u8]) -> usize;

    /// `json_object_keys` — keys joined with `,`.
    ///
    /// A comma-joined string is a poor return type (keys containing commas
    /// are ambiguous) but it is the shipped signature, so it is preserved.
    /// Prefer [`JsonTier::object_key_list`].
    fn object_keys(input: &[u8]) -> String;

    /// The same keys as a list, without the ambiguity.
    fn object_key_list(input: &[u8]) -> Vec<Cow<'_, str>>;

    /// `json_array_get` — raw JSON text of element `idx`, or `None`.
    fn array_get(input: &[u8], idx: usize) -> Option<&[u8]>;

    /// `json_array_count` — number of elements.
    fn array_count(input: &[u8]) -> usize;

    /// `json_validate` — does the whole input parse as one JSON value?
    fn validate(input: &[u8]) -> bool;

    /// `json_parse_value_at` — raw JSON text of the value at `pos`.
    fn parse_value_at(input: &[u8], pos: usize) -> Option<&[u8]>;
}
