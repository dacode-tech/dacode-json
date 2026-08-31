//! Inspecting a document without a struct for it.
//!
//! `cargo run --example query`
//!
//! Deserializing into a type is the fast path, but it needs a type. When
//! you only want a few values out of a shape you do not control — a config
//! file, a webhook body, an API you are exploring — a `Parser` gives you a
//! borrowed view to walk instead.
//!
//! This is the one place the node pool is the right structure: it is built
//! once and queried repeatedly, which is the opposite of filling a struct.

const CONFIG: &str = r#"{
  "service": {
    "name": "edge",
    "listen": [":8080", ":8443"],
    "tls":  { "cert": "/etc/certs/edge.pem", "min_version": 1.2 }
  },
  "limits":   { "rps": 5000, "burst": 12000, "timeout_ms": 250 },
  "features": ["compression", "http3"],
  "replicas": 3,
  "debug":    false
}"#;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut p = dacodec::Parser::new();
    let doc = p.parse(CONFIG.as_bytes())?;
    let root = doc.root();

    // --- Reach into nested objects -----------------------------------
    let name = root
        .get("service")
        .and_then(|s| s.get("name"))
        .and_then(|v| v.as_str());
    println!("service.name       {name:?}");

    let min_tls = root
        .get("service")
        .and_then(|s| s.get("tls"))
        .and_then(|t| t.get("min_version"))
        .and_then(|v| v.as_f64());
    println!("tls.min_version    {min_tls:?}");

    println!("replicas           {:?}", root.get("replicas").and_then(|v| v.as_i64()));
    println!("debug              {:?}", root.get("debug").and_then(|v| v.as_bool()));

    // --- Iterate an array --------------------------------------------
    if let Some(listen) = root.get("service").and_then(|s| s.get("listen")) {
        // `as_str` yields a `Cow`: borrowed when the JSON had no escape,
        // owned only when one had to be expanded.
        let addrs: Vec<_> = listen.elements().filter_map(|v| v.as_str()).collect();
        println!("listen             {addrs:?}");
    }

    // --- Iterate an object -------------------------------------------
    if let Some(limits) = root.get("limits") {
        print!("limits             ");
        for (k, v) in limits.entries() {
            let key = String::from_utf8_lossy(k);
            print!("{key}={:?} ", v.as_i64());
        }
        println!();
    }

    // --- A missing key is None, not an error -------------------------
    println!("missing            {:?}", root.get("nope").and_then(|v| v.as_i64()));

    // --- Wrong type is also None, not a panic ------------------------
    println!("replicas as str    {:?}", root.get("replicas").and_then(|v| v.as_str()));

    // --- Reuse the parser across documents ---------------------------
    //
    // The buffers are allocated once and reset, so a steady-state parse
    // does not allocate. The borrow checker enforces that the previous
    // document is dropped before the next parse.
    let mut p = dacodec::Parser::with_capacity(4096);
    let mut total = 0i64;
    for src in [r#"{"n":1}"#, r#"{"n":2}"#, r#"{"n":39}"#] {
        let doc = p.parse(src.as_bytes())?;
        total += doc.root().get("n").and_then(|v| v.as_i64()).unwrap_or(0);
    }
    println!("\nsum over 3 docs    {total}");

    // --- Validate without keeping anything ---------------------------
    for src in [r#"{"ok":true}"#, r#"{"bad":}"#] {
        match dacodec::validate(src.as_bytes()) {
            Ok(()) => println!("valid              {src}"),
            Err(e) => println!("invalid            {src}  ({e})"),
        }
    }

    Ok(())
}
