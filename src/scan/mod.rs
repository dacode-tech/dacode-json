//! Stage 1 — the structural index.
//!
//! Port of `tier2/structural.vl` (scalar) and `tier2/structural_simd.vl`
//! (S5 branchy / S6 branchless / S6b 2x-unrolled), backed by
//! `runtime/simd_json_branchless.ll`.
//!
//! The index is a list of byte positions of *structural* characters:
//! `{ } [ ] : ,` outside of strings, plus every unescaped `"` (both the
//! opener and the closer). Stage 2 ([`crate::builder`]) walks this list.
//!
//! Vela stores positions as `i32` in a raw buffer with an `i64` count at
//! offset 0, which is why every entry point guards against inputs larger
//! than `i32::MAX`. We keep the `u32` element type (it halves cache
//! pressure versus `usize`, which is the whole point) and keep the guard.

pub mod branchless;
pub mod scalar;
pub mod table;

#[cfg(target_arch = "aarch64")]
pub mod neon;

/// How many slots `write_bits` may over-write past the real popcount.
///
/// A 32-bit mask has at most 32 set bits, and the unrolled writer fills in
/// blocks of 8/8/16.
pub const SPILL: usize = 32;


/// Vela's hard limit: positions are stored as `i32`.
///
/// `structural.vl:124`, `structural_simd.vl:80`, `parse_indexed.vl:604`.
pub const MAX_INPUT_LEN: usize = i32::MAX as usize;

/// Byte positions of structural characters, in ascending order.
///
/// Equivalent to Vela's "handle": `[count: i64][pos: i32; count]`.
#[derive(Debug, Clone, Default)]
pub struct StructuralIndex {
    positions: Vec<u32>,
}

impl StructuralIndex {
    /// `json_structural_new_simd(capacity)`.
    #[inline]
    #[must_use]
    pub fn with_capacity(capacity: usize) -> Self {
        StructuralIndex {
            positions: Vec::with_capacity(capacity),
        }
    }

    /// `json_structural_reset_simd` — set count to 0, keep the buffer.
    #[inline]
    pub fn clear(&mut self) {
        self.positions.clear();
    }

    /// `json_structural_count_simd`.
    #[inline]
    #[must_use]
    pub fn len(&self) -> usize {
        self.positions.len()
    }

