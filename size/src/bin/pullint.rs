//! `dacode_json::pull` reading integers only, one width.
//!
//! Same job as `pull.rs`, but `as_int::<i32>()` instead of `as_i64()`.
//! The gap between the two rows is `core`'s float parser.
//!
//! This is also the "as if it were a Cargo feature" build: exactly one
//! instantiation of the digit loop exists, which is all a feature could
//! ever give. Compare with `pullint3`.

#![no_std]
#![no_main]

use size::{finish, input};

#[no_mangle]
pub extern "C" fn _start() -> ! {
    let mut total = 0i32;
    let _ = dacode_json::pull::select(input(), &[b"score"], |got| {
        total = total.wrapping_add(got[0].and_then(|v| v.as_int::<i32>()).unwrap_or(0));
        Ok(())
    });
    finish(i64::from(total))
}
