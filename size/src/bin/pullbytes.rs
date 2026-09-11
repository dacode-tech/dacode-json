//! `dacode_json::pull` stopping at `Raw::bytes` — a diagnostic, not a
//! contender.
//!
//! Identical to `pull.rs` except that it never converts the field, so it
//! never reaches `parse_number` and therefore never reaches `core`'s
//! `f64::from_str`. The gap between this row and `pull` is the price of
//! correctly-rounded number parsing on a target with no double-precision
//! FPU, and it is most of the binary.

#![no_std]
#![no_main]

use size::{finish, input};

#[no_mangle]
pub extern "C" fn _start() -> ! {
    let mut total = 0i64;
    let _ = dacode_json::pull::select(input(), &[b"score"], |got| {
        total += got[0].map(|v| v.bytes().len() as i64).unwrap_or(0);
        Ok(())
    });
    finish(total)
}
