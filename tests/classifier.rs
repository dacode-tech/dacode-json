//! Correctness of the table-driven classifiers.
//!
//! The nibble-shuffle table was derived by hand (see `src/scan/table.rs`),
//! so it is verified exhaustively rather than by sampling: all 256 byte
//! values, and all six scanners against the scalar oracle.

use dacode_json::corpus;
use dacode_json::scan::branchless::{classify, classify_scalar, Classified};
use dacode_json::scan::table::{
    class_of, classify_hybrid, classify_lut256, classify_shuffle, CLASS_TABLE, HI_TABLE, LO_TABLE,
    M_BACKSLASH, M_QUOTE, M_STRUCTURAL, STRUCT_HI, STRUCT_LO,
};
use dacode_json::scan::{scan, Scanner};

/// The eight bytes JSON Stage 1 cares about, and nothing else.
fn expected_class(b: u8) -> u8 {
    match b {
        b'{' | b'}' | b'[' | b']' | b':' | b',' => M_STRUCTURAL,
        b'"' => M_QUOTE,
        b'\\' => M_BACKSLASH,
        _ => 0,
    }
}

#[test]
fn class_table_is_exact_for_all_256_bytes() {
    for b in 0u8..=255 {
        let got = class_of(b);
        let want = expected_class(b);

        // The table encodes structural as one of three bits (0x01/0x02/0x04)
        // so that low-nibble groups cannot collide; compare by class, not by
        // raw value.
        let got_class = (
            got & M_STRUCTURAL != 0,
            got & M_QUOTE != 0,
            got & M_BACKSLASH != 0,
        );
        let want_class = (
            want & M_STRUCTURAL != 0,
            want & M_QUOTE != 0,
            want & M_BACKSLASH != 0,
        );
        assert_eq!(got_class, want_class, "byte {b:#04x} ({:?})", b as char);
    }
}

#[test]
fn nibble_tables_reproduce_the_256_entry_table() {
    for b in 0usize..256 {
        let lo = LO_TABLE.get(b & 0xF).copied().unwrap_or(0);
        let hi = HI_TABLE.get(b >> 4).copied().unwrap_or(0);
        assert_eq!(
            CLASS_TABLE.get(b).copied().unwrap_or(0xFF),
            lo & hi,
            "byte {b:#04x}"
        );
    }
}

/// The twelve non-JSON bytes that share a nibble with a JSON byte. These are
/// the ones a badly derived table would misclassify, so they get their own
/// test.
#[test]
fn near_miss_bytes_classify_to_zero() {
    for b in [
        b'*', b'+', b'-', // hi=2, lo in {A,B,D}
        b'2', b';', b'<', b'=', // hi=3
        b'R', b'Z', // hi=5
        b'r', b'z', b'|', // hi=7
    ] {
        assert_eq!(
            class_of(b),
            0,
            "{:?} ({b:#04x}) must not classify",
            b as char
        );
    }
}

#[test]
fn all_classifiers_agree_on_every_chunk_of_interest() {
    // Every byte in every lane position, so a lane-indexing bug in the
    // movemask cannot hide.
    for lane in 0..16usize {
        for b in 0u8..=255 {
            let mut chunk = [b'a'; 16];
            if let Some(slot) = chunk.get_mut(lane) {
                *slot = b;
            }
            let reference = classify_scalar(&chunk);
            assert_eq!(
                classify(&chunk),
                reference,
                "compare, lane {lane}, byte {b:#04x}"
            );
            assert_eq!(
                classify_lut256(&chunk),
                reference,
                "lut256, lane {lane}, byte {b:#04x}"
            );
            assert_eq!(
                classify_shuffle(&chunk),
                reference,
                "shuffle, lane {lane}, byte {b:#04x}"
            );
            assert_eq!(
                classify_hybrid(&chunk),
                reference,
                "hybrid, lane {lane}, byte {b:#04x}"
            );
        }
    }
}

#[test]
fn all_classifiers_agree_on_random_chunks() {
    let mut rng = corpus::Rng::new(0x7AB1E);
    for _ in 0..20_000 {
        let mut chunk = [0u8; 16];
        for slot in chunk.iter_mut() {
            *slot = (rng.next_u64() & 0xFF) as u8;
        }
        let reference = classify_scalar(&chunk);
        assert_eq!(classify(&chunk), reference, "compare on {chunk:?}");
        assert_eq!(classify_lut256(&chunk), reference, "lut256 on {chunk:?}");
        assert_eq!(classify_shuffle(&chunk), reference, "shuffle on {chunk:?}");
        assert_eq!(classify_hybrid(&chunk), reference, "hybrid on {chunk:?}");
    }
}

#[test]
fn all_zero_and_all_set_chunks() {
    for fill in [0x00u8, 0xFF, 0x80, 0x7F] {
        let chunk = [fill; 16];
        let reference = classify_scalar(&chunk);
        assert_eq!(classify_shuffle(&chunk), reference, "fill {fill:#04x}");
        assert_eq!(classify_hybrid(&chunk), reference, "fill {fill:#04x}");
        assert_eq!(classify_lut256(&chunk), reference, "fill {fill:#04x}");
    }

    // A chunk made entirely of JSON structural bytes.
    let chunk = *b"{}[]:,\"\\{}[]:,\"\\";
    let reference = classify_scalar(&chunk);
    assert_eq!(
        reference,
        Classified {
            structural: 0b0011_1111_0011_1111 & 0x3F3F,
            quote: 0b0100_0000_0100_0000,
            backslash: 0b1000_0000_1000_0000,
        }
    );
    assert_eq!(classify_shuffle(&chunk), reference);
    assert_eq!(classify_lut256(&chunk), reference);
    assert_eq!(classify_hybrid(&chunk), reference);
}