    #[inline]
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.positions.is_empty()
    }

    #[inline]
    #[must_use]
    pub fn capacity(&self) -> usize {
        self.positions.capacity()
    }

    /// `json_structural_get_simd(handle, idx)`.
    #[inline]
    #[must_use]
    pub fn get(&self, idx: usize) -> Option<u32> {
        self.positions.get(idx).copied()
    }

    #[inline]
    #[must_use]
    pub fn positions(&self) -> &[u32] {
        &self.positions
    }

    #[inline]
    pub(crate) fn push(&mut self, pos: usize) {
        self.positions.push(pos as u32);
    }

    /// Batch-extract set bits with an unrolled, branch-light loop.
    ///
    /// Phase profiling (`benches/scan.rs::bench_phases`) showed that
    /// position extraction is **64–82%** of Stage 1's total cost, dwarfing
    /// classification at 13–25%. Vela's
    /// `__simd_json_write_positions`
    /// (`runtime/simd_json_branchless.ll:148-192`) loops once per set bit
    /// with a loop-carried dependency on `mask` and a branch per iteration.
    ///
    /// This is simdjson's `bit_indexer::write` instead: write eight
    /// positions *unconditionally*, then advance the length by
    /// `popcount(mask)`. Slots past the popcount receive garbage — when the
    /// mask hits zero `trailing_zeros()` returns 32, so the value is
    /// `base + 32` — but they are never published, because `set_len` only
    /// counts the real ones. The payoff is that eight independent
    /// `cttz`/`blsr` pairs issue without a branch between them.
    #[inline]
    pub(crate) fn write_bits<const UNROLL: bool>(&mut self, base: usize, mask: u32) {
        if !UNROLL {
            return self.write_bits_serial(base, mask);
        }
        if mask == 0 {
            return;
        }
        let cnt = mask.count_ones() as usize;

        // Always room for the full over-write, regardless of `cnt`.
        self.positions.reserve(SPILL);
        let len = self.positions.len();

        let Some(slots) = self.positions.spare_capacity_mut().get_mut(..SPILL) else {
            debug_assert!(false, "reserve({SPILL}) did not yield {SPILL} spare slots");
            return;
        };

        let mut m = mask;
        let mut fill = |chunk: &mut [core::mem::MaybeUninit<u32>]| {
            for slot in chunk.iter_mut() {
                slot.write((base + m.trailing_zeros() as usize) as u32);
                m &= m.wrapping_sub(1);
            }
        };

        // First eight: unconditional, no branch between them. Blocks of
        // four were tried and are worse everywhere (-11% on records, -25%
        // on geo_float) — the extra branch costs more than the saved
        // stores.
        let Some((first, rest)) = slots.split_at_mut_checked(8) else {
            return;
        };
        fill(first);

        // Beyond eight is uncommon even for a 32-bit mask, so these
        // branches predict well.
        if cnt > 8 {
            if let Some((second, tail)) = rest.split_at_mut_checked(8) {
                fill(second);
                if cnt > 16 {
                    fill(tail);
                }
            }
        }

        // SAFETY: `cnt <= SPILL` because `mask` is 32 bits, and the loops
        // above initialised at least the first `cnt` slots (eight always,
        // sixteen when `cnt > 8`, all thirty-two when `cnt > 16`).
        unsafe { self.positions.set_len(len + cnt) };
    }

    /// Vela's original extraction loop: one iteration per set bit, with the
    /// loop-carried dependency and per-bit branch intact.
    ///
    /// Kept so `benches/scan.rs` can measure what the unrolled version buys.
    #[inline]
    pub(crate) fn write_bits_serial(&mut self, base: usize, mask: u32) {
        if mask == 0 {
            return;
        }
        let n = mask.count_ones() as usize;
        self.positions.reserve(n);
        let len = self.positions.len();

        // `reserve(n)` guarantees `spare_capacity_mut().len() >= n`, so the
        // slice below always exists. The `else` arm is unreachable; taking
        // it simply skips the `set_len` and leaves the vec untouched.
        let Some(slots) = self.positions.spare_capacity_mut().get_mut(..n) else {
            debug_assert!(false, "reserve({n}) did not yield {n} spare slots");
            return;
        };

        let mut m = mask;
        for slot in slots.iter_mut() {
            let bit = m.trailing_zeros() as usize;
            slot.write((base + bit) as u32);
            m &= m.wrapping_sub(1);
        }
        debug_assert_eq!(m, 0);

        // SAFETY: the loop above initialised exactly `n` elements starting at
        // index `len`, and `len + n <= capacity` by the preceding `reserve`.
        unsafe { self.positions.set_len(len + n) };
    }

    /// Reserve room for a scan of `input_len` bytes without reallocating.
    ///
    /// Worst case is one structural character per byte, matching Vela's
    /// `si_cap = input_len + 1`.
    #[inline]
    pub fn reserve_for(&mut self, input_len: usize) {
        // `+ SPILL` so the unrolled extractor never has to grow the vector
        // mid-scan just to hold its over-write.
        let want = input_len + 1 + SPILL;
        if self.positions.capacity() < want {
            self.positions.reserve(want - self.positions.len());
        }
    }
}

