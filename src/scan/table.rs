//! Table-driven character classification — the optimisation Vela specified
//! but never implemented.
//!
//! `docs/stage2/P1_2_JSON_TIERS.md:88-92` lists, for tier 3:
//!
//! > *Branchless character classification (**lookup tables**)*
//!
//! and `docs/stage2/JSON_DESIGN.md:286-291` lists simdjson's "Lookup-4"
//! algorithm as an algorithm to port. Neither shipped. What Vela actually
//! built is eight `icmp eq` against splatted constants plus five `or`
//! (`runtime/simd_json_branchless.ll:26-40`), and an if-chain in the scalar
//! path (`tier2/structural.vl:42-51`).
//!
//! This module implements both missing forms so the cost of the omission can
//! be measured:
//!
//! * [`classify_lut256`] — a 256-byte class table, replacing the scalar
//!   if-chain.
//! * [`classify_shuffle`] — a nibble-shuffle table pair evaluated with
//!   `vqtbl1q_u8`, replacing the eight vector compares.
//!
//! # The nibble-shuffle table
//!
//! simdjson's ARM64 classifier (`3py/simdjson/src/arm64.cpp:50-51`)
//! distinguishes two classes, whitespace and "op". We need three —
//! structural, quote and backslash — so its tables cannot be reused and a
//! new pair had to be derived.
//!
//! The identity is `class = LO[byte & 0xF] & HI[byte >> 4]`, with distinct
//! bits assigned per low-nibble group so that the `and` cannot produce a
//! collision between groups:
//!
//! | byte | hi | lo | class | bit |
//! |---|---|---|---|---|
//! | `[` `{` | 5, 7 | B | structural | `0x01` |
//! | `]` `}` | 5, 7 | D | structural | `0x01` |
//! | `:` | 3 | A | structural | `0x02` |
//! | `,` | 2 | C | structural | `0x04` |
//! | `"` | 2 | 2 | quote | `0x08` |
//! | `\` | 5 | C | backslash | `0x20` |
//!
//! `,` and `\` share low nibble `C`, which is why that entry carries two
//! bits and the high nibbles select between them: `HI[2]` admits `0x04` but
//! not `0x20`, `HI[5]` admits `0x20` but not `0x04`.
//!
//! The 20 reachable `(hi, lo)` combinations were checked exhaustively; the
//! twelve non-JSON bytes among them (`*`, `+`, `-`, `2`, `;`, `<`, `=`,
//! `R`, `Z`, `r`, `z`, `|`) all classify to zero. `tests/classifier.rs`
//! re-verifies all 256 bytes at test time.

use super::branchless::Classified;

/// Structural class bits: `{ } [ ] : ,`
pub const M_STRUCTURAL: u8 = 0x07;
/// Quote class bit: `"`
pub const M_QUOTE: u8 = 0x08;
/// Backslash class bit: `\`
pub const M_BACKSLASH: u8 = 0x20;

/// `LO[byte & 0xF]` — see the module docs for the derivation.
pub const LO_TABLE: [u8; 16] = [
    0x00, 0x00, 0x08, 0x00, 0x00, 0x00, 0x00, 0x00, //
    0x00, 0x00, 0x02, 0x01, 0x24, 0x01, 0x00, 0x00,
];

/// `HI[byte >> 4]` — see the module docs for the derivation.
pub const HI_TABLE: [u8; 16] = [
    0x00, 0x00, 0x0C, 0x02, 0x00, 0x21, 0x00, 0x01, //
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
];

/// Full 256-entry class table, built from the nibble tables at compile time.
///
/// This is the direct replacement for the if-chain in
/// `json_is_structural_char` (`tier2/structural.vl:42-51`), and is what the
/// scalar scanner uses when [`super::Scanner::ScalarTable`] is selected.
pub const CLASS_TABLE: [u8; 256] = build_class_table();

