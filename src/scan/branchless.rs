//! S6 / S6b — branchless structural scanner.
//!
//! Port of `json_structural_scan_branchless_into`
//! (`tier2/structural_simd.vl:217-315`) and
//! `json_structural_scan_branchless2x_into` (`:360-454`), together with the
//! four primitives in `runtime/simd_json_branchless.ll`:
//!
//! | Vela symbol                    | Rust                          |
//! |--------------------------------|-------------------------------|
//! | `__simd_json_classify`         | [`classify`]                  |
//! | `__simd_json_find_escaped`     | [`find_escaped`]              |
//! | `__simd_prefix_xor_16`         | [`prefix_xor16`]              |
//! | `__simd_json_write_positions`  | `StructuralIndex::write_bits` |
//!
//! The technique is simdjson's Stage 1, narrowed to 16-byte chunks:
//!
//! 1. classify — one vector load, three bitmasks
//! 2. escape detection via the subtraction trick (no per-backslash branch)
//! 3. in-string state via prefix XOR (no per-quote toggle)
//! 4. batch position extraction (one length update per chunk)
//!
//! # Divergence from the scalar reference on invalid input
//!
//! Step 2 runs over the raw backslash mask *before* the string mask exists,
//! so a backslash **outside** a string still escapes the byte after it. The
//! scalar reference gates on `in_string` first
//! (`tier2/structural.vl:83-89`) and therefore ignores it.
//!
//! Valid JSON cannot tell the difference — a backslash may only appear
//! inside a string literal. Malformed input can:
//!
//! ```text
//! input:      \"aaaaaaaaaaaa:1"
//! scalar:     [1, 16]     both quotes are real
//! branchless: [14, 16]    opening quote read as escaped, ':' leaks out
//! ```
//!
//! This is inherited from Vela, not introduced by the port, and it means
//! tier 3's output for malformed input depends on which scanner is compiled
//! in. Vela's `t859_json_branchless.vl` asserts S6 matches the scalar
//! scanner but only ever puts backslashes inside strings, so it does not
//! catch this. Vela's *shipped* fast path calls S5
//! (`json_structural_scan_simd_into`), which does gate on `in_string`, so
//! only S6/S6b are affected. See
//! `tests/scanner_equivalence.rs::documented_divergence_backslash_outside_string`.

use super::scalar::{scan_range, TailState};
use super::StructuralIndex;

/// simdjson's `ODD_BITS`, narrowed to 16 bits. `0xAAAA`.
const ODD_BITS_16: u16 = 0xAAAA;

/// Per-chunk classification masks.
///
/// Vela packs these into one `i64` as `structural | quote << 16 | bs << 32`
/// because its FFI can only return a scalar; we return a struct.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Classified {
    /// `{ } [ ] : ,`
    pub structural: u16,
    /// `"`
    pub quote: u16,
    /// `\`
    pub backslash: u16,
}

/// `__simd_json_classify` — `simd_json_branchless.ll:21-56`.
///
/// Dispatches to NEON on aarch64, SSE2 on x86-64, and a scalar loop
/// elsewhere. All three produce identical masks.
#[inline]
#[must_use]
pub fn classify(chunk: &[u8; 16]) -> Classified {
    #[cfg(target_arch = "aarch64")]
    {
        super::neon::classify(chunk)
    }
    #[cfg(all(target_arch = "x86_64", target_feature = "sse2"))]
    {
        x86::classify(chunk)
    }
    #[cfg(not(any(target_arch = "aarch64", all(target_arch = "x86_64", target_feature = "sse2"))))]
    {
        classify_scalar(chunk)
    }
}

/// Portable reference implementation of [`classify`].
#[inline]
#[must_use]
pub fn classify_scalar(chunk: &[u8; 16]) -> Classified {
    let mut c = Classified::default();
    for (i, &b) in chunk.iter().enumerate() {
        let bit = 1u16 << i;
        match b {
            b'{' | b'}' | b'[' | b']' | b':' | b',' => c.structural |= bit,
            b'"' => c.quote |= bit,
            b'\\' => c.backslash |= bit,
            _ => {}
        }
    }
    c
}

