//! Stage 1 equivalence: the branchless scanners must agree with the scalar
//! reference on every input.
//!
//! Port of `bootstrap/tests/velac2/t859_json_branchless.vl`, which asserts
//! S6 and S6b match the scalar scanner on 17 hand-picked cases including
//! cross-chunk escapes, exactly-16-byte and exactly-32-byte inputs, and a
//! backslash landing on a chunk boundary. Extended here with randomised
//! inputs, because those boundaries are exactly where the carry logic
//! breaks.

use vela_json::corpus;
use vela_json::scan::{scan, Scanner};

const SCANNERS: [Scanner; 2] = [Scanner::Branchless, Scanner::Branchless2x];

#[track_caller]
fn assert_agrees(input: &[u8]) {
    let expected = scan(Scanner::Scalar, input);
    for s in SCANNERS {
        let got = scan(s, input);
        assert_eq!(
            got.positions(),
            expected.positions(),
            "{s:?} disagrees on {:?}",
            String::from_utf8_lossy(input)
        );
    }
}

#[test]
fn t859_hand_picked_cases() {
    let cases: &[&str] = &[
        "",
        "{}",
        "[]",
        r#"{"a":1}"#,
        r#"{"a":"b"}"#,
        // Escaped quote inside a string: the `"` must NOT be structural.
        r#"{"a":"x\"y"}"#,
        // Escaped backslash followed by a real closing quote.
        r#"{"a":"x\\"}"#,
        // Three backslashes: odd run, so the quote IS escaped.
        r#"{"a":"x\\\"y"}"#,
        // Four backslashes: even run, the quote closes the string.
        r#"{"a":"x\\\\"}"#,
        // Structural characters inside a string must be ignored.
        r#"{"a":"{[,:]}"}"#,
        // Exactly 16 bytes.
        r#"{"aaaaaaaaaa":1}"#,
        // Exactly 32 bytes.
        r#"{"aaaaaaaaaa":1,"bbbbbbbbb":22}"#,
        r#"[1,2,3,4,5,6,7,8,9,10,11,12,13]"#,
        r#"{"nested":{"deep":{"deeper":[1,2,3]}}}"#,
        "   \t\n\r   ",
        r#""just a string""#,
        "12345",
    ];
    for c in cases {
        assert_agrees(c.as_bytes());
    }
}