// Indexing is deliberate here. This is a `const fn` evaluated at compile
// time, so an out-of-bounds index is a *compile error*, not a runtime panic
// — a strictly stronger guarantee than `get` would give, and one that costs
// nothing. `b < 256`, `b & 0xF < 16` and `b >> 4 < 16` all hold by
// construction.
#[allow(clippy::indexing_slicing)]
const fn build_class_table() -> [u8; 256] {
    let mut t = [0u8; 256];
    let mut b = 0usize;
    while b < 256 {
        t[b] = LO_TABLE[b & 0xF] & HI_TABLE[b >> 4];
        b += 1;
    }
    t
}

/// Class of a single byte, via the 256-entry table.
#[inline]
#[must_use]
pub fn class_of(b: u8) -> u8 {
    // A 256-entry table indexed by a `u8` cannot go out of range, but
    // `get` keeps the crate free of slice indexing. LLVM removes the check
    // because the index is provably < 256.
    match CLASS_TABLE.get(b as usize) {
        Some(&c) => c,
        None => 0,
    }
}

/// Is this byte one of `{ } [ ] : , "`?
///
/// Semantically identical to `json_is_structural_char`
/// (`tier2/structural.vl:42-51`), which also includes the quote.
#[inline]
#[must_use]
pub fn is_structural_or_quote(b: u8) -> bool {
    class_of(b) & (M_STRUCTURAL | M_QUOTE) != 0
}

/// Portable classification of a 16-byte chunk using [`CLASS_TABLE`].
///
/// This is the scalar form of the technique: one load per byte instead of
/// eight compares per byte. On a machine with SIMD it loses to
/// [`classify_shuffle`]; it exists as a correctness oracle and as the form
/// applicable to Vela's scalar scanner.
#[inline]
#[must_use]
pub fn classify_lut256(chunk: &[u8; 16]) -> Classified {
    let mut c = Classified::default();
    for (i, &b) in chunk.iter().enumerate() {
        let cls = class_of(b);
        let bit = 1u16 << i;
        // Branchless: turn each class bit into a 0/1 and shift into place.
        c.structural |= bit * u16::from(cls & M_STRUCTURAL != 0);
        c.quote |= bit * u16::from(cls & M_QUOTE != 0);
        c.backslash |= bit * u16::from(cls & M_BACKSLASH != 0);
    }
    c
}

/// Structural-only nibble tables, for [`classify_hybrid`].
///
/// Identifies `{ } [ ] : ,` and deliberately **excludes** `"` and `\`, which
/// the hybrid classifier gets from single-byte compares instead. Verified
/// exhaustively in `tests/classifier.rs`.
pub const STRUCT_LO: [u8; 16] = [
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, //
    0x00, 0x00, 0x02, 0x01, 0x04, 0x01, 0x00, 0x00,
];

/// See [`STRUCT_LO`].
pub const STRUCT_HI: [u8; 16] = [
    0x00, 0x00, 0x04, 0x02, 0x00, 0x01, 0x00, 0x01, //
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
];

/// Table for structural, compares for quote and backslash.
///
/// # Why this exists
///
/// [`classify_shuffle`] is 18.6% faster than the compare classifier when
/// measured in isolation, and 2–8% *slower* inside the actual scanner. The
/// reason is that classification is not on Stage 1's critical path. That
/// path is:
///
/// ```text
/// quote mask -> real_quotes -> prefix_xor -> in_string -> output_mask
/// ```
///
/// `vceqq_u8` yields all-ones lanes, which feed the movemask directly. A
/// table lookup yields a *bit pattern*, so it needs a `cmtst` first — one
/// extra instruction on the critical path, paid per chunk. The structural
/// mask, by contrast, is consumed only at the very end and is entirely off
/// the path.
///
/// So: keep the two cheap compares where latency matters, and spend the
/// table where only throughput matters. Op count per chunk:
///
/// | | vector ops before movemask |
/// |---|---|
/// | compare (Vela) | 13 (8 `cmeq` + 5 `orr`) |
/// | full table | 8 (5 shuffle + 3 `cmtst`) |
/// | **hybrid** | **8** (2 `cmeq` + 5 shuffle + 1 `cmtst`) |
///
/// Same op count as the full table, but only the structural mask pays the
/// extra latency.
#[inline]
#[must_use]
pub fn classify_hybrid(chunk: &[u8; 16]) -> Classified {
    #[cfg(target_arch = "aarch64")]
    {
        neon::classify_hybrid(chunk)
    }
    #[cfg(not(target_arch = "aarch64"))]
    {
        classify_shuffle(chunk)
    }
}

