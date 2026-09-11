//! Serialize -> parse -> serialize must be a fixed point.
//!
//! Equivalent to simdjson's `fuzz_minify`: the second serialization must
//! equal the first, byte for byte.
#![no_main]
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Ok(v) = serde_json::from_slice::<serde_json::Value>(data) else {
        return;
    };
    let Ok(once) = dacode_json::ser::to_vec(&v) else {
        return;
    };
    // Our own output must be accepted by our own strict parser.
    dacode_json::strict::validate(&once)
        .unwrap_or_else(|e| panic!("emitted invalid JSON: {e}\n{:?}", String::from_utf8_lossy(&once)));

    let back: serde_json::Value =
        serde_json::from_slice(&once).expect("our output must re-parse");
    let twice = dacode_json::ser::to_vec(&back).expect("re-serialize");
    assert_eq!(
        once,
        twice,
        "serialization is not idempotent:\n  {:?}\n  {:?}",
        String::from_utf8_lossy(&once),
        String::from_utf8_lossy(&twice)
    );
});