#[test]
fn escape_at_every_chunk_boundary() {
    // Slide an escaped quote across chunk boundaries at offsets 0..96 so the
    // escape carry has to cross a 16-byte and a 32-byte edge in every phase.
    for pad in 0..96usize {
        let mut s = String::from("{\"");
        for _ in 0..pad {
            s.push('a');
        }
        s.push_str(r#"\"tail":1}"#);
        assert_agrees(s.as_bytes());
    }
}

#[test]
fn backslash_runs_at_every_chunk_boundary() {
    for pad in 0..80usize {
        for run in 1..6usize {
            let mut s = String::from("[\"");
            for _ in 0..pad {
                s.push('x');
            }
            for _ in 0..run {
                s.push('\\');
            }
            // With an odd run the next quote is escaped, so append a real
            // terminator either way and let the reference decide.
            s.push_str("\\\"z\"]");
            assert_agrees(s.as_bytes());
        }
    }
}

#[test]
fn every_prefix_of_a_document() {
    // Truncating at every byte generates unterminated strings, dangling
    // escapes and half-chunks — all the states the tail loop must inherit
    // correctly from the SIMD carry.
    let doc = corpus::records(40, 11);
    for n in 0..doc.len() {
        assert_agrees(doc.as_bytes().get(..n).unwrap_or_default());
    }
}

#[test]
fn generated_corpora() {
    for (name, json) in corpus::suite(300_000, 99) {
        let expected = scan(Scanner::Scalar, json.as_bytes());
        for s in SCANNERS {
            let got = scan(s, json.as_bytes());
            assert_eq!(got.positions(), expected.positions(), "{s:?} on {name}");
        }
        assert!(!expected.is_empty(), "{name} produced no structurals");
    }
}

#[test]
fn random_valid_documents() {
    let mut rng = corpus::Rng::new(0xDEAD_BEEF);
    for _ in 0..20_000 {
        let doc = corpus::random_value(&mut rng, 5);
        assert_agrees(doc.as_bytes());
    }
}

/// The scanners are **not** equivalent on invalid input, and this pins down
/// exactly why.
///
/// Vela's scalar reference only treats `\` as an escape when it is already
/// inside a string (`tier2/structural.vl:83-89`). The branchless scanner
/// inherits simdjson's ordering: `__simd_json_find_escaped` runs over the
/// raw backslash mask *before* the string mask exists
/// (`structural_simd.vl:233-240`), so a backslash outside a string still
/// escapes the byte after it.
///
/// For valid JSON this is unobservable — a backslash can only legally occur
/// inside a string. For invalid JSON the two scanners produce different
/// indices, which means Vela's tier 3 output for malformed input depends on
/// which scanner is compiled in. Vela's own `t859_json_branchless.vl` never
/// exercises this: all three of its escape cases put the backslash inside a
/// string.
///
/// Note Vela's *shipped* fast path calls S5
/// (`json_structural_scan_simd_into`), which gates on `in_string` and so
/// agrees with the scalar reference. Only S6/S6b diverge.
#[test]
fn documented_divergence_backslash_outside_string() {
    // A leading backslash swallows the opening quote, so the branchless
    // scanner never enters string state. Must be at least 16 bytes, or the
    // SIMD loop is skipped entirely and the shared scalar tail runs instead.
    //
    // index: 0 = '\', 1 = '"', 2..=13 = 'a', 14 = ':', 15 = '1', 16 = '"'
    let input = br#"\"aaaaaaaaaaaa:1""#;
    assert_eq!(input.len(), 17);

    let scalar = scan(Scanner::Scalar, input);
    let branchless = scan(Scanner::Branchless, input);

    assert_eq!(
        scalar.positions(),
        &[1, 16],
        "scalar ignores the backslash outside a string, so both quotes are real"
    );
    assert_eq!(
        branchless.positions(),
        &[14, 16],
        "branchless treats the opening quote as escaped, so the ':' leaks out \
         and the closing quote is read as an opener"
    );
}

/// Every disagreement on random bytes must involve a backslash outside a
/// string — i.e. the divergence class above and nothing else.
#[test]
fn random_bytes_only_diverge_on_the_documented_case() {
    let mut rng = corpus::Rng::new(0x5EED_1234);
    let alphabet: &[u8] = br#"{}[]",:\ ab019"#;
    let mut buf = Vec::with_capacity(200);
    let mut diverged = 0usize;

    for _ in 0..20_000 {
        buf.clear();
        let len = rng.below(200) as usize;
        for _ in 0..len {
            buf.push(rng.pick(alphabet).copied().unwrap_or(b'a'));
        }

        let scalar = scan(Scanner::Scalar, &buf);
        for s in SCANNERS {
            let got = scan(s, &buf);
            if got.positions() != scalar.positions() {
                diverged += 1;
                assert!(
                    has_backslash_outside_string(&buf),
                    "{s:?} diverged without a backslash outside a string: {:?}",
                    String::from_utf8_lossy(&buf)
                );
            }
        }
    }

    assert!(diverged > 0, "the divergence case was never generated");
}

/// Walk the input the way the scalar scanner does and report whether any
/// backslash appears while not inside a string.
fn has_backslash_outside_string(input: &[u8]) -> bool {
    let mut in_string = false;
    let mut escaped = false;
    for &b in input {
        if escaped {
            escaped = false;
            continue;
        }
        match b {
            b'\\' if in_string => escaped = true,
            b'\\' => return true,
            b'"' => in_string = !in_string,
            _ => {}
        }
    }
    false
}

#[test]
fn all_byte_values_in_a_string() {
    // Every byte 0..=255 as string content, to be sure classify has no
    // false positives outside the eight characters it cares about.
    let mut s = Vec::from(br#"{"k":""#.as_slice());
    for b in 0u8..=255 {
        if b == b'"' || b == b'\\' {
            s.push(b'\\');
        }
        s.push(b);
    }
    s.extend_from_slice(br#""}"#);
    assert_agrees(&s);
}