/// Nibble-shuffle classification: two `vqtbl1q_u8` lookups and one `and`,
/// replacing Vela's eight `icmp eq` and five `or`.
///
/// Falls back to [`classify_lut256`] on targets without a byte-shuffle
/// instruction.
#[inline]
#[must_use]
pub fn classify_shuffle(chunk: &[u8; 16]) -> Classified {
    #[cfg(target_arch = "aarch64")]
    {
        neon::classify(chunk)
    }
    #[cfg(all(target_arch = "x86_64", target_feature = "ssse3"))]
    {
        x86::classify(chunk)
    }
    #[cfg(not(any(
        target_arch = "aarch64",
        all(target_arch = "x86_64", target_feature = "ssse3")
    )))]
    {
        classify_lut256(chunk)
    }
}

#[cfg(target_arch = "aarch64")]
mod neon {
    use super::{Classified, HI_TABLE, LO_TABLE, M_BACKSLASH, M_QUOTE, M_STRUCTURAL};
    use core::arch::aarch64::*;

    const LANE_BITS: [u8; 16] = [
        0x01, 0x02, 0x04, 0x08, 0x10, 0x20, 0x40, 0x80, //
        0x01, 0x02, 0x04, 0x08, 0x10, 0x20, 0x40, 0x80,
    ];

    /// # Safety
    /// Requires the `neon` target feature.
    #[inline]
    #[target_feature(enable = "neon")]
    unsafe fn movemask_tst(cls: uint8x16_t, mask: u8, bits: uint8x16_t) -> u16 {
        {
            // vtstq sets a lane to all-ones when (cls & mask) != 0.
            let hit = vtstq_u8(cls, vdupq_n_u8(mask));
            let masked = vandq_u8(hit, bits);
            let lo = u16::from(vaddv_u8(vget_low_u8(masked)));
            let hi = u16::from(vaddv_u8(vget_high_u8(masked)));
            lo | (hi << 8)
        }
    }

    /// # Safety
    /// Requires the `neon` target feature and 16 readable bytes at `chunk`.
    #[inline]
    #[target_feature(enable = "neon")]
    unsafe fn classify_neon(chunk: &[u8; 16]) -> Classified {
        // SAFETY: caller guarantees `neon`; all pointers are to 16-byte
        // arrays, which is what `vld1q_u8` requires.
        unsafe {
            let v = vld1q_u8(chunk.as_ptr());
            let lo_tbl = vld1q_u8(LO_TABLE.as_ptr());
            let hi_tbl = vld1q_u8(HI_TABLE.as_ptr());
            let bits = vld1q_u8(LANE_BITS.as_ptr());

            let lo_nib = vandq_u8(v, vdupq_n_u8(0x0F));
            let hi_nib = vshrq_n_u8(v, 4);

            // vqtbl1q_u8 zeroes any lane whose index is >= 16. Both nibble
            // vectors are < 16 by construction, so no lane is dropped.
            let sl = vqtbl1q_u8(lo_tbl, lo_nib);
            let sh = vqtbl1q_u8(hi_tbl, hi_nib);
            let cls = vandq_u8(sl, sh);

            Classified {
                structural: movemask_tst(cls, M_STRUCTURAL, bits),
                quote: movemask_tst(cls, M_QUOTE, bits),
                backslash: movemask_tst(cls, M_BACKSLASH, bits),
            }
        }
    }

    #[inline]
    #[must_use]
    pub fn classify(chunk: &[u8; 16]) -> Classified {
        // SAFETY: NEON is baseline on aarch64.
        unsafe { classify_neon(chunk) }
    }

