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

#[cfg(target_arch = "aarch64")]
pub mod neon;

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

    /// Batch-extract the set bits of `mask` as `base + bit` positions.
    ///
    /// Port of `__simd_json_write_positions` / `_32`
    /// (`runtime/simd_json_branchless.ll:148-236`), which loads the count
    /// once, runs a `cttz` / `mask &= mask - 1` loop, and stores the count
    /// once. Reproducing the single length update is the point: a plain
    /// `push` per bit re-checks capacity on every iteration.
    #[inline]
    pub(crate) fn write_bits(&mut self, base: usize, mask: u32) {
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
        let want = input_len + 1;
        if self.positions.capacity() < want {
            self.positions.reserve(want - self.positions.len());
        }
    }
}

/// Which Stage 1 implementation to use.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Scanner {
    /// `json_structural_scan_into` — byte-at-a-time reference implementation.
    /// Always correct; used as the oracle in tests.
    Scalar,
    /// `json_structural_scan_branchless_into` (S6) — 16 bytes per iteration.
    Branchless,
    /// `json_structural_scan_branchless2x_into` (S6b) — 32 bytes per iteration.
    #[default]
    Branchless2x,
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
        Scanner::Branchless => branchless::scan_into(input, out),
        Scanner::Branchless2x => branchless::scan_2x_into(input, out),
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
