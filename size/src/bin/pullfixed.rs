//! `dacodec::pull` reading a *fractional* field, without a float parser.
//!
//! `pullint` cannot read `INPUT`'s `ratio` field at all — `1.5` is not an
//! integer, and `as_int` refuses it rather than link 22 KB to find out
//! what it is. This reads the same field as thousandths with
//! `as_fixed::<i32>(3)`.
//!
//! The row to compare against is `pullint`: the question this answers is
//! what a fraction costs *over* an integer, on a target where the honest
//! alternative (`pull`, using `as_f64`) is an order of magnitude larger.

#![no_std]
#![no_main]

use size::{finish, input};

#[no_mangle]
pub extern "C" fn _start() -> ! {
    let mut total = 0i32;
    let _ = dacodec::pull::select(input(), &[b"ratio"], |got| {
        total = total.wrapping_add(got[0].and_then(|v| v.as_fixed::<i32>(3)).unwrap_or(0));
        Ok(())
    });
    finish(i64::from(total))
}
