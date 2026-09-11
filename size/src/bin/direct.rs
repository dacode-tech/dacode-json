//! `dacode-json` through `serde`, with an allocator.
//!
//! The like-for-like against `serde_json`: the same derived
//! `Deserialize`, the same `Vec<Rec>`, the same sum. What this costs over
//! `pull` is what typed deserialization costs, and it is mostly the
//! derive's monomorphised visitor rather than the parser.

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
    let total = match dacode_json::direct::from_slice::<Vec<Rec>>(input()) {
        Ok(rows) => rows.iter().map(|r| r.score).sum(),
        Err(_) => 0,
    };
    finish(total)
}