#[cfg(all(target_arch = "x86_64", target_feature = "sse2"))]
mod x86 {
    use super::Classified;
    use core::arch::x86_64::*;

    #[inline]
    pub fn classify(chunk: &[u8; 16]) -> Classified {
        // SAFETY: SSE2 is guaranteed by the `target_feature` cfg on this
        // module, and `chunk` is exactly 16 readable bytes.
        unsafe {
            let v = _mm_loadu_si128(chunk.as_ptr().cast::<__m128i>());
            let eq = |c: u8| _mm_cmpeq_epi8(v, _mm_set1_epi8(c as i8));

            let s = _mm_or_si128(
                _mm_or_si128(_mm_or_si128(eq(b'{'), eq(b'}')), _mm_or_si128(eq(b'['), eq(b']'))),
                _mm_or_si128(eq(b':'), eq(b',')),
            );

            Classified {
                structural: _mm_movemask_epi8(s) as u16,
                quote: _mm_movemask_epi8(eq(b'"')) as u16,
                backslash: _mm_movemask_epi8(eq(b'\\')) as u16,
            }
        }
    }
}

/// `__simd_json_find_escaped` — `simd_json_branchless.ll:74-112`.
///
/// Returns `(escaped, next_carry)`. `escaped` marks bytes consumed by a
/// preceding odd-length backslash run; `next_carry` is 1 when byte 0 of the
/// next chunk is escaped.
#[inline]
#[must_use]
pub fn find_escaped(bs: u16, prev_carry: u16) -> (u16, u16) {
    let carry = prev_carry & 1;

    // If carry is set, byte 0's backslash is itself escaped and therefore
    // not an active escape.
    let potential_escape = bs & !carry;

    let maybe_escaped = potential_escape << 1;

    // Subtracting through the odd bits propagates a carry across each run of
    // consecutive backslashes; XORing the odd bits back out isolates the
    // run endpoints.
    let even_series = (maybe_escaped | ODD_BITS_16).wrapping_sub(potential_escape);
    let escape_and_terminal = even_series ^ ODD_BITS_16;

    let escaped = escape_and_terminal ^ (bs | carry);

    let escape = escape_and_terminal & bs;
    let carry_out = (escape >> 15) & 1;

    (escaped, carry_out)
}

/// `__simd_prefix_xor_16` — `simd_json_branchless.ll:120-140`.
///
/// `result[i] = input[0] ^ input[1] ^ ... ^ input[i]`, via the 4-step
/// parallel prefix simdjson uses on ARM64 (no carry-less multiply).
#[inline]
#[must_use]
pub fn prefix_xor16(mut v: u16) -> u16 {
    v ^= v << 1;
    v ^= v << 2;
    v ^= v << 4;
    v ^= v << 8;
    v
}

/// Rolling state carried between 16-byte chunks.
#[derive(Debug, Clone, Copy, Default)]
struct Carry {
    /// 0 or 1 — byte 0 of the next chunk is escaped.
    esc: u16,
    /// 0 or `0xFFFF` — simdjson's `prev_in_string`.
    str_: u16,
}

/// Process one 16-byte chunk. Returns the output mask and the updated carry.
///
/// Port of `json_branchless_chunk` (`structural_simd.vl:326-355`), minus the
/// bit-packing Vela needs to return three values through one `i64`.
#[inline]
fn chunk(input: &[u8], offset: usize, carry: Carry) -> (u16, Carry) {
    let Some(bytes) = input.get(offset..offset + 16) else {
        return (0, carry);
    };
    let Ok(arr) = <&[u8; 16]>::try_from(bytes) else {
        return (0, carry);
    };

    // Step 1: classify.
    let c = classify(arr);

    // Step 2: escape detection.
    let (escaped, esc_carry) = find_escaped(c.backslash, carry.esc);

    // Step 3: real (unescaped) quotes.
    let real_quotes = c.quote & !escaped;

    // Step 4: in-string state via prefix XOR.
    let in_string = prefix_xor16(real_quotes) ^ carry.str_;

    // simdjson broadcasts bit 15 by arithmetic shift; Vela spells it out.
    let str_carry = if (in_string >> 15) & 1 != 0 { 0xFFFF } else { 0 };

    // Step 5: quotes are always emitted; other structurals only outside
    // strings.
    let output = real_quotes | (c.structural & !in_string);

    (
        output,
        Carry {
            esc: esc_carry,
            str_: str_carry,
        },
    )
}

