//! Failing must not allocate.
//!
//! `pull` and `write` allocate nothing on the happy path, and that is the
//! claim the crate makes for them. It is worth little if the error path
//! allocates: a target with no heap has nothing to fall back on, and the
//! error path is the one a hostile input takes.
//!
//! Every message the library raises for itself is a `&'static str` (see
//! `src/errmsg.rs`), so the count asserted here is a literal zero. Only
//! `serde`'s `Error::custom` may allocate, because the trait hands the
//! text over as a `Display` and there is nowhere else to put it.
//!
//! # Why one test function
//!
//! [`memstat::Counter`] counts into process-global atomics, so a second
//! test running on another thread lands inside the measurement window.
//! `--test-threads=1` would fix it for anyone who remembered to pass it;
//! a single test function fixes it for everyone. The message assertions
//! at the end allocate freely and so come after all the counting.

use dacodec::memstat;

#[global_allocator]
static ALLOC: memstat::Counter = memstat::Counter;

/// Run `f` and return how many times it allocated.
fn allocs<R>(f: impl FnOnce() -> R) -> usize {
    let before = memstat::rust_allocs();
    let r = f();
    let n = memstat::rust_allocs() - before;
    drop(r);
    n
}

/// Malformed inputs covering every error site in `stream` and `pull`,
/// including the two that used to build their message with `format!`.
const BAD: &[&str] = &[
    "",
    "   ",
    "{",
    "[",
    "{\"a\" 1}",
    "{\"a\":1 \"b\":2}",
    "[1 2]",
    "[1,]",
    "{\"a\":1,}",
    "{1:2}",
    "\"unterminated",
    "01",
    "1.",
    "1e",
    "-",
    "tru",
    "nul",
    "{}extra",
    "[[[[[[[[[[[[[[[[[[[[",
];

#[test]
fn rejecting_bad_input_does_not_allocate() {
    // --- pull: allocation-free outright, error path included ---------
    for &src in BAD {
        let bytes = src.as_bytes();
        let run = || dacodec::pull::object(bytes, |_| Ok(()));
        // `pull` validates only what it reads, so some of these are
        // accepted. Either verdict is fine; the allocation count is not.
        let n = allocs(run);
        assert_eq!(n, 0, "pull::object({src:?}) allocated {n} times");
    }

    // --- stream: allocates its structural index and nothing else -----
    //
    // `from_slice_with` reuses a caller-owned index, so once the index
    // has grown to fit, a further parse — and the error it returns — is
    // allocation-free.
    let mut idx = dacodec::stream::Index::default();
    let widest = BAD.iter().copied().max_by_key(|s| s.len()).unwrap_or("");
    let _ = dacodec::stream::from_slice_with::<serde::de::IgnoredAny>(&mut idx, widest.as_bytes());
    for &src in BAD {
        let bytes = src.as_bytes();
        let n = allocs(|| {
            dacodec::stream::from_slice_with::<serde::de::IgnoredAny>(&mut idx, bytes)
                .expect_err("must be rejected")
        });
        assert_eq!(n, 0, "stream::from_slice_with({src:?}) allocated {n} times");
    }

    // --- direct: no index at all, so nothing may allocate ------------
    for &src in BAD {
        let bytes = src.as_bytes();
        let n = allocs(|| dacodec::direct::from_slice::<serde::de::IgnoredAny>(bytes).err());
        assert_eq!(n, 0, "direct::from_slice({src:?}) allocated {n} times");
    }

    messages_still_say_what_and_where();
}

/// The text and the offset survived the move from `String` to
/// `&'static str`. Formats freely, so it runs after the counting above
/// rather than as a test of its own.
fn messages_still_say_what_and_where() {
    let e = dacodec::from_str::<serde::de::IgnoredAny>("{\"a\":1 \"b\":2}").unwrap_err();
    let text = e.to_string();
    assert!(text.contains("byte 7"), "{text}");
    assert!(text.contains("expected ',' or '}'"), "{text}");

    // A parse failure surfacing through the pool deserializer keeps its
    // offset. The old `format!("parse error: {e}")` buried it in the
    // text, so `offset` was `None` for exactly the errors that had one.
    let e = dacodec::de::from_slice::<u32>(b"[1").unwrap_err();
    assert_eq!(e.offset, Some(2), "{e}");
    assert_eq!(e.to_string(), "unexpected end of input at byte 2");
}
