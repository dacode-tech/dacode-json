//! Byte-at-a-time structural scanner — the correctness oracle.
//!
//! Direct port of `json_structural_scan_into`
//! (`tier2/structural.vl:68-110`). Also used verbatim as the `< 16 byte`
//! tail of the SIMD scanners, so it lives here as a reusable helper.

use super::{is_structural, StructuralIndex};

/// Carry state threaded from the SIMD main loop into the scalar tail.
#[derive(Debug, Clone, Copy, Default)]
pub struct TailState {
    /// Currently inside a string literal.
    pub in_string: bool,
    /// The next byte is the target of a backslash escape.
    pub escaped: bool,
}

/// `json_structural_scan_into(input, handle)`.
#[inline]
pub fn scan_into(input: &[u8], out: &mut StructuralIndex) {
    out.reserve_for(input.len());
    scan_range(input, 0, TailState::default(), out);
}

/// Scan `input[from..]` starting from the given carry state.
///
/// This is the body of the tail loops in `structural_simd.vl:164-202`,
/// `:276-314` and `:415-453` — all three are textually identical.
#[inline]
pub fn scan_range(
    input: &[u8],
    from: usize,
    mut state: TailState,
    out: &mut StructuralIndex,
) -> TailState {
    let mut i = from;
    while i < input.len() {
        // SAFETY-free: bounds already checked by the loop condition. Using
        // `get` keeps this panic-free without an unsafe block; LLVM elides
        // the second check.
        let Some(&ch) = input.get(i) else { break };

        if state.escaped {
            state.escaped = false;
            i += 1;
            continue;
        }

        if ch == b'\\' && state.in_string {
            state.escaped = true;
            i += 1;
            continue;
        }

        if ch == b'"' {
            out.push(i);
            state.in_string = !state.in_string;
            i += 1;
            continue;
        }

        if state.in_string {
            i += 1;
            continue;
        }

        if is_structural(ch) {
            out.push(i);
        }

        i += 1;
    }
    state
}