/// The hybrid classifier uses a structural-only table that must exclude the
/// quote and the backslash, since those come from compares.
#[test]
fn structural_only_table_is_exact() {
    let want = |b: u8| matches!(b, b'{' | b'}' | b'[' | b']' | b':' | b',');
    for b in 0u8..=255 {
        let lo = STRUCT_LO.get((b & 0xF) as usize).copied().unwrap_or(0);
        let hi = STRUCT_HI.get((b >> 4) as usize).copied().unwrap_or(0);
        assert_eq!((lo & hi) != 0, want(b), "byte {b:#04x} ({:?})", b as char);
    }
    // The two bytes that must NOT be in the structural table.
    for b in *b"\"\\" {
        let lo = STRUCT_LO.get((b & 0xF) as usize).copied().unwrap_or(0);
        let hi = STRUCT_HI.get((b >> 4) as usize).copied().unwrap_or(0);
        assert_eq!(
            lo & hi,
            0,
            "{:?} must not be in the structural table",
            b as char
        );
    }
}

// ---------------------------------------------------------------------
// End-to-end: every scanner must produce the same index
// ---------------------------------------------------------------------

#[track_caller]
fn all_scanners_agree(input: &[u8]) {
    let expected = scan(Scanner::Scalar, input);
    for s in Scanner::ALL {
        // S6/S6b use simdjson's escape ordering and legitimately differ from
        // the scalar reference when a backslash appears outside a string.
        // That divergence is a property of the *escape scanner*, not the
        // classifier, so it applies to both the compare and table variants
        // and is covered by `scanner_equivalence.rs`.
        let got = scan(s, input);
        assert_eq!(
            got.positions(),
            expected.positions(),
            "{} disagrees on {:?}",
            s.label(),
            String::from_utf8_lossy(input)
        );
    }
}

#[test]
fn every_scanner_agrees_on_valid_documents() {
    let mut rng = corpus::Rng::new(0xC1A55);
    for _ in 0..10_000 {
        all_scanners_agree(corpus::random_value(&mut rng, 5).as_bytes());
    }
}

#[test]
fn every_scanner_agrees_on_the_corpora() {
    for (name, json) in corpus::suite(200_000, 31) {
        let expected = scan(Scanner::Scalar, json.as_bytes());
        for s in Scanner::ALL {
            assert_eq!(
                scan(s, json.as_bytes()).positions(),
                expected.positions(),
                "{} on {name}",
                s.label()
            );
        }
    }
}

#[test]
fn every_scanner_agrees_across_chunk_boundaries() {
    // The table classifier is per-chunk, so alignment bugs show up as
    // off-by-16 differences. Slide the payload through every phase.
    for pad in 0..80usize {
        let mut s = String::from("[");
        for _ in 0..pad {
            s.push(' ');
        }
        s.push_str(r#"{"k":"v\"x","n":[1,2]}]"#);
        all_scanners_agree(s.as_bytes());
    }
}

#[test]
fn table_scalar_scanner_matches_ifchain_on_arbitrary_bytes() {
    // ScalarTable and Scalar differ only in the classifier, so they must
    // agree on *everything*, valid or not.
    let mut rng = corpus::Rng::new(0xFEED);
    let mut buf = Vec::with_capacity(300);
    for _ in 0..20_000 {
        buf.clear();
        for _ in 0..rng.below(300) {
            buf.push((rng.next_u64() & 0xFF) as u8);
        }
        assert_eq!(
            scan(Scanner::ScalarTable, &buf).positions(),
            scan(Scanner::Scalar, &buf).positions(),
            "lut256 scalar scanner diverged"
        );
    }
}

#[test]
fn table_simd_scanners_match_compare_simd_scanners_on_arbitrary_bytes() {
    // Likewise: swapping the classifier must not change anything, even on
    // input where S6 as a whole diverges from the scalar oracle.
    let mut rng = corpus::Rng::new(0xBEEF);
    let mut buf = Vec::with_capacity(300);
    for _ in 0..20_000 {
        buf.clear();
        for _ in 0..rng.below(300) {
            buf.push((rng.next_u64() & 0xFF) as u8);
        }
        assert_eq!(
            scan(Scanner::BranchlessTable, &buf).positions(),
            scan(Scanner::Branchless, &buf).positions(),
            "S6 table vs compare diverged"
        );
        assert_eq!(
            scan(Scanner::Branchless2xTable, &buf).positions(),
            scan(Scanner::Branchless2x, &buf).positions(),
            "S6b table vs compare diverged"
        );
        assert_eq!(
            scan(Scanner::BranchlessHybrid, &buf).positions(),
            scan(Scanner::Branchless, &buf).positions(),
            "S6 hybrid vs compare diverged"
        );
        assert_eq!(
            scan(Scanner::Branchless2xHybrid, &buf).positions(),
            scan(Scanner::Branchless2x, &buf).positions(),
            "S6b hybrid vs compare diverged"
        );
    }
}
