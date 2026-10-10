//! Differential coverage for the ignored-field skip in `direct`.
//!
//! `src/direct/mod.rs`'s `deserialize_ignored_any` walks a value without
//! materialising it — for every unknown struct field, and, behind
//! `raw_value`, for `RawJson` capture, where the walk is the *only*
//! validation the captured span gets: the span travels the crate-private
//! trusted channel (`raw::TrustedStr`) into `from_validated_str` without
//! a second parse (commit cf7bcbf). A skip that under-validates would
//! accept documents `serde_json` rejects whenever the fault sits in an
//! ignored field, and let unvalidated bytes into a captured `RawJson`.
//!
//! Nothing else pins that agreement:
//!
//! * `src/fuzz.rs` compares `direct::from_slice::<serde_json::Value>` —
//!   the *value* path — against serde_json; `IgnoredAny` never appears.
//! * `tests/stream.rs::malformed_skipped_values_are_still_rejected` is
//!   eleven hand-picked cases against serde_json.
//! * `tests/edge_cases.rs` pins the number-range dimension only.
//!
//! So this file applies the cf7bcbf audit methodology to the skip: the
//! JSONTestSuite corpus, mutations of it, and an adversarial list, each
//! checked for accept/reject agreement with `crate::validate` — the
//! grammar the trust contract measures the walk against.
//!
//! One divergence is expected and documented, not a finding: the walk's
//! `MAX_DEPTH = 128` is deliberately stricter than `validate`'s 256
//! (pinned by `src/raw.rs`'s depth tests), so a value nested 129–256 deep
//! is accepted by `validate` and rejected by the walk. The direction the
//! contract depends on — everything the walk accepts, `validate` accepts
//! — is asserted unconditionally.

use dacode_json::corpus::Rng;
use serde::Deserialize;
use std::fs;
use std::path::{Path, PathBuf};

/// One known field; every other field is walked, not converted — the
/// `PrefixOnly` shape from `benches/rawjson.rs`.
#[derive(Deserialize)]
struct PrefixOnly {
    #[allow(dead_code)]
    id: u64,
}

/// Wrap `value` as an unknown field's value. The envelope is fixed and
/// valid, so the wire's verdict is the skip's verdict on `value`.
fn wire(value: &[u8]) -> Vec<u8> {
    let mut w = Vec::with_capacity(value.len() + 24);
    w.extend_from_slice(br#"{"id":1,"junk":"#);
    w.extend_from_slice(value);
    w.push(b'}');
    w
}

/// Surrounding JSON whitespace off both ends — the bytes a capture's
/// span keeps, given a buffer `validate` accepted as one value plus
/// optional padding. Exact, because no JSON value begins or ends with
/// a whitespace byte.
fn trim_json_ws(value: &[u8]) -> &[u8] {
    let is_ws = |b: &u8| matches!(b, b' ' | b'\t' | b'\n' | b'\r');
    let mut end = value.len();
    while end > 0 && is_ws(&value[end - 1]) {
        end -= 1;
    }
    let mut start = 0;
    while start < end && is_ws(&value[start]) {
        start += 1;
    }
    &value[start..end]
}

/// Container levels in a *valid* document; the innermost scalar does not
/// count, so `[[1]]` is 2. String-aware, so brackets inside strings do
/// not nest. Only called on input `validate` accepted.
fn nesting(value: &[u8]) -> usize {
    let (mut depth, mut max) = (0usize, 0usize);
    let mut i = 0;
    while let Some(&b) = value.get(i) {
        match b {
            b'"' => {
                i += 1;
                while let Some(&c) = value.get(i) {
                    i += 1;
                    if c == b'\\' {
                        i += 1;
                    } else if c == b'"' {
                        break;
                    }
                }
            }
            b'{' | b'[' => {
                depth += 1;
                max = max.max(depth);
                i += 1;
            }
            b'}' | b']' => {
                depth = depth.saturating_sub(1);
                i += 1;
            }
            _ => i += 1,
        }
    }
    max
}

/// The skip's verdict on `value` must agree with `validate`'s.
///
/// `validate` rejecting always means the skip rejects — the trust
/// direction, since a captured span is never re-validated. `validate`
/// accepting means the skip accepts, unless the value is nested past the
/// walk's `MAX_DEPTH = 128` (the one documented divergence).
#[track_caller]
fn agrees_with_validate(value: &[u8]) {
    let valid = dacode_json::validate(value).is_ok();
    let skipped = dacode_json::from_slice::<PrefixOnly>(&wire(value)).is_ok();
    let shown = String::from_utf8_lossy(value.get(..80).unwrap_or(value));
    if !valid {
        assert!(
            !skipped,
            "the skip accepted a value `validate` rejects: {shown:?}"
        );
        return;
    }
    let n = nesting(value);
    if n <= 128 {
        assert!(
            skipped,
            "the skip rejected a valid value (nesting {n}): {shown:?}"
        );
    } else {
        assert!(
            !skipped,
            "the skip accepted nesting {n}, past MAX_DEPTH = 128: {shown:?}"
        );
    }
}

fn testdata(sub: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("testdata")
        .join(sub)
}

/// Every file in a directory, sorted, as `(name, bytes)` — the same
/// loader the conformance suites use.
fn load_dir(sub: &str) -> Vec<(String, Vec<u8>)> {
    let dir = testdata(sub);
    let mut out: Vec<(String, Vec<u8>)> = fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("read {}: {e}", dir.display()))
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().is_some_and(|x| x == "json"))
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            fs::read(e.path()).ok().map(|b| (name, b))
        })
        .collect();
    out.sort_by(|a, b| a.0.cmp(&b.0));
    assert!(!out.is_empty(), "no .json files in {}", dir.display());
    out
}

