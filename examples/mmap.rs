//! Reading a document larger than memory, without a streaming API.
//!
//! `pull` takes a `&[u8]`, which looks like it rules out documents that do
//! not fit in memory. It does not, but the reason is worth stating
//! precisely, because it is easy to overclaim.
//!
//! The requirement is that the bytes are **addressable**, not that the
//! process has *allocated* them. A mapped file is addressable; the kernel
//! faults pages in as they are touched, and because those pages are clean
//! and file-backed it can drop them again at any time without swapping. So
//! a document larger than physical memory is read by paging through it, and
//! the process itself allocates **nothing at all** — no DOM, no `String`,
//! no index.
//!
//! What this does *not* mean is a small resident set. Scanning every record
//! touches every page, so with no memory pressure the kernel simply keeps
//! them: reading a 400 MiB file here shows ~400 MiB resident. That is page
//! cache the kernel is free to reclaim, not memory the program is holding,
//! and it is the opposite of what a parse into `Value` does, which is to
//! allocate several times the file size and be unable to give any of it
//! back. Under pressure this program's footprint collapses; that one's
//! does not.
//!
//! The same shape works with no operating system at all, where the
//! distinction is much starker. A memory-mapped QSPI flash makes a
//! bare-metal target's storage addressable without it being resident
//! anywhere: firmware on a Cortex-M4 with **192 KiB of RAM** parses a
//! **403 KiB** document in place out of the flash window, copying nothing
//! and allocating nothing, because there is nowhere to copy it to.
//!
//! ```text
//! cargo run --release --example mmap -- big.json
//! /usr/bin/time -l target/release/examples/mmap big.json
//! ```

use std::env;
use std::fs::File;
use std::time::Instant;

use memmap2::Mmap;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = match env::args().nth(1) {
        Some(p) => p,
        None => {
            eprintln!("usage: mmap <file.json>");
            eprintln!("expects a top-level array of objects with an `id` and a `score`");
            return Ok(());
        }
    };

    let file = File::open(&path)?;
    let len = file.metadata()?.len();

    // SAFETY: the usual mmap caveat — the mapping is invalidated if another
    // process truncates the file underneath it. For a file this process is
    // only reading, that is the same risk as any other read.
    let data = unsafe { Mmap::map(&file)? };

    let started = Instant::now();
    let mut records = 0u64;
    let mut total = 0i64;

    // Nothing here allocates. `Raw` borrows out of the mapping, so a field
    // is a pointer and a length into a page the kernel faulted in.
    dacode_json::pull::for_each_object(&data, |fields| {
        records += 1;
        while let Some((key, value)) = fields.next()? {
            if key == b"score" {
                // `as_int` rather than `as_i64`: the latter accepts an
                // integral float and so links a float parser. See the
                // note in `pull`'s documentation.
                total += value.as_int::<i64>().unwrap_or(0);
                // Everything after this field in the record is skipped
                // wholesale rather than being parsed field by field.
                break;
            }
        }
        Ok(())
    })?;

    let elapsed = started.elapsed();
    let mib = len as f64 / (1024.0 * 1024.0);
    println!("file      {mib:.1} MiB");
    println!("records   {records}");
    println!("sum       {total}");
    println!(
        "elapsed   {:.2} s  ({:.0} MiB/s)",
        elapsed.as_secs_f64(),
        mib / elapsed.as_secs_f64()
    );
    println!();
    println!("Heap allocated by the parse: none.");
    println!("Resident set will approach the file size, because scanning");
    println!("touches every page and the kernel keeps clean file pages when");
    println!("nothing is competing for them. That is reclaimable cache, not");
    println!("memory this program holds -- which is the difference that lets");
    println!("a file larger than RAM be read at all.");
    Ok(())
}
