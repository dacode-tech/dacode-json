//! Every property in `dacode_json::fuzz::check`, driven by libFuzzer.
//!
//! Equivalent to simdjson's `fuzz_parser` / yyjson's `fuzzer.c`, but
//! differential: it compares against `serde_json` and against the crate's
//! own invariants rather than only looking for crashes.
#![no_main]
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let findings = dacode_json::fuzz::check(data);
    assert!(
        findings.is_empty(),
        "input {:?}\nfindings: {findings:?}",
        String::from_utf8_lossy(data.get(..200).unwrap_or(data))
    );
});