impl Carry {
    /// Convert the bitmask carries into the boolean state the scalar tail
    /// loop expects (`structural_simd.vl:266-274`).
    #[inline]
    fn to_tail(self) -> TailState {
        TailState {
            in_string: self.str_ != 0,
            escaped: self.esc != 0,
        }
    }
}

/// S6 — `json_structural_scan_branchless_into`, 16 bytes per iteration.
#[inline]
pub fn scan_into(input: &[u8], out: &mut StructuralIndex) {
    out.reserve_for(input.len());

    let len = input.len();
    let mut offset = 0usize;
    let mut carry = Carry::default();

    while offset + 16 <= len {
        let (mask, next) = chunk(input, offset, carry);
        carry = next;
        out.write_bits(offset, u32::from(mask));
        offset += 16;
    }

    scan_range(input, offset, carry.to_tail(), out);
}

/// S6b — `json_structural_scan_branchless2x_into`, 32 bytes per iteration.
///
/// Two chunks are combined into one 32-bit mask so the extraction loop is
/// entered half as often; the carry from chunk A feeds chunk B, so the two
/// classifications pipeline but the escape/string state stays sequential.
#[inline]
pub fn scan_2x_into(input: &[u8], out: &mut StructuralIndex) {
    out.reserve_for(input.len());

    let len = input.len();
    let mut offset = 0usize;
    let mut carry = Carry::default();

    while offset + 32 <= len {
        let (mask_a, carry_a) = chunk(input, offset, carry);
        let (mask_b, carry_b) = chunk(input, offset + 16, carry_a);
        carry = carry_b;

        let combined = u32::from(mask_a) | (u32::from(mask_b) << 16);
        out.write_bits(offset, combined);
        offset += 32;
    }

    if offset + 16 <= len {
        let (mask, next) = chunk(input, offset, carry);
        carry = next;
        out.write_bits(offset, u32::from(mask));
        offset += 16;
    }

    scan_range(input, offset, carry.to_tail(), out);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_agrees_with_scalar() {
        let mut chunk = [0u8; 16];
        for seed in 0u32..2000 {
            let mut s = seed.wrapping_mul(2654435761);
            for b in chunk.iter_mut() {
                s = s.wrapping_mul(1103515245).wrapping_add(12345);
                // Bias towards interesting bytes.
                *b = match (s >> 16) % 10 {
                    0 => b'{',
                    1 => b'}',
                    2 => b'[',
                    3 => b']',
                    4 => b':',
                    5 => b',',
                    6 => b'"',
                    7 => b'\\',
                    _ => (s >> 8) as u8,
                };
            }
            assert_eq!(classify(&chunk), classify_scalar(&chunk), "chunk {chunk:?}");
        }
    }

    #[test]
    fn prefix_xor_matches_definition() {
        for v in [0u16, 1, 0xFFFF, 0x8001, 0x0F0F, 0xAAAA] {
            let mut expect = 0u16;
            let mut acc = 0u16;
            for i in 0..16 {
                acc ^= (v >> i) & 1;
                expect |= acc << i;
            }
            assert_eq!(prefix_xor16(v), expect, "v = {v:#06x}");
        }
    }

    #[test]
    fn escape_runs() {
        // `\"` — the backslash escapes the quote.
        let (escaped, carry) = find_escaped(0b01, 0);
        assert_eq!(escaped & 0b11, 0b10);
        assert_eq!(carry, 0);

        // `\\"` — the second backslash is escaped, the quote is not.
        let (escaped, _) = find_escaped(0b011, 0);
        assert_eq!(escaped & 0b111, 0b010);

        // `\\\"` — odd run, the quote is escaped.
        let (escaped, _) = find_escaped(0b0111, 0);
        assert_eq!(escaped & 0b1111, 0b1010);

        // Backslash in the last lane carries into the next chunk.
        let (_, carry) = find_escaped(0x8000, 0);
        assert_eq!(carry, 1);
    }
}
