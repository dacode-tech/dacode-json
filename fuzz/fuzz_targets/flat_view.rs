//! A `jsonflat` buffer is the data structure, so arbitrary bytes reaching
//! a reader are an attack. Every accessor must return `None`, never panic.
//!
//! Equivalent in spirit to simdjson's `fuzz_padded`, which checks the
//! reader tolerates hostile buffers.
#![no_main]
use libfuzzer_sys::fuzz_target;
use dacode_json::flat::View;

fuzz_target!(|data: &[u8]| {
    let Ok(view) = View::new(data) else { return };
    // Deep validation must also be total.
    let _ = view.validate_deep();

    fn walk(r: dacode_json::flat::Ref<'_>, depth: u32) {
        if depth > 8 {
            return;
        }
        let _ = (r.typ(), r.is_null(), r.as_bool(), r.as_i64(), r.as_f64());
        let _ = (r.as_str(), r.as_bytes(), r.len());
        let _ = (r.get("a"), r.get(""), r.at(0), r.at(usize::MAX));
        for (i, e) in r.elements().enumerate() {
            if i > 32 { break }
            walk(e, depth + 1);
        }
        for (i, (_k, v)) in r.entries().enumerate() {
            if i > 32 { break }
            walk(v, depth + 1);
        }
    }
    walk(view.root(), 0);
});