/// The seed corpus: every testdata file small enough to mutate.
fn seeds() -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    for sub in ["test_parsing", "test_checker", "test_transform"] {
        for (_, bytes) in load_dir(sub) {
            if bytes.len() <= (1 << 16) {
                out.push(bytes);
            }
        }
    }
    out
}

/// Mutate `seed` into `out`, deterministic given `rng`.
///
/// A compact relative of `src/fuzz.rs::mutate`, which this test cannot
/// reuse: the `fuzz` module rides the `fuzzing` feature, and this
/// differential must run in the default `cargo test` suite.
fn mutate(rng: &mut Rng, seed: &[u8], out: &mut Vec<u8>) {
    const TOKENS: &[&[u8]] = &[
        b"\"",
        b"\\",
        b"\\u",
        b"\\ud800",
        b"\\u0000",
        b"{",
        b"}",
        b"[",
        b"]",
        b":",
        b",",
        b"0",
        b"-0",
        b"1e999",
        b"true",
        b"null",
        b"\x00",
        b"\xff",
        b"\xc3\xa9",
    ];
    out.clear();
    out.extend_from_slice(seed);
    if out.is_empty() {
        out.extend_from_slice(b"{}");
    }
    for _ in 0..1 + rng.below(4) {
        let len = out.len();
        match rng.below(6) {
            // delete a byte
            0 if len > 1 => {
                out.remove(rng.below(len as u64) as usize);
            }
            // duplicate a byte
            1 => {
                let i = rng.below(len as u64) as usize;
                let b = out.get(i).copied().unwrap_or(b'x');
                out.insert(i, b);
            }
            // flip a bit
            2 => {
                let i = rng.below(len as u64) as usize;
                if let Some(slot) = out.get_mut(i) {
                    *slot ^= 1 << rng.below(8);
                }
            }
            // splice in a token a parser has special cases for
            3 => {
                let tok = rng.pick(TOKENS).copied().unwrap_or(b"null");
                let i = rng.below(out.len() as u64 + 1) as usize;
                let tail = out.split_off(i.min(out.len()));
                out.extend_from_slice(tok);
                out.extend_from_slice(&tail);
            }
            // truncate
            4 => {
                out.truncate(rng.below(len as u64 + 1) as usize);
            }
            // duplicate a span, to build deep nesting cheaply
            _ => {
                let a = rng.below(len as u64) as usize;
                let b = (a + 1 + rng.below(32) as usize).min(len);
                if let Some(span) = out.get(a..b).map(<[u8]>::to_vec) {
                    let at = rng.below(out.len() as u64 + 1) as usize;
                    let tail = out.split_off(at.min(out.len()));
                    out.extend_from_slice(&span);
                    out.extend_from_slice(&tail);
                }
            }
        }
        // Keep inputs bounded; the depth boundary is covered explicitly.
        out.truncate(1 << 16);
    }
}

