//! Newline-delimited JSON: many small documents, one after another.
//!
//! `cargo run --example ndjson`
//!
//! This is the shape most log and event pipelines have, and it is where
//! borrowing matters most: the lines are already in memory, so copying
//! every field out of them is avoidable work.

use std::collections::BTreeMap;

use serde::Deserialize;

/// Borrows from the line it was parsed from. Nothing here is copied
/// unless the JSON contained an escape.
#[derive(Debug, Deserialize)]
struct Event<'a> {
    ts: u64,
    level: &'a str,
    #[serde(borrow)]
    msg: &'a str,
    #[serde(borrow, default)]
    tags: Vec<&'a str>,
}

/// Only two fields of many. The rest are validated and discarded without
/// being converted, which is the cheapest thing this library does.
#[derive(Debug, Deserialize)]
struct LevelOnly<'a> {
    #[serde(borrow)]
    level: &'a str,
}

const LOG: &str = r#"
{"ts":1710000000,"level":"info","msg":"listening","addr":"0.0.0.0:8080","pid":41}
{"ts":1710000001,"level":"warn","msg":"slow query","ms":812,"sql":"select 1","tags":["db"]}
{"ts":1710000002,"level":"error","msg":"upstream failed","code":502,"tags":["net","retry"]}
{"ts":1710000003,"level":"info","msg":"recovered","after_ms":31}
"#;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // --- 1. Parse each line into a borrowing struct ------------------
    println!("events:");
    for line in LOG.lines().filter(|l| !l.trim().is_empty()) {
        let ev: Event<'_> = dacodec::direct::from_slice_borrowed(line.as_bytes())?;
        println!(
            "  {} {:<5} {:<16} tags={:?}",
            ev.ts, ev.level, ev.msg, ev.tags
        );

        // The borrow is real: `msg` points into `line`, not into a copy.
        let base = line.as_ptr() as usize;
        let at = ev.msg.as_ptr() as usize;
        debug_assert!(at >= base && at < base + line.len());
    }

    // --- 2. Count levels, reading one field per line -----------------
    //
    // Every other field is checked for validity and then skipped. No
    // number is converted, no string is unescaped.
    let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
    for line in LOG.lines().filter(|l| !l.trim().is_empty()) {
        let row: LevelOnly<'_> = dacodec::direct::from_slice_borrowed(line.as_bytes())?;
        *counts.entry(row.level).or_default() += 1;
    }
    println!("\nlevels: {counts:?}");

    // --- 3. A malformed line reports where ---------------------------
    let bad = r#"{"ts":4,"level":"info","msg":}"#;
    match dacodec::direct::from_slice::<serde_json::Value>(bad.as_bytes()) {
        Err(e) => println!("\nrejected at byte {:?}: {e}", e.offset),
        Ok(_) => println!("\nunexpectedly accepted"),
    }

    // --- 4. One bad line does not poison the rest --------------------
    let mixed = [r#"{"level":"info"}"#, "{oops}", r#"{"level":"error"}"#];
    let (ok, failed): (Vec<_>, Vec<_>) = mixed
        .iter()
        .map(|l| dacodec::direct::from_slice::<serde_json::Value>(l.as_bytes()))
        .partition(Result::is_ok);
    println!("parsed {} lines, {} failed", ok.len(), failed.len());

    Ok(())
}
