//! Storing documents in a form you can read without parsing.
//!
//! `cargo run --example zero_copy`
//!
//! Parsing is cheap once and expensive a thousand times. For data written
//! once and read repeatedly — a cache, an mmapped file, an RPC payload —
//! `dacodec::flat` stores a document in a self-contained buffer that is
//! read directly.
//!
//! Reading a 10.4 MiB document this way costs **2 allocations and 36
//! bytes**, against `serde_json`'s 1 310 661 and 82.99 MiB, because
//! nothing is reconstructed. See `docs/MEMORY.md`.

use std::time::Instant;

use dacodec::flat::typed::{de, TypedView, TypedWriter};
use dacodec::flat::{self, View};
use dacodec::flat_struct;
use serde::Deserialize;

// The schema. Field positions become compile-time constants, so keys are
// never stored in the buffer and a read is a load at a fixed offset.
flat_struct! {
    pub struct Sensor : SensorFields {
        id: u64,
        reading: i64,
        ok: bool,
        name: str,
        site: str,
        tags: [str],
    }
}

#[allow(dead_code)] // read via Debug in the printout below
#[derive(Deserialize, Debug)]
struct SensorRow<'a> {
    id: u64,
    reading: i64,
    #[serde(borrow)]
    name: &'a str,
}

const JSON: &str = r#"[
  {"id":1,"name":"north-inlet","reading":214,"ok":true},
  {"id":2,"name":"south-inlet","reading":-8,"ok":false},
  {"id":3,"name":"turbine-3",  "reading":991,"ok":true}
]"#;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // =================================================================
    // 1. Dynamic: any JSON, no schema needed
    // =================================================================
    let mut p = dacodec::Parser::new();
    let buf = flat::encode(p.parse(JSON.as_bytes())?)?;

    println!("json      {} bytes", JSON.len());
    println!("flat      {} bytes  ({:.2}x)", buf.len(), buf.len() as f64 / JSON.len() as f64);

    // Opening is O(1). No parsing happens here — the header is checked
    // and the buffer is used in place.
    let view = View::new(&buf)?;
    let second = view
        .root()
        .at(1)
        .and_then(|r| r.get("name"))
        .and_then(|v| v.as_str());
    println!("row 1 name {second:?}");

    // A corrupted buffer is an error, never a wrong answer or a crash.
    let mut broken = buf.clone();
    if let Some(b) = broken.get_mut(4) {
        *b ^= 0xFF;
    }
    println!("corrupted  {:?}", View::new(&broken).err().is_some());

    // =================================================================
    // 2. Typed: both ends know the schema
    // =================================================================
    let mut w = TypedWriter::<Sensor>::new();
    w.record().u64(1).i64(214).bool(true).str("north-inlet").str("field-a").str_list(["inlet"]);
    w.record().u64(2).i64(-8).bool(false).str("south-inlet").str("field-a").str_list([]);
    w.record().u64(3).i64(991).bool(true).str("turbine-3").str("field-b").str_list(["hot", "watch"]);
    let tbuf = w.finish();

    println!("\ntyped     {} bytes  ({:.2}x the JSON)", tbuf.len(), tbuf.len() as f64 / JSON.len() as f64);

    let v = TypedView::<Sensor>::new(&tbuf)?;
    println!("rows      {}", v.len());
    println!("name(2)   {:?}", v.name(2));
    println!("reading(2){:?}", v.reading(2));

    // Reading one field does not touch the others.
    let total: i64 = (0..v.len()).filter_map(|i| v.reading(i)).sum();
    println!("sum       {total}");

    // Straight into structs, still borrowing from the buffer.
    let rows: Vec<SensorRow<'_>> = de::from_all(&v)?;
    println!("as structs {:?}", rows.iter().map(|r| r.name).collect::<Vec<_>>());

    // The schema is checked on open, so reading with the wrong layout is
    // an error rather than silent garbage.
    println!("wrong schema {:?}", TypedView::<Sensor>::new(&buf).err().is_some());

    // =================================================================
    // 3. Why bother: re-reading
    // =================================================================
    const N: u32 = 20_000;

    let t = Instant::now();
    let mut acc = 0i64;
    for _ in 0..N {
        let v = TypedView::<Sensor>::new(&tbuf)?;
        acc += v.reading(2).unwrap_or(0);
    }
    let flat_ns = t.elapsed().as_nanos() as f64 / f64::from(N);

    let t = Instant::now();
    let mut acc2 = 0i64;
    for _ in 0..N {
        let val: serde_json::Value = serde_json::from_str(JSON)?;
        acc2 += val[2]["reading"].as_i64().unwrap_or(0);
    }
    let json_ns = t.elapsed().as_nanos() as f64 / f64::from(N);

    println!("\nread one field, {N} times (debug build unless --release):");
    println!("  flat::typed  {flat_ns:>9.1} ns/op");
    println!("  serde_json   {json_ns:>9.1} ns/op   ({:.0}x slower)", json_ns / flat_ns.max(1e-9));
    assert_eq!(acc, acc2, "the two paths must agree");

    println!("\nrun with --release for numbers worth quoting");
    Ok(())
}
