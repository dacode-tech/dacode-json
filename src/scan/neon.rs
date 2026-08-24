//! AArch64 NEON implementation of [`crate::scan::branchless::classify`].
//!
//! `simd_json_branchless.ll:21-56` writes this as `<16 x i8>` compares
//! followed by `bitcast <16 x i1> to i16`. LLVM lowers that bitcast on ARM64
//! to a shift-and-narrow sequence; here we spell out the equivalent
//! `and` + pairwise-add movemask, which is the standard NEON idiom (ARM has
//! no `pmovmskb`).
//!
//! NEON is architecturally mandatory on aarch64, so no runtime feature
//! detection is needed and the safe wrapper below cannot be wrong.

use super::branchless::Classified;
use core::arch::aarch64::*;

/// Per-lane bit weights: `1, 2, 4, ... 128` repeated for each half.
const LANE_BITS: [u8; 16] = [
    0x01, 0x02, 0x04, 0x08, 0x10, 0x20, 0x40, 0x80, 0x01, 0x02, 0x04, 0x08, 0x10, 0x20, 0x40, 0x80,
];

/// Collapse a vector of per-lane `0x00`/`0xFF` into a 16-bit mask.
///
/// # Safety
/// Requires the `neon` target feature.
#[inline]
#[target_feature(enable = "neon")]
unsafe fn movemask(v: uint8x16_t, bits: uint8x16_t) -> u16 {
    {
        let masked = vandq_u8(v, bits);
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
    // SAFETY: the caller guarantees the `neon` target feature and that
    // `chunk` points at 16 readable bytes.
    unsafe {
        let v = vld1q_u8(chunk.as_ptr());
        let bits = vld1q_u8(LANE_BITS.as_ptr());

        let lbrace = vceqq_u8(v, vdupq_n_u8(b'{'));
        let rbrace = vceqq_u8(v, vdupq_n_u8(b'}'));
        let lbrack = vceqq_u8(v, vdupq_n_u8(b'['));
        let rbrack = vceqq_u8(v, vdupq_n_u8(b']'));
        let colon = vceqq_u8(v, vdupq_n_u8(b':'));
        let comma = vceqq_u8(v, vdupq_n_u8(b','));
        let quote = vceqq_u8(v, vdupq_n_u8(b'"'));
        let bslash = vceqq_u8(v, vdupq_n_u8(b'\\'));

        // OR the six structural comparisons in the vector domain so we pay for
        // three movemasks instead of eight.
        let s = vorrq_u8(
            vorrq_u8(vorrq_u8(lbrace, rbrace), vorrq_u8(lbrack, rbrack)),
            vorrq_u8(colon, comma),
        );

        Classified {
            structural: movemask(s, bits),
            quote: movemask(quote, bits),
            backslash: movemask(bslash, bits),
        }
    }
}

/// `__simd_json_classify` on NEON.
#[inline]
#[must_use]
pub fn classify(chunk: &[u8; 16]) -> Classified {
    // SAFETY: NEON is baseline on aarch64 — the target feature is always
    // present. `chunk` is exactly 16 readable, 1-byte-aligned bytes, which
    // is all `vld1q_u8` requires.
    unsafe { classify_neon(chunk) }
}
