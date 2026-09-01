//! The shim and nothing else — the floor to subtract from every other
//! binary.
//!
//! Whatever this measures (the reset vector, `input`, `finish`, and
//! whichever `compiler_builtins` routines those drag in) is the cost of
//! being a program, not the cost of a JSON library.

#![no_std]
#![no_main]

use size::{finish, input};

#[no_mangle]
pub extern "C" fn _start() -> ! {
    // Touch the input so `INPUT` and `input()` are present here too, and
    // therefore subtract out.
    finish(input().len() as i64)
}
