//! `dacode_json::write`, with no allocator.
//!
//! The other half of the embedded story: build a document into a
//! caller-supplied `&mut [u8]`. Not the same job as the readers, so its
//! number is reported separately rather than in the same column.

#![no_std]
#![no_main]

use size::{finish, input};

#[no_mangle]
pub extern "C" fn _start() -> ! {
    let mut buf = [0u8; 256];
    let n = input().len() as u64;

    let mut w = dacode_json::write::Writer::new(&mut buf);
    let out = (|| -> Result<usize, dacode_json::write::Error> {
        w.begin_object()?;
        w.key("bytes")?.u64(n)?;
        w.key("name")?.str("alpha")?;
        w.key("ratio")?.f64(1.5)?;
        w.key("ok")?.bool(true)?;
        w.end_object()?;
        Ok(w.finish()?.len())
    })();

    finish(out.unwrap_or(0) as i64)
}
