//! Node tag encoding.
//!
//! Port of `idx_make_tag` / `tag_type` / `tag_aux` from
//! `bootstrap/stage2/src/stdlib/encoding/json/tier3/parse_indexed.vl:68-70`
//! and `tier3/parse.vl:46-51`.
//!
//! Vela stores the tag as an `i64` computed as `aux * 256 + typ`, and decodes
//! with `tag % 256` / `tag / 256`. Because `aux` and `typ` are always
//! non-negative in practice, that is exactly a shift/mask pair.

/// Value type codes. Must match `IDX_T_*` in `parse_indexed.vl:56-62` and the
/// hardcoded constants in `runtime/json_pool_write.ll`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u8)]
pub enum Type {
    /// `null` — payload 0, aux 0.
    Null = 0,
    /// `true`/`false` — payload 1/0, aux 0.
    Bool = 1,
    /// Number — payload is the parsed `i64`, aux 0.
    ///
    /// Note: Vela's tier 3 is integer-only; see [`crate::scalar`].
    Number = 2,
    /// String value — payload is a byte offset into the input, aux is the
    /// raw (still-escaped) byte length.
    String = 3,
    /// Array — payload is the index of the first child node, aux is the
    /// element count.
    Array = 4,
    /// Object — payload is the index of the first child node (a `Key`),
    /// aux is the *pair* count.
    Object = 5,
    /// Object key — payload is a byte offset, aux is the raw byte length.
    /// Always immediately followed by its value node.
    Key = 6,
    /// Non-integer number — payload is `f64::to_bits`.
    ///
    /// **Not present in Vela.** Tier 3 is integer-only and destroys the
    /// fractional part (see [`crate::scalar::parse_number`]); this variant
    /// exists so [`crate::strict`] can be RFC 8259-conformant while reusing
    /// the same 16-byte node and the same query API. The faithful parser
    /// never emits it.
    Float = 7,
}

impl Type {
    /// Decode a raw type code. Anything outside 0..=6 is reported as `Null`,
    /// matching Vela's behaviour where unknown tags fall through every
    /// `if typ == T_X()` test.
    #[inline]
    #[must_use]
    pub const fn from_code(code: u8) -> Self {
        match code {
            1 => Type::Bool,
            2 => Type::Number,
            3 => Type::String,
            4 => Type::Array,
            5 => Type::Object,
            6 => Type::Key,
            7 => Type::Float,
            _ => Type::Null,
        }
    }

    /// `true` for `Array` and `Object`.
    #[inline]
    #[must_use]
    pub const fn is_container(self) -> bool {
        matches!(self, Type::Array | Type::Object)
    }

    /// Name used by `json_pool_type` (`tier3/parse.vl:530-539`).
    #[inline]
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Type::Null => "null",
            Type::Bool => "bool",
            Type::Number | Type::Float => "number",
            Type::String => "string",
            Type::Array => "array",
            Type::Object => "object",
            // Vela has no branch for KEY, so it falls through to "unknown".
            Type::Key => "unknown",
        }
    }
}

/// `idx_make_tag(typ, aux)` — `(aux * 256) + typ`.
#[inline]
#[must_use]
pub const fn make_tag(typ: Type, aux: u64) -> u64 {
    (aux << 8) | (typ as u64)
}

/// `tag_type(tag)` — `tag % 256`.
#[inline]
#[must_use]
pub const fn tag_type(tag: u64) -> Type {
    Type::from_code((tag & 0xFF) as u8)
}

/// `tag_aux(tag)` — `tag / 256`.
#[inline]
#[must_use]
pub const fn tag_aux(tag: u64) -> u64 {
    tag >> 8
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tag_roundtrip() {
        for &t in &[
            Type::Null,
            Type::Bool,
            Type::Number,
            Type::String,
            Type::Array,
            Type::Object,
            Type::Key,
            Type::Float,
        ] {
            for aux in [0u64, 1, 255, 256, 65535, 1 << 40] {
                let tag = make_tag(t, aux);
                assert_eq!(tag_type(tag), t);
                assert_eq!(tag_aux(tag), aux);
            }
        }
    }

    #[test]
    fn matches_vela_arithmetic() {
        // Vela: (aux * 256) + typ, decoded with % 256 and / 256.
        for aux in 0u64..300 {
            for typ in 0u64..8 {
                let vela = aux * 256 + typ;
                let ours = make_tag(Type::from_code(typ as u8), aux);
                assert_eq!(vela, ours);
                assert_eq!(vela % 256, typ);
                assert_eq!(vela / 256, aux);
            }
        }
    }
}
