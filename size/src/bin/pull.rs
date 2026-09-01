//! `dacodec::pull`, with no allocator — the figure worth quoting.
//!
//! Sums one integer field across a root array of records. `select` names
//! the field, so the other six per record are skipped by counting
//! delimiters rather than parsed.

#![no_std]
#![no_main]

use size::{finish, input};

#[no_mangle]
pub extern "C" fn _start() -> ! {
    let mut total = 0i64;
    let _ = dacodec::pull::select(input(), &[b"score"], |got| {
        total += got[0].and_then(|v| v.as_i64()).unwrap_or(0);
        Ok(())
    });
    finish(total)
}
