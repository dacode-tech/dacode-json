//! The crate is safe to use from more than one thread, and this states
//! exactly what that means.
//!
//! # Why a test and not a paragraph
//!
//! Nothing here is `unsafe impl Send`. Every type below gets its auto
//! traits from its fields, so today's answers are correct by
//! construction — the parser holds no global state, and which SIMD kernel
//! it uses is decided by `target_feature` at compile time rather than by a
//! cached runtime probe, so there is not even a `Once` to reason about.
//!
//! The risk is not that this is wrong now. It is that someone adds an
//! `Rc<RefCell<_>>` cache to `Parser` for a 3% win and nobody notices
//! that a public type stopped being `Send`. That is a silent semver break
//! for every caller with a thread pool, and a compile error here.
//!
//! # The contract
//!
//! **Every public type is `Send + Sync`.** Owned ones are also
//! `'static`. There are no exceptions, and that is not an accident of
//! this measurement — it follows from the crate having no interior
//! mutability at all. No `Cell`, no `RefCell`, no `Rc`, no `Mutex`, no
//! `static mut`, no `unsafe impl`.
//!
//! Note that the types holding an exclusive borrow are `Sync` too:
//! `write::Writer` holds `&mut [u8]`, and `&mut T` is `Sync` whenever `T`
//! is, because sharing a `&&mut T` only permits reading through it. The
//! first draft of this test asserted the opposite and failed to compile,
//! which is the reason it is a test.
//!
//! What follows practically: a `Doc` or a `flat::View` can go straight
//! into `rayon`; an error can cross a channel; a `Parser` can live in a
//! thread pool's slot. Nothing in the crate needs a lock, because nothing
//! in it is shared mutable state.

#![cfg(feature = "serde")]

use std::sync::Arc;
use std::thread;

// =====================================================================
// The contract, checked at compile time
// =====================================================================

const fn send_sync<T: Send + Sync>() {}
const fn send_sync_static<T: Send + Sync + 'static>() {}

#[test]
fn public_types_have_the_documented_auto_traits() {
    // --- owned: Send + Sync + 'static ---------------------------------
    send_sync_static::<dacodec::Parser>();
    send_sync_static::<dacodec::Workspace>();
    send_sync_static::<dacodec::Pool>();
    send_sync_static::<dacodec::Node>();
    send_sync_static::<dacodec::StructuralIndex>();
    send_sync_static::<dacodec::Scanner>();
    send_sync_static::<dacodec::Type>();
    send_sync_static::<dacodec::strict::StrictParser>();
    send_sync_static::<dacodec::strict::Options>();
    send_sync_static::<dacodec::builder::Stack>();
    send_sync_static::<dacodec::flat::Builder>();
    send_sync_static::<dacodec::flat::Intern>();
    send_sync_static::<dacodec::ser::Options>();
    send_sync_static::<dacodec::pull::Kind>();

    // Errors especially: they get returned across channels and collected
    // into aggregate reports. `stream::Error` is the one that used to
    // hold a `String` and now holds a `&'static str` or a `Box<str>` —
    // both fine here, which is worth pinning since that change was made
    // for code size and not for this.
    send_sync_static::<dacodec::Error>();
    send_sync_static::<dacodec::stream::Error>();
    send_sync_static::<dacodec::strict::Error>();
    send_sync_static::<dacodec::strict::ErrorKind>();
    send_sync_static::<dacodec::de::Error>();
    send_sync_static::<dacodec::ser::Error>();
    send_sync_static::<dacodec::write::Error>();
    send_sync_static::<dacodec::flat::Error>();

    // A generic writer must not inherit its schema marker's auto traits.
    // `TypedWriter<T>` stores `PhantomData<fn() -> T>` rather than a `T`,
    // so it stays `Send + Sync` even for a thread-hostile `T` — which is
    // the reason for that spelling.
    struct Hostile(core::marker::PhantomData<*const ()>);
    send_sync::<core::marker::PhantomData<fn() -> Hostile>>();

    // --- borrowing views ----------------------------------------------
    send_sync::<dacodec::Document<'_>>();
    send_sync::<dacodec::ValueRef<'_>>();
    send_sync::<dacodec::pull::Raw<'_>>();
    send_sync::<dacodec::pull::Fields<'_, '_>>();
    send_sync::<dacodec::flat::View<'_>>();
    send_sync::<dacodec::flat::Ref<'_>>();
    send_sync::<dacodec::flat::Elements<'_>>();
    send_sync::<dacodec::flat::Entries<'_>>();
    send_sync::<dacodec::de::Deserializer<'_>>();

    // --- exclusive borrows are `Sync` as well -------------------------
    //
    // `&mut T` is `Sync` when `T` is. Sharing a `&&mut [u8]` grants only
    // reads, so there is nothing to race. This is the assertion the first
    // draft got backwards.
    send_sync::<dacodec::write::Writer<'_>>();
    send_sync::<dacodec::ser::Serializer<'_>>();
}

