//! The single-pass builder must produce the same pool as the indexed one.
//!
//! Port of Vela's `t848_json_onepass.vl`, which asserts onepass ≡ indexed
//! on 24 document shapes. Extended here with the generated corpora and
//! 20 000 random documents, since "byte-identical pool" is a strong and
//! cheap invariant to check.

use dacodec::{corpus, onepass, Workspace};

/// Both builders on the same input; the pools must match node for node.
#[track_caller]
fn agree(src: &[u8]) {
    let mut ws = Workspace::new();
    let indexed = ws.parse_to_pool(src);
    let single = onepass::parse_to_pool(src);

    assert_eq!(
        single.nodes(),
        indexed.nodes(),
        "pools differ on {:?}\n  onepass: {:?}\n  indexed: {:?}",
        String::from_utf8_lossy(src),
        single.nodes(),
        indexed.nodes()
    );
    assert_eq!(single.root_idx(), indexed.root_idx());
}

#[test]
fn t848_document_shapes() {
    for s in [
        "{}", "[]", "{ }", "[ ]",
        r#"{"a":1}"#,
        r#"{"a":1,"b":2}"#,
        r#"{"a":"x"}"#,
        r#"{"a":[1,2,3]}"#,
        r#"{"a":{"b":1}}"#,
        "[1,2,3]",
        r#"["a","b"]"#,
        "[[1],[2]]",
        r#"[{"a":1},{"b":2}]"#,
        "[true,false,null]",
        r#"{"a":true,"b":false,"c":null}"#,
        r#"{"nested":{"deep":{"deeper":[1,[2,[3]]]}}}"#,
        "  {\n \"a\" : [ 1 , 2 ] ,\t\"b\" : 3 }  ",
        r#"{"":0}"#,
        r#"{"k":"v with , comma"}"#,
        r#"{"esc":"a\"b"}"#,
        "[[[[[]]]]]",
        r#"[{"a":[{"b":1}]}]"#,
        "42", "true", "null", r#""top level string""#,
        "", "   ",
    ] {
        agree(s.as_bytes());
    }
}

#[test]
fn generated_corpora() {
    for (name, json) in corpus::suite(200_000, 63) {
        let mut ws = Workspace::new();
        let indexed = ws.parse_to_pool(json.as_bytes());
        let single = onepass::parse_to_pool(json.as_bytes());
        assert_eq!(
            single.len(),
            indexed.len(),
            "{name}: node counts differ ({} vs {})",
            single.len(),
            indexed.len()
        );
        assert_eq!(single.nodes(), indexed.nodes(), "{name}: pools differ");
    }
}

#[test]
fn random_valid_documents() {
    let mut rng = corpus::Rng::new(0x0_11E9A5);
    for _ in 0..20_000 {
        agree(corpus::random_value(&mut rng, 4).as_bytes());
    }
}

/// Escapes are where a byte-walking parser and an index-driven one are most
/// likely to diverge, since only one of them tracked string state in SIMD.
#[test]
fn escape_heavy_documents() {
    for s in [
        r#"["a\"b"]"#,
        r#"["a\\"]"#,
        r#"["a\\\"b"]"#,
        r#"["\u0041\n\t"]"#,
        r#"{"a\"b":"c\\d"}"#,
        r#"["{[,:]}"]"#,
        r#"["\\\\\\\\"]"#,
    ] {
        agree(s.as_bytes());
    }
    // An escaped quote at every offset, to cross the string scanner's
    // 16-byte boundary in every phase.
    for pad in 0..40usize {
        let mut s = String::from("[\"");
        for _ in 0..pad {
            s.push('x');
        }
        s.push_str("\\\"tail\"]");
        agree(s.as_bytes());
    }
}

#[test]
fn deserializes_to_the_same_values() {
    // Beyond node equality: the resulting document must read the same.
    let json = corpus::records(500, 91);
    let src = json.as_bytes();

    let mut ws = Workspace::new();
    let a = ws.parse_to_pool(src);
    let b = onepass::parse_to_pool(src);

    let da = dacodec::Doc::new(src, &a);
    let db = dacodec::Doc::new(src, &b);
    assert_eq!(da.root().len(), db.root().len());

    for (ea, eb) in da.root().elements().zip(db.root().elements()) {
        assert_eq!(
            ea.get("score").and_then(|v| v.as_i64()),
            eb.get("score").and_then(|v| v.as_i64())
        );
        assert_eq!(
            ea.get("name").and_then(|v| v.as_str()),
            eb.get("name").and_then(|v| v.as_str())
        );
    }
}

#[test]
fn no_panic_on_arbitrary_bytes() {
    let mut rng = corpus::Rng::new(0x19A55);
    let mut buf = Vec::with_capacity(300);
    for _ in 0..20_000 {
        buf.clear();
        for _ in 0..rng.below(300) {
            buf.push((rng.next_u64() & 0xFF) as u8);
        }
        let p = onepass::parse_to_pool(&buf);
        let _ = p.len();
        let doc = dacodec::Doc::new(&buf, &p);
        let _ = doc.root().to_json();
    }
}

#[test]
fn no_panic_on_truncations() {
    let doc = corpus::records(30, 5);
    for n in 0..doc.len() {
        let src = doc.as_bytes().get(..n).unwrap_or_default();
        let _ = onepass::parse_to_pool(src).len();
    }
}

#[test]
fn reuse_matches_one_shot() {
    let mut p = onepass::OnePass::with_capacity(4096);
    for i in 0..300u64 {
        let src = format!(r#"{{"i":{i},"xs":[{i},{i}],"s":"v{i}"}}"#);
        let reused: Vec<_> = p.parse(src.as_bytes()).pool().nodes().to_vec();
        let fresh = onepass::parse_to_pool(src.as_bytes());
        assert_eq!(reused, fresh.nodes(), "iteration {i}");
    }
}
