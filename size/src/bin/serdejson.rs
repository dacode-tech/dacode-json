//! `serde_json`, `no_std` + `alloc`.
//!
//! Identical to `direct.rs` but for the `from_slice`, so the difference
//! between the two is the JSON library and nothing else.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::vec::Vec;
use serde::Deserialize;
use size::{finish, input};

#[global_allocator]
static A: size::bump::Bump = size::bump::Bump;

#[derive(Deserialize)]
struct Rec {
    score: i64,
}

#[no_mangle]
pub extern "C" fn _start() -> ! {
    let total = match serde_json::from_slice::<Vec<Rec>>(input()) {
        Ok(rows) => rows.iter().map(|r| r.score).sum(),
        Err(_) => 0,
    };
    finish(total)
}