// =====================================================================
// ...and at run time, in case the types are lying
// =====================================================================

const DOC: &str = r#"[
  {"id":1,"name":"alpha","score":11,"tags":["a","b"],"ratio":1.5},
  {"id":2,"name":"bravo","score":22,"tags":["c"],"ratio":2.25},
  {"id":3,"name":"charlie","score":33,"tags":[],"ratio":0.125}
]"#;

#[derive(serde::Deserialize, serde::Serialize, PartialEq, Debug)]
struct Rec {
    id: u32,
    score: i64,
}

/// Every entry point, hammered from eight threads at once.
///
/// The type checker cannot see a `static mut` reached through `unsafe`, or
/// a lazily-initialised table with a data race in it. This would.
/// Deliberately runs long enough for threads to genuinely overlap rather
/// than finish in sequence.
#[test]
fn every_entry_point_agrees_under_contention() {
    let want: Vec<Rec> = dacodec::from_str(DOC).expect("baseline");
    let want = Arc::new(want);
    let src = Arc::new(DOC.to_string());

    let mut handles = Vec::new();
    for t in 0..8u32 {
        let want = Arc::clone(&want);
        let src = Arc::clone(&src);
        handles.push(thread::spawn(move || {
            // A parser per thread, reused — the case the docs recommend,
            // and the one where shared state would show up.
            let mut parser = dacodec::Parser::new();
            let mut idx = dacodec::stream::Index::default();
            let mut ws = dacodec::Workspace::new();

            for i in 0..500 {
                let b = src.as_bytes();

                // typed, via the default path
                let got: Vec<Rec> = dacodec::from_slice(b).expect("from_slice");
                assert_eq!(&got, want.as_ref(), "thread {t} iter {i}");

                // typed, streaming with a reused index
                let got: Vec<Rec> = dacodec::stream::from_slice_with(&mut idx, b).expect("stream");
                assert_eq!(&got, want.as_ref());

                // typed, no index
                let got: Vec<Rec> = dacodec::direct::from_slice(b).expect("direct");
                assert_eq!(&got, want.as_ref());

                // the pool, through a reused parser and a reused workspace
                let doc = parser.parse(b).expect("parse");
                assert_eq!(doc.root().len(), 3);
                let doc = ws.parse(b);
                assert_eq!(doc.root().len(), 3);

                // on-demand, which allocates nothing
                let mut sum = 0i64;
                dacodec::pull::select(b, &[b"score"], |got| {
                    sum += got[0].and_then(|v| v.as_int::<i64>()).unwrap_or(0);
                    Ok(())
                })
                .expect("pull");
                assert_eq!(sum, 66);

                // writing into a stack buffer
                let mut buf = [0u8; 64];
                let mut w = dacodec::write::Writer::new(&mut buf);
                w.begin_object().expect("obj");
                w.key("t").expect("key").u64(u64::from(t)).expect("val");
                w.end_object().expect("end");
                let out = w.finish().expect("finish");
                assert_eq!(out, format!(r#"{{"t":{t}}}"#));

                // serializing
                let s = dacodec::to_string(want.as_ref()).expect("ser");
                assert_eq!(s, serde_json::to_string(want.as_ref()).expect("serde_json"));
            }
        }));
    }
    for h in handles {
        h.join().expect("thread panicked");
    }
}

/// A borrowing view really can be shared across threads, not just claimed
/// to be `Sync`.
#[test]
fn a_parsed_document_can_be_read_from_many_threads() {
    let mut ws = dacodec::Workspace::new();
    let doc = ws.parse(DOC.as_bytes());

    thread::scope(|s| {
        for _ in 0..8 {
            s.spawn(|| {
                for _ in 0..2_000 {
                    let root = doc.root();
                    assert_eq!(root.len(), 3);
                    let total: i64 = root
                        .elements()
                        .filter_map(|r| r.get("score"))
                        .filter_map(|v| v.as_i64())
                        .sum();
                    assert_eq!(total, 66);
                }
            });
        }
    });
}

/// The same for a `flat` buffer, which is the type most likely to be
/// `mmap`ed once and read by a pool of workers.
#[test]
fn a_flat_buffer_can_be_read_from_many_threads() {
    let mut ws = dacodec::Workspace::new();
    let buf = {
        let doc = ws.parse(DOC.as_bytes());
        dacodec::flat::encode(doc).expect("encode")
    };
    let view = dacodec::flat::View::new(&buf).expect("view");

    thread::scope(|s| {
        for _ in 0..8 {
            s.spawn(|| {
                for _ in 0..2_000 {
                    let root = view.root();
                    assert_eq!(root.len(), 3);
                    let total: i64 = root
                        .elements()
                        .filter_map(|r| r.get("score"))
                        .filter_map(|v| v.as_i64())
                        .sum();
                    assert_eq!(total, 66);
                }
            });
        }
    });
}

// =====================================================================
// Sharing without copying
// =====================================================================

/// A parsed document is shared, not copied, and this shows what that
/// looks like in both of the shapes a caller has.
///
/// The question this answers: "if I want to read one parsed document from
/// several threads, do I have to clone it first?" No, and there is no
/// arrangement in which you would. A `Doc` is `(&[u8], &Pool)` and a
/// `flat::View` is a `&[u8]` plus six integers, so both are `Copy`-cheap
/// handles onto storage someone else owns.
///
/// # Scoped threads: no `Arc`, no clone, nothing
///
/// `thread::scope` proves to the compiler that the threads end before the
/// borrow does, so a plain `&Doc` crosses the boundary.
///
/// # `'static` threads: `Arc` the *storage*, not the document
///
/// `thread::spawn` needs `'static`, and a `Doc` borrows, so it cannot be
/// moved into one. Put the two things it borrows in an `Arc` and rebuild
/// the handle per thread: `Doc::new` and `View::new` are O(1) — a header
/// check and some field assignments — so this is not a parse and not a
/// copy. The bytes are read once, by whoever built the `Arc`.
#[test]
fn one_document_is_read_by_many_threads_without_copying() {
    // --- scoped: share the handle itself ------------------------------
    let mut ws = dacodec::Workspace::new();
    let doc = ws.parse(DOC.as_bytes());
    let doc_ref = &doc;
    thread::scope(|s| {
        for _ in 0..4 {
            s.spawn(move || assert_eq!(score_of(*doc_ref), 66));
        }
    });

    // --- 'static: share the storage, rebuild the handle ---------------
    //
    // `Arc<(Vec<u8>, Pool)>` — one allocation, made once. Each thread
    // pairs them back into a `Doc`, which allocates nothing.
    let owned: Arc<(Vec<u8>, dacodec::Pool)> = {
        let bytes = DOC.as_bytes().to_vec();
        let pool = dacodec::strict::parse_to_pool(&bytes).expect("parse");
        Arc::new((bytes, pool))
    };
    let mut handles = Vec::new();
    for _ in 0..4 {
        let owned = Arc::clone(&owned);
        handles.push(thread::spawn(move || {
            let doc = dacodec::Document::new(&owned.0, &owned.1);
            assert_eq!(score_of(doc), 66);
        }));
    }
    for h in handles {
        h.join().expect("thread panicked");
    }

    // --- the same for `flat`, which is the case this format is for ----
    //
    // One `Arc<Vec<u8>>`, or a `&'static [u8]` from `mmap` or flash.
    // `View::new` validates a 32-byte header and returns; there is no
    // per-thread cost beyond that.
    let buf: Arc<Vec<u8>> = {
        let mut ws = dacodec::Workspace::new();
        let doc = ws.parse(DOC.as_bytes());
        Arc::new(dacodec::flat::encode(doc).expect("encode"))
    };
    let mut handles = Vec::new();
    for _ in 0..4 {
        let buf = Arc::clone(&buf);
        handles.push(thread::spawn(move || {
            let view = dacodec::flat::View::new(&buf).expect("view");
            let total: i64 = view
                .root()
                .elements()
                .filter_map(|r| r.get("score"))
                .filter_map(|v| v.as_i64())
                .sum();
            assert_eq!(total, 66);
        }));
    }
    for h in handles {
        h.join().expect("thread panicked");
    }
}

fn score_of(doc: dacodec::Document<'_>) -> i64 {
    doc.root()
        .elements()
        .filter_map(|r| r.get("score"))
        .filter_map(|v| v.as_i64())
        .sum()
}

// The claim above — that rebuilding a handle per thread costs nothing —
// is measured in `tests/error_alloc.rs`, not here. Counting allocations
// needs `memstat::Counter` installed as the global allocator, and that
// file is the one that installs it. Asserting it here would have been
// vacuous: with no `Counter`, `rust_allocs()` never moves and the
// assertion passes for the wrong reason.
