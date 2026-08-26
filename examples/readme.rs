//! Every code example from the README, compiled and run.
//!
//! `cargo run --example readme`
//!
//! A README example that does not compile is worse than no example, so
//! these are kept here rather than in the README alone, and the README
//! quotes them.

use dacodec::flat::typed::{de, TypedView, TypedWriter};
use dacodec::flat::{self, View};
use dacodec::flat_struct;
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, PartialEq, Debug)]
struct Config {
    name: String,
    port: u16,
}

#[derive(Deserialize, PartialEq, Debug)]
struct Row<'a> {
    id: u64,
    #[serde(borrow)]
    name: &'a str,
    #[serde(borrow)]
    tags: Vec<&'a str>,
}

flat_struct! {
    pub struct Record : RecordFields {
        id: u64, age: u32, active: bool, score: i64,
        name: str, city: str, tags: [str],
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // --- Quick start -------------------------------------------------
    let cfg: Config = dacodec::from_str(r#"{"name":"edge","port":8080}"#)?;
    assert_eq!(cfg.port, 8080);
    let json = dacodec::to_string(&cfg)?;
    assert_eq!(json, r#"{"name":"edge","port":8080}"#);
    // Byte-identical to serde_json.
    assert_eq!(json, serde_json::to_string(&cfg)?);
    println!("quick start          {json}");

    // --- Owned types -------------------------------------------------
    let text = r#"{"name":"edge","port":8080}"#;
    let a: Config = dacodec::from_str(text)?;
    let b: Config = dacodec::from_slice(text.as_bytes())?;
    let c: Config = dacodec::from_reader(text.as_bytes())?;
    assert_eq!(a, b);
    assert_eq!(b, c);

    let _s: String = dacodec::to_string(&cfg)?;
    let _v: Vec<u8> = dacodec::to_vec(&cfg)?;
    let mut buffer = Vec::new();
    dacodec::to_writer(&mut buffer, &cfg)?; // reuses the buffer
    assert_eq!(buffer, _v);
    println!("owned types          ok");

    // --- Borrowing types, no copying ---------------------------------
    let bytes = br#"{"id":7,"name":"alpha","tags":["x","y"]}"#;
    let mut p = dacodec::Parser::new();
    let row: Row<'_> = p.deserialize(bytes)?;
    assert_eq!(row.id, 7);
    assert_eq!(row.name, "alpha");
    assert_eq!(row.tags, ["x", "y"]);
    // `row.name` really does point into `bytes`, it is not a copy.
    let base = bytes.as_ptr() as usize;
    let at = row.name.as_ptr() as usize;
    assert!(
        at >= base && at < base + bytes.len(),
        "expected a borrow into the input"
    );
    println!("borrowed             name={:?} at input offset {}", row.name, at - base);

    // --- Parsing many documents --------------------------------------
    let lines: [&[u8]; 3] = [br#"{"status":1}"#, br#"{"status":2}"#, br#"{"status":3}"#];
    let mut p = dacodec::Parser::with_capacity(64 * 1024);
    let mut seen = Vec::new();
    for line in lines {
        let doc = p.parse(line)?;
        if let Some(v) = doc.root().get("status") {
            seen.push(v.as_i64());
        }
    }
    assert_eq!(seen, [Some(1), Some(2), Some(3)]);
    println!("reused parser        {seen:?}");

    // --- Errors carry a position -------------------------------------
    let bad = r#"{"name":"edge","port":}"#;
    match dacodec::from_str::<Config>(bad) {
        Err(e) => {
            assert_eq!(e.offset(), Some(22));
            println!("error                {e} (byte {:?})", e.offset());
        }
        Ok(_) => panic!("should not have parsed"),
    }

    // --- Zero-copy: dynamic ------------------------------------------
    let json = br#"[{"name":"alpha","n":1},{"name":"beta","n":2}]"#;
    let mut p = dacodec::Parser::new();
    let buf = flat::encode(p.parse(json)?)?;

    let view = View::new(&buf)?; // O(1), no parsing
    let name = view
        .root()
        .at(0)
        .and_then(|r| r.get("name"))
        .and_then(|v| v.as_str());
    assert_eq!(name, Some("alpha"));
    println!("flat dynamic         {name:?} from a {}-byte buffer", buf.len());

    // --- Zero-copy: typed --------------------------------------------
    let mut w = TypedWriter::<Record>::new();
    w.record()
        .u64(1)
        .u32(30)
        .bool(true)
        .i64(-5)
        .str("alpha")
        .str("london")
        .str_list(["x", "y"]);
    let buf = w.finish();

    let v = TypedView::<Record>::new(&buf)?;
    assert_eq!(v.name(0), Some("alpha"));
    assert_eq!(v.city(0), Some("london"));
    assert_eq!(v.age(0), Some(30));

    let rows: Vec<TypedRow<'_>> = de::from_all(&v)?;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].name, "alpha");
    println!("flat typed           {:?} in {} bytes", rows[0].name, buf.len());

    // Reading a field allocates nothing; see docs/MEMORY.md for the
    // measurement (2 allocations, 36 bytes, for a 10.4 MiB document).

    println!("\nall README examples ok");
    Ok(())
}

#[derive(Deserialize, Debug)]
struct TypedRow<'a> {
    #[allow(dead_code)]
    id: u64,
    #[serde(borrow)]
    name: &'a str,
}