/// The whole JSONTestSuite corpus, each file as one ignored value.
#[test]
fn corpus_files_agree_with_validate() {
    let mut n = 0usize;
    for bytes in seeds() {
        agrees_with_validate(&bytes);
        n += 1;
    }
    assert!(n > 300, "only {n} corpus files were checked");
}

/// Mutations of the corpus: near-valid documents, which is where an
/// under-validating skip shows up — random bytes only ever exercise the
/// first rejection branch.
#[test]
fn mutated_corpus_agrees_with_validate() {
    let corpus = seeds();
    let mut rng = Rng::new(0xD1FFE);
    let mut buf = Vec::new();
    const N: usize = 20_000;
    let mut valid = 0usize;
    for _ in 0..N {
        let pick = rng.below(corpus.len() as u64) as usize;
        let seed = corpus.get(pick).map(Vec::as_slice).unwrap_or(b"{}");
        mutate(&mut rng, seed, &mut buf);
        if dacode_json::validate(&buf).is_ok() {
            valid += 1;
        }
        agrees_with_validate(&buf);
    }
    // A corpus that is never valid would exercise only the reject path.
    assert!(
        valid > N / 100,
        "only {valid}/{N} mutations were valid JSON; the mutator is too destructive"
    );
}

/// Hand-picked shapes the generator cannot be trusted to reach: the
/// cf7bcbf audit's adversarial set, plus the depth boundary on both
/// sides of the walk's limit.
#[test]
fn adversarial_cases_agree_with_validate() {
    let cases: &[&str] = &[
        // bad escapes
        r#""\q""#,
        r#""\ ""#,
        r#""\u12""#,
        r#""\ud800""#,
        r#""\udfff""#,
        r#""\ud800\ud800""#,
        // raw control bytes
        "\"a\u{01}b\"",
        "\"a\u{7f}b\"",
        "\"a\tb\"",
        // bad numbers
        "01",
        "1.",
        "1e",
        "-",
        "+1",
        ".5",
        "0x10",
        "1e400",
        "-1e999",
        "Infinity",
        "NaN",
        // unmatched / trailing brackets
        "[1,2",
        "{\"a\":1",
        "[1,]",
        "{\"a\":1,}",
        "[}]",
        "{]",
        "[[[",
        "}}",
        // missing ':' and framing
        "{\"a\" 1}",
        "{\"a\":1 \"b\":2}",
        "[1 2]",
        // literals
        "tru",
        "nul",
        "fals",
        // strings
        "\"unterminated",
        "\"abc\\",
        // empty / whitespace
        "",
        " ",
        "\t",
        // valid shapes, for the accept direction
        "{}",
        "[]",
        "[1,2,3]",
        "{\"a\":{\"b\":[true,null,\"x\"]}}",
        "{ \"a\" : [ 1 , 2 ] }",
        "\"\"",
        "42",
        "-0",
        "1e-7",
        "\"\\ud83d\\ude00\"",
        "\"\\u0000\"",
        "\"é \\n é\"",
    ];
    for case in cases {
        agrees_with_validate(case.as_bytes());
    }

    // The depth boundary: 128 internal levels are walked, 129 are past
    // the walk's limit while `validate` still accepts (to 256; 257 is
    // past validate's own limit).
    for depth in [0usize, 1, 63, 127, 128, 129, 200, 256, 257] {
        let value = format!("{}1{}", "[".repeat(depth), "]".repeat(depth));
        agrees_with_validate(value.as_bytes());
        let value = format!("{}\"x\"{}", "{".repeat(depth), "}".repeat(depth));
        agrees_with_validate(value.as_bytes());
    }
}

