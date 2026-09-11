//! `as_int` at three widths in one program — the monomorphisation cost.
//!
//! `i16`, `u8` and `i32` from the same document. A Cargo feature could
//! not express this at all: it would force every field through one
//! width. The question this row answers is what the *choice* costs when
//! it is actually used, measured against `pullint`, which uses one.

#![no_std]
#![no_main]

use size::{finish, input};

#[no_mangle]
pub extern "C" fn _start() -> ! {
    let mut a = 0i16;
    let mut b = 0u8;
    let mut c = 0i32;
    let _ = dacode_json::pull::select(input(), &[b"score", b"id"], |got| {
        a = a.wrapping_add(got[0].and_then(|v| v.as_int::<i16>()).unwrap_or(0));
        b = b.wrapping_add(got[1].and_then(|v| v.as_int::<u8>()).unwrap_or(0));
        c = c.wrapping_add(got[0].and_then(|v| v.as_int::<i32>()).unwrap_or(0));
        Ok(())
    });
    finish(i64::from(a) + i64::from(b) + i64::from(c))
}