    /// # Safety
    /// Requires the `neon` target feature and 16 readable bytes at `chunk`.
    #[inline]
    #[target_feature(enable = "neon")]
    unsafe fn hybrid_neon(chunk: &[u8; 16]) -> Classified {
        use super::{STRUCT_HI, STRUCT_LO};
        // SAFETY: caller guarantees `neon`; all loads are from 16-byte arrays.
        unsafe {
            let v = vld1q_u8(chunk.as_ptr());
            let bits = vld1q_u8(LANE_BITS.as_ptr());

            // Critical path: `cmeq` gives all-ones lanes, so the movemask
            // follows immediately with no `cmtst`.
            let quote = vceqq_u8(v, vdupq_n_u8(b'"'));
            let bslash = vceqq_u8(v, vdupq_n_u8(b'\\'));

            // Off the critical path: the 6-way structural OR becomes two
            // table lookups.
            let lo_nib = vandq_u8(v, vdupq_n_u8(0x0F));
            let hi_nib = vshrq_n_u8(v, 4);
            let cls = vandq_u8(
                vqtbl1q_u8(vld1q_u8(STRUCT_LO.as_ptr()), lo_nib),
                vqtbl1q_u8(vld1q_u8(STRUCT_HI.as_ptr()), hi_nib),
            );
            // Any nonzero class byte means structural; `cmtst(x, x)` is the
            // one-instruction "is nonzero" test.
            let structural = vtstq_u8(cls, cls);

            let mask = |hit: uint8x16_t| -> u16 {
                let m = vandq_u8(hit, bits);
                u16::from(vaddv_u8(vget_low_u8(m))) | (u16::from(vaddv_u8(vget_high_u8(m))) << 8)
            };

            Classified {
                structural: mask(structural),
                quote: mask(quote),
                backslash: mask(bslash),
            }
        }
    }

    #[inline]
    #[must_use]
    pub fn classify_hybrid(chunk: &[u8; 16]) -> Classified {
        // SAFETY: NEON is baseline on aarch64.
        unsafe { hybrid_neon(chunk) }
    }
}

#[cfg(all(target_arch = "x86_64", target_feature = "ssse3"))]
mod x86 {
    use super::{Classified, HI_TABLE, LO_TABLE, M_BACKSLASH, M_QUOTE, M_STRUCTURAL};
    use core::arch::x86_64::*;

    #[inline]
    pub fn classify(chunk: &[u8; 16]) -> Classified {
        // SAFETY: SSSE3 is guaranteed by the module's `target_feature` cfg
        // and `chunk` is exactly 16 readable bytes.
        unsafe {
            let v = _mm_loadu_si128(chunk.as_ptr().cast::<__m128i>());
            let lo_tbl = _mm_loadu_si128(LO_TABLE.as_ptr().cast::<__m128i>());
            let hi_tbl = _mm_loadu_si128(HI_TABLE.as_ptr().cast::<__m128i>());

            let lo_nib = _mm_and_si128(v, _mm_set1_epi8(0x0F));
            // No byte-granularity shift on SSE: shift as 16-bit then mask.
            let hi_nib = _mm_and_si128(_mm_srli_epi16(v, 4), _mm_set1_epi8(0x0F));

            let sl = _mm_shuffle_epi8(lo_tbl, lo_nib);
            let sh = _mm_shuffle_epi8(hi_tbl, hi_nib);
            let cls = _mm_and_si128(sl, sh);

            let zero = _mm_setzero_si128();
            let test = |m: u8| {
                let anded = _mm_and_si128(cls, _mm_set1_epi8(m as i8));
                // != 0  <=>  NOT (== 0)
                let eqz = _mm_cmpeq_epi8(anded, zero);
                !(_mm_movemask_epi8(eqz) as u16)
            };

            Classified {
                structural: test(M_STRUCTURAL),
                quote: test(M_QUOTE),
                backslash: test(M_BACKSLASH),
            }
        }
    }
}