/// The same differential through the capture path, which shares the walk
/// with the ignored-field skip: a span the walk accepts must be one
/// `validate` accepts — the cf7bcbf trust contract, asserted directly
/// rather than only through `from_validated_str`'s debug assert.
#[cfg(feature = "raw_value")]
#[test]
fn captured_spans_agree_with_validate() {
    use dacode_json::RawJson;

    #[derive(Deserialize)]
    struct Captured {
        #[allow(dead_code)]
        id: u64,
        #[allow(dead_code)]
        args: RawJson,
    }

    let cases: &[&str] = &[
        "{}",
        "[1,2,3]",
        r#"{"a":{"b":[true,null,"x"]}}"#,
        "{ \"a\" : [ 1 , 2 ] }",
        "42",
        "-0",
        "1e-7",
        r#""é \n é""#,
        r#""\ud83d\ude00""#,
        // the audit's dimensions: bad escape, bad number, bad structure
        r#""\q""#,
        "1e400",
        "[1,]",
        "{\"a\" 1}",
    ];
    for case in cases {
        let valid = dacode_json::validate(case.as_bytes()).is_ok();
        let mut w = Vec::with_capacity(case.len() + 24);
        w.extend_from_slice(br#"{"id":1,"args":"#);
        w.extend_from_slice(case.as_bytes());
        w.push(b'}');
        let got = dacode_json::from_slice::<Captured>(&w);
        assert_eq!(
            got.is_ok(),
            valid,
            "capture verdict disagrees with validate on {case:?}"
        );
        if let Ok(e) = got {
            assert_eq!(
                e.args.get(),
                *case,
                "capture was not byte-exact on {case:?}"
            );
        }
    }

    // The mutation corpus rides the capture route too: byte-exactness
    // under mutation was pinned only by the hand-picked cases above.
    // The expected verdict is `agrees_with_validate`'s predicate —
    // `validate`'s verdict plus the documented nesting divergence —
    // because the capture envelope has the same depth shape as `wire`'s.
    // The expected span is the buffer with surrounding JSON whitespace
    // trimmed: the capture records `start` after `skip_ws` and `end` at
    // the value's last byte, and no value can begin or end with a
    // whitespace byte, so the trim is exact.
    let corpus = seeds();
    let mut rng = Rng::new(0xD1FFE);
    let mut buf = Vec::new();
    let mut w = Vec::new();
    for _ in 0..20_000 {
        let pick = rng.below(corpus.len() as u64) as usize;
        let seed = corpus.get(pick).map(Vec::as_slice).unwrap_or(b"{}");
        mutate(&mut rng, seed, &mut buf);
        let expect = dacode_json::validate(&buf).is_ok() && nesting(&buf) <= 128;
        w.clear();
        w.extend_from_slice(br#"{"id":1,"args":"#);
        w.extend_from_slice(&buf);
        w.push(b'}');
        let got = dacode_json::from_slice::<Captured>(&w);
        assert_eq!(
            got.is_ok(),
            expect,
            "capture verdict disagrees on mutated {:?}",
            String::from_utf8_lossy(&buf[..buf.len().min(80)])
        );
        if let Ok(e) = got {
            let span = e.args.get().as_bytes();
            let value = trim_json_ws(&buf);
            assert_eq!(span, value, "capture was not byte-exact under mutation");
        }
    }

    // Capture rides one level deeper than a bare document (the envelope
    // object), so 128 internal levels capture and 129 do not.
    for depth in [128usize, 129] {
        let span = format!("{}1{}", "[".repeat(depth), "]".repeat(depth));
        let w = format!(r#"{{"id":1,"args":{span}}}"#);
        let got = dacode_json::from_slice::<Captured>(w.as_bytes());
        assert_eq!(
            got.is_ok(),
            depth <= 128,
            "capture at depth {depth}: {:?}",
            got.map(|_| ()).map_err(|e| e.to_string())
        );
    }
}
