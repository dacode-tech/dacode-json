//! `dacode_json::flat`, read-only, with no allocator.
//!
//! A buffer built on a host and shipped in the image — here a `const`
//! byte array standing in for `mmap` or a flash address. `View::new` is
//! the whole parse step: it validates a 32-byte header and nothing else.
//! Every accessor after that is arithmetic on a borrowed slice.
//!
//! The row exists to answer "how cheap is the *read* path", which is the
//! one an embedded device is on.

#![no_std]
#![no_main]

use size::{finish, flat_buffer, input};

#[no_mangle]
pub extern "C" fn _start() -> ! {
    // Touch `INPUT` as well, even though this row does not parse it, so
    // that the floor being subtracted is the same floor. What this row
    // does carry that the others do not is the 471-byte flat buffer
    // itself, which is data rather than code.
    let mut total = input().len() as i64;
    if let Ok(v) = dacode_json::flat::View::new(flat_buffer()) {
        for rec in v.root().elements() {
            if let Some(x) = rec.get("score").and_then(|s| s.as_i64()) {
                total += x;
            }
        }
    }
    finish(total)
}