/// Which Stage 1 implementation to use.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Scanner {
    /// `json_structural_scan_into` — byte-at-a-time reference implementation
    /// with Vela's if-chain classifier (`tier2/structural.vl:42-51`).
    /// Always correct; used as the oracle in tests.
    Scalar,
    /// The same byte-at-a-time scan, but with the if-chain replaced by a
    /// 256-entry class table. This is "branchless character classification
    /// (lookup tables)" from `docs/stage2/P1_2_JSON_TIERS.md:91` in its
    /// simplest form.
    ScalarTable,
    /// `json_structural_scan_branchless_into` (S6) — 16 bytes per iteration,
    /// eight vector compares per chunk.
    Branchless,
    /// `json_structural_scan_branchless2x_into` (S6b) — 32 bytes per
    /// iteration, eight vector compares per chunk.
    Branchless2x,
    /// S6 with the nibble-shuffle table classifier — two `vqtbl1q_u8`
    /// lookups and one `and` per chunk instead of thirteen vector ops.
    BranchlessTable,
    /// S6b with the nibble-shuffle table classifier.
    Branchless2xTable,
    /// S6 with the hybrid classifier: table for structural, compares for
    /// quote and backslash. See [`table::classify_hybrid`].
    BranchlessHybrid,
    /// S6b with the hybrid classifier.
    #[default]
    Branchless2xHybrid,
    /// S6b + hybrid, but with Vela's serial per-bit extraction loop. Exists
    /// to measure what the unrolled extractor is worth.
    Branchless2xHybridSerial,
}

impl Scanner {
    /// Short name for benchmark labels.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Scanner::Scalar => "scalar_ifchain",
            Scanner::ScalarTable => "scalar_lut256",
            Scanner::Branchless => "s6_compare",
            Scanner::Branchless2x => "s6b_2x_compare",
            Scanner::BranchlessTable => "s6_shuffle_table",
            Scanner::Branchless2xTable => "s6b_2x_shuffle_table",
            Scanner::BranchlessHybrid => "s6_hybrid",
            Scanner::Branchless2xHybrid => "s6b_2x_hybrid",
            Scanner::Branchless2xHybridSerial => "s6b_2x_hybrid_serialextract",
        }
    }

    /// Every scanner, for exhaustive tests and benchmarks.
    pub const ALL: [Scanner; 9] = [
        Scanner::Scalar,
        Scanner::ScalarTable,
        Scanner::Branchless,
        Scanner::Branchless2x,
        Scanner::BranchlessTable,
        Scanner::Branchless2xTable,
        Scanner::BranchlessHybrid,
        Scanner::Branchless2xHybrid,
        Scanner::Branchless2xHybridSerial,
    ];
}

/// Scan `input`, appending positions to `out`.
///
/// `out` is **not** cleared; callers that reuse a buffer must call
/// [`StructuralIndex::clear`] first (as `json_pool_parse_ws` does at
/// `parse_indexed.vl:713`).
#[inline]
pub fn scan_into(scanner: Scanner, input: &[u8], out: &mut StructuralIndex) {
    match scanner {
        Scanner::Scalar => scalar::scan_into(input, out),
        Scanner::ScalarTable => scalar::scan_table_into(input, out),
        Scanner::Branchless => branchless::scan_into(input, out),
        Scanner::Branchless2x => branchless::scan_2x_into(input, out),
        Scanner::BranchlessTable => branchless::scan_table_into(input, out),
        Scanner::Branchless2xTable => branchless::scan_table_2x_into(input, out),
        Scanner::BranchlessHybrid => branchless::scan_hybrid_into(input, out),
        Scanner::Branchless2xHybrid => branchless::scan_hybrid_2x_into(input, out),
        Scanner::Branchless2xHybridSerial => branchless::scan_hybrid_2x_serial_into(input, out),
    }
}

/// Convenience: allocate an index and scan into it.
#[must_use]
#[allow(clippy::manual_clamp)]
pub fn scan(scanner: Scanner, input: &[u8]) -> StructuralIndex {
    // `min`/`max` rather than `clamp`, which panics when max < min.
    let mut idx = StructuralIndex::with_capacity(input.len().min(4096).max(64));
    scan_into(scanner, input, &mut idx);
    idx
}

/// `json_is_structural_char` — `structural.vl:42-51`.
///
/// Note this *includes* `"` (34). Every caller tests for the quote first,
/// so it never double-appends.
#[inline]
#[must_use]
pub const fn is_structural(ch: u8) -> bool {
    matches!(ch, b'[' | b']' | b'{' | b'}' | b':' | b',' | b'"')
}

/// The same predicate via [`table::CLASS_TABLE`] instead of a comparison
/// chain.
#[inline]
#[must_use]
pub fn is_structural_table(ch: u8) -> bool {
    table::is_structural_or_quote(ch)
}
