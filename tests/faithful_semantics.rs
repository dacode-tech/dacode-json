//! The faithful parser's behaviour, quirks pinned down.
//!
//! Every assertion here encodes a real Vela tier 3 behaviour. If one of
//! these starts failing, the port has drifted from the original — including
//! the cases where the original is wrong.

use vela_json::{Type, Workspace};

fn ws() -> Workspace {
    Workspace::new()
}

#[test]
fn flat_object_access() {
    let mut w = ws();
    let doc = w.parse(br#"{"name":"vela","n":42,"ok":true,"nil":null}"#);
    let r = doc.root();

    assert_eq!(r.typ(), Type::Object);
    assert_eq!(r.len(), 4);
    assert_eq!(r.get("name").and_then(|v| v.as_str()).as_deref(), Some("vela"));
    assert_eq!(r.get("n").and_then(|v| v.as_i64()), Some(42));
    assert_eq!(r.get("ok").and_then(|v| v.as_bool()), Some(true));
    assert!(r.get("nil").is_some_and(|v| v.is_null()));
    assert!(r.get("missing").is_none());
}

#[test]
fn arrays_and_nesting() {
    let mut w = ws();
    let doc = w.parse(br#"{"xs":[1,2,3],"o":{"a":{"b":[true,null]}}}"#);
    let r = doc.root();

    let xs = r.get("xs").expect("xs");
    assert_eq!(xs.len(), 3);
    assert_eq!(
        xs.elements().filter_map(|v| v.as_i64()).collect::<Vec<_>>(),
        vec![1, 2, 3]
    );
    assert_eq!(xs.at(1).and_then(|v| v.as_i64()), Some(2));
    assert!(xs.at(3).is_none());

    let deep = r
        .get("o")
        .and_then(|v| v.get("a"))
        .and_then(|v| v.get("b"))
        .expect("o.a.b");
    assert_eq!(deep.len(), 2);
    assert_eq!(deep.at(0).and_then(|v| v.as_bool()), Some(true));
    assert!(deep.at(1).is_some_and(|v| v.is_null()));
}

#[test]
fn empty_and_singleton_containers() {
    let mut w = ws();
    for (src, len) in [
        (&b"[]"[..], 0),
        (b"{}", 0),
        (b"[ ]", 0),
        (b"{ }", 0),
        (b"[1]", 1),
        (b"[[]]", 1),
        (b"[{}]", 1),
        (b"[[],[]]", 2),
    ] {
        assert_eq!(w.parse(src).root().len(), len, "{:?}", String::from_utf8_lossy(src));
    }
}

#[test]
fn whitespace_is_tolerated() {
    let mut w = ws();
    let doc = w.parse(b"  {\n\t\"a\" : [ 1 , 2 ]\r\n} ");
    assert_eq!(doc.root().get("a").map(|v| v.len()), Some(2));
}

// ---------------------------------------------------------------------
// Quirks. These are Vela behaviours, deliberately reproduced.
// ---------------------------------------------------------------------

/// Numbers are `i64` only. `.`, `e`, `E`, `+`, `-` are skipped mid-number
/// while digits keep accumulating (`runtime/json_pool_write.ll:771-784`).
#[test]
fn quirk_floats_lose_their_point() {
    let mut w = ws();
    let doc = w.parse(br#"{"pi":3.14,"e":1e3,"neg":-0.5,"sci":1.5e-2}"#);
    let r = doc.root();
    assert_eq!(r.get("pi").and_then(|v| v.as_i64()), Some(314));
    assert_eq!(r.get("e").and_then(|v| v.as_i64()), Some(13));
    assert_eq!(r.get("neg").and_then(|v| v.as_i64()), Some(-5));
    assert_eq!(r.get("sci").and_then(|v| v.as_i64()), Some(152));
}

/// `read_null` in the IR is an explicit no-op that falls into `push_null`,
/// which is also the fallback for every unrecognised byte
/// (`json_pool_write.ll:732-735`).
#[test]
fn quirk_anything_unrecognised_is_null() {
    let mut w = ws();
    let doc = w.parse(br#"{"a":null,"b":nope,"c":@!?,"d":tru}"#);
    let r = doc.root();
    for k in ["a", "b", "c", "d"] {
        assert!(r.get(k).is_some_and(|v| v.is_null()), "key {k}");
    }
}

/// Strings keep their escapes; only the accessor decodes them.
#[test]
fn quirk_strings_are_raw_slices() {
    let mut w = ws();
    let doc = w.parse(br#"{"k":"a\nb\u0041"}"#);
    let v = doc.root().get("k").expect("k");

    assert_eq!(v.as_raw_str(), Some(br#"a\nb\u0041"#.as_slice()));
    // The port's accessor is correct where Vela's legacy one was lossy.
    assert_eq!(v.as_str().as_deref(), Some("a\nbA"));
}

/// Key lookup compares raw bytes, so an escaped key never matches an
/// unescaped query (`tier3/parse.vl:551-552`).
#[test]
fn quirk_escaped_keys_do_not_match() {
    let mut w = ws();
    let doc = w.parse(br#"{"a\u0062c":1}"#);
    let r = doc.root();
    assert!(r.get("abc").is_none(), "unescaped lookup must miss");
    assert!(r.get(r"a\u0062c").is_some(), "raw lookup must hit");
}

/// A closing bracket does not have to match its opener
/// (`json_pool_write.ll:329-377` takes the type from the caller).
#[test]
fn quirk_brackets_are_not_matched() {
    let mut w = ws();
    let doc = w.parse(br#"{"a":1]"#);
    // The object node gets relabelled as an array by the ']'.
    assert_eq!(doc.root().typ(), Type::Array);
}

/// Nesting past 256 is dropped silently rather than reported
/// (`__json_ctx_push` no-ops at the limit).
#[test]
fn quirk_depth_limit_truncates_silently() {
    let depth = 300;
    let mut src = String::new();
    for _ in 0..depth {
        src.push_str(r#"{"k":"#);
    }
    src.push('1');
    for _ in 0..depth {
        src.push('}');
    }

    let mut w = ws();
    let doc = w.parse(src.as_bytes());
    // No panic, no error, just a wrong answer.
    assert_eq!(doc.root().typ(), Type::Object);
}

/// Malformed input never errors — it produces a well-formed but wrong pool.
#[test]
fn quirk_no_errors_ever() {
    let inputs: &[&[u8]] = &[
        b"",
        b"{",
        b"}",
        b"[",
        b"]",
        b"{\"a\"",
        b"{\"a\":}",
        b"[,]",
        b"[1,]",
        b"{,}",
        b"\"unterminated",
        b"{\"a\":1}{\"b\":2}",
        b"nul",
        b"--",
        b"{{{{",
        b"]]]]",
    ];
    let mut w = ws();
    for src in inputs {
        let doc = w.parse(src);
        // Just has to not panic and produce something coherent.
        let _ = doc.root().to_json();
    }
}

// ---------------------------------------------------------------------
// Panic freedom
// ---------------------------------------------------------------------

#[test]
fn arbitrary_bytes_never_panic() {
    let mut rng = vela_json::corpus::Rng::new(0xF00D);
    let mut w = ws();
    let mut buf = Vec::with_capacity(512);

    for _ in 0..30_000 {
        buf.clear();
        for _ in 0..rng.below(512) {
            buf.push((rng.next_u64() & 0xFF) as u8);
        }
        let doc = w.parse(&buf);
        let root = doc.root();
        let _ = root.to_json();
        let _ = root.get("a");
        let _ = root.at(0);
        for (k, v) in root.entries() {
            let _ = k.len();
            let _ = v.as_str();
        }
        for e in root.elements() {
            let _ = e.as_i64();
        }
    }
}

#[test]
fn truncations_of_valid_documents_never_panic() {
    let mut w = ws();
    let doc = vela_json::corpus::records(30, 5);
    for n in 0..doc.len() {
        let src = doc.as_bytes().get(..n).unwrap_or_default();
        let d = w.parse(src);
        let _ = d.root().to_json();
    }
}

#[test]
fn structurally_hostile_inputs() {
    let mut w = ws();
    let hostile: Vec<Vec<u8>> = vec![
        b"[".repeat(10_000),
        b"]".repeat(10_000),
        b"{".repeat(10_000),
        b"}".repeat(10_000),
        b"\"".repeat(10_000),
        b"\\".repeat(10_000),
        b",".repeat(10_000),
        b":".repeat(10_000),
        [b"[".repeat(5_000), b"]".repeat(5_000)].concat(),
    ];
    for src in &hostile {
        let d = w.parse(src);
        let _ = d.root().len();
    }
}

// ---------------------------------------------------------------------
// The workspace contract
// ---------------------------------------------------------------------

#[test]
fn scanner_choice_does_not_change_valid_results() {
    use vela_json::Scanner;
    let src = vela_json::corpus::records(200, 21);

    let mut a = Workspace::new().with_scanner(Scanner::Scalar);
    let mut b = Workspace::new().with_scanner(Scanner::Branchless);
    let mut c = Workspace::new().with_scanner(Scanner::Branchless2x);

    let pa = a.parse_to_pool(src.as_bytes());
    let pb = b.parse_to_pool(src.as_bytes());
    let pc = c.parse_to_pool(src.as_bytes());

    assert_eq!(pa.nodes(), pb.nodes());
    assert_eq!(pa.nodes(), pc.nodes());
}

#[test]
fn repeated_parses_are_independent() {
    let mut w = Workspace::with_capacity(1024);
    for i in 0..500u64 {
        let src = format!(r#"{{"i":{i},"xs":[{i},{i}]}}"#);
        let doc = w.parse(src.as_bytes());
        assert_eq!(doc.root().get("i").and_then(|v| v.as_i64()), Some(i as i64));
        assert_eq!(doc.root().get("xs").map(|v| v.len()), Some(2));
    }
}
