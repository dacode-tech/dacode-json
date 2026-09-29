//! `RawJson`: what the verbatim splice saves, and what capture costs.
//!
//! An event-sourced protocol carries opaque JSON — a tool call's `args`, a
//! provider's raw body — that is parsed once, stored, and re-emitted on every
//! subsequent turn (`docs/RAWJSON.md`). The claim under test is the one that
//! document opens with: re-emitting stored JSON through a parsed tree costs a
//! full re-encode, and splicing does not.
//!
//! Two groups, each measured against the alternative a caller would otherwise
//! have written:
//!
//! * `rawjson_splice_ser` — three rows over one event: the opaque field held as
//!   a `RawJson` and spliced, and the same field held as a parsed tree and
//!   re-encoded, once by `dacode-json` and once by `serde_json`. All three emit the
//!   same number of bytes (asserted in setup), so MiB/s is directly comparable
//!   and the gap is CPU. Quote the ratio against `serde_json_dom`: it is the
//!   stronger opponent, at ≈110× on the 853 KB payload (112-118× across four
//!   runs, `tools/isolate.sh` and a shared criterion process alike). Against
//!   `dacode_json_dom` the same measurement reads ≈130×, because `dacode-json`'s
//!   serializer drives a foreign enum 8-15% slower than `serde_json` drives its
//!   own — where in that range depends on size and harness. See *Fairness* below
//!   before quoting either.
//! * `rawjson_capture_de` — deserialize the same wire bytes into a `RawJson`
//!   field, into a parsed field, and into a struct that ignores the field
//!   entirely. The third row is the floor: it pays to walk past the bytes and
//!   keeps nothing. Capture sits ~2.6× above that floor, but the delta is an
//!   **upper bound** on the price of keeping them and not the price; the group's
//!   own docs decompose it.
//!
//! Payload size is swept because the shape of the win is the interesting part.
//! Both arms are linear in payload bytes and the re-encode side has no
//! measurable fixed cost, so the win is per-byte: against `dacode_json_dom` the
//! *marginal* ratio is already ~130-150× over the first interval and holds
//! there. What climbs across the sweep — ~50× at 2 KB, 130× at 853 KB — is the
//! *total* ratio, and that is the splice arm's own ~50 ns floor falling away:
//! 60% of its 2 KB time, 0.4% of its 853 KB time. The 2 KB point sits near the
//! measurement floor, not in a different regime.
//!
//! Byte-stability is **asserted, not timed** — see `check_fidelity` and
//! `check_padded_capture`. A regression in the verbatim guarantee does not make
//! these numbers worse, it makes them meaningless: output that quietly stopped
//! being byte-stable would still serialize fast, and a benchmark cannot see
//! that. So the bench fails instead.
//!
//! # Fairness
//!
//! The payload is `corpus::geo_int`: nested, object-rooted, and integer-only.
//! Integers matter — with floats in the payload the two paths could differ in
//! number *formatting*, and then the comparison would be about which float
//! printer ran rather than about re-encoding. As it stands the re-encoded output
//! differs from the source in key order alone, because `serde_json::Map` is a
//! `BTreeMap` unless its `preserve_order` feature is on, which it is not here.
//! That reordering is not a defect of this benchmark, it is the hazard the
//! splice exists to avoid: a provider's prompt cache keys on a byte-stable
//! prefix.
//!
//! The parsed tree is a `serde_json::Value`, which is the *most* favourable
//! opponent available rather than the least. `dacode-json` has no owned DOM that
//! implements `Serialize` — `query::Value<'a>` borrows both the pool and the
//! input — so the `dacode-json`-native way to re-emit a stored payload is `to_json`
//! (`src/query.rs:265`), the same call `RawJson::from_doc` makes. With the pool
//! already parsed and retained, that costs 5.3 ms for the 873 KB payload against
//! 1.6 ms to walk the same bytes as a `serde_json::Value`; neither figure
//! includes acquiring the representation, so the two are comparable. Re-parsing
//! first, 8.1 ms; acquiring a `RawJson` from the pool instead (`from_doc`),
//! 11.4 ms — a one-time cost, after which re-emitting is the 14 µs splice. A
//! caller holding a `dacode-json`-native parsed form therefore loses by more than
//! the ratio above, and quoting against the foreign tree is the conservative
//! choice. The strawman objection — "you raced a generic serializer
//! driving a foreign enum" — is what the `serde_json` row is for: same tree, same
//! bytes out, its own serializer.
//!
//! That row calls `serde_json::to_vec`, which allocates and reallocs from a
//! 128-byte start to the final size — ~13 reallocations at 873 KB — while both
//! `dacode-json` rows reuse one buffer. It costs `serde_json` a few percent, which
//! makes the quoted ratio a lower bound. The asymmetry is repo convention rather
//! than a choice made here: `benches/structs.rs:295-309` times `vela` against a
//! reused buffer and every contender through its own allocating API.
//!
//! `serde_json`'s own `RawValue` cannot appear as an opponent. It lives behind
//! `serde_json`'s `raw_value` feature, which this crate does not enable, and
//! enabling it in `[dev-dependencies]` would unify the feature into the
//! optional `serde_json` dependency every other bench and test builds against.
//! So the `serde_json` rows measure the re-encode path only, and the splice
//! column is `dacode-json`'s alone.
//!
//! The pool deserializer's capture (`de::from_doc`, canonical rather than
//! byte-exact — `docs/RAWJSON.md` §4) is deliberately absent from the sweep: it
//! re-serialises a tree, so it would re-measure the DOM cost
//! `rawjson_splice_ser` already prices, with a parse stapled in front of it. It
//! is not absent from the file: a canonicalising capture is precisely the
//! regression `check_padded_capture` exists to fail on.

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use dacode_json::{corpus, ser, RawJson};
use serde::{Deserialize, Serialize};
use std::hint::black_box;
use std::sync::OnceLock;

const SEED: u64 = 0xA7C5;

/// Payload sizes in `corpus::geo_int` features. `corpus::sized` is bypassed
/// because its first step already generates 64 features, which overshoots the
/// small end of the sweep — and the small end is where a fixed cost would show.
const PAYLOADS: [usize; 3] = [8, 128, 3072];

/// The whitespace-padded fidelity case is small: it checks a property, not a
/// rate, so there is nothing to gain from paying for it three times over.
const PADDED_FEATURES: usize = 8;

/// The fixed prefix. Deliberately tiny next to the payload: the question is
/// what the opaque field costs, not what the envelope costs.
const SEQ: u64 = 4242;
const TS: u64 = 1_700_000_000_000_000;
const KIND: &str = "tool_call";

/// The wire form of one event, written out independently of the serializer:
/// prefix fields in declaration order, then the payload spliced in raw.
fn expected_wire(payload: &str) -> String {
    format!(r#"{{"seq":{SEQ},"ts":{TS},"kind":"{KIND}","args":{payload}}}"#)
}

// =====================================================================
// The event, in each representation a caller could hold
// =====================================================================

/// The producer's view: the opaque field is validated text, spliced out
/// unchanged.
#[derive(Serialize)]
struct Spliced<'a> {
    seq: u64,
    ts: u64,
    kind: &'a str,
    args: &'a RawJson,
}

/// The alternative: the same field as a parsed tree, which the serializer has
/// to walk and re-encode on every turn.
#[derive(Serialize)]
struct Reencoded<'a> {
    seq: u64,
    ts: u64,
    kind: &'a str,
    args: &'a serde_json::Value,
}

/// Capture: walk the span to find its end, then validate and copy it out
/// (`src/direct.rs` -> `src/raw.rs`). The walk validates as it goes, which is
/// the redundancy `bench_capture_deserialize` prices.
#[derive(Deserialize)]
struct Captured {
    seq: u64,
    ts: u64,
    kind: String,
    args: RawJson,
}

/// Parse: build a tree out of the same bytes.
#[derive(Deserialize)]
struct Parsed {
    seq: u64,
    ts: u64,
    kind: String,
    args: serde_json::Value,
}

/// Ignore: walk past the payload and keep nothing — the floor for capture.
#[derive(Deserialize)]
struct PrefixOnly {
    seq: u64,
    ts: u64,
    kind: String,
}

/// Touch every field of a parsed event, so the tree the deserializer built
/// cannot be dropped on the floor by an optimizer that sees nothing reading it.
fn keep_parsed(e: &Parsed) -> usize {
    let kept = black_box(e);
    kept.args.as_object().map_or(0, |m| m.len())
        + kept.seq as usize
        + kept.ts as usize
        + kept.kind.len()
}

/// One size point, with the fidelity checks already run over it.
struct Case {
    /// What the payload actually weighed, for `BenchmarkId`.
    name: String,
    /// The payload as the producer received it. `raw.get()` is the source text.
    raw: RawJson,
    /// The same payload parsed — what the re-encode path holds.
    dom: serde_json::Value,
    /// The event on the wire, prefix and all: what deserialize sees.
    wire: String,
}

/// Built once per process, and the fidelity checks along with it. Both groups
/// want the same corpora, and rebuilding them would re-run — and re-print — the
/// whole fidelity table a second time.
fn cases() -> &'static [Case] {
    static CASES: OnceLock<Vec<Case>> = OnceLock::new();
    CASES.get_or_init(|| {
        check_padded_capture();
        PAYLOADS.iter().copied().map(build_case).collect()
    })
}

fn build_case(features: usize) -> Case {
    let payload = corpus::geo_int(features, SEED);
    let raw: RawJson = payload.parse().expect("corpus payload is valid JSON");
    let dom: serde_json::Value = serde_json::from_str(raw.get()).expect("payload parses as a tree");
    let wire = splice_wire(&raw);
    let name = size_label(raw.get().len());
    check_fidelity(&name, &raw, &dom, &wire);
    Case {
        name,
        raw,
        dom,
        wire,
    }
}

/// Serialize one event with its payload spliced in — the producer's path, and
/// the bytes every fidelity check and every capture row is written against.
fn splice_wire(raw: &RawJson) -> String {
    dacode_json::to_string(&Spliced {
        seq: SEQ,
        ts: TS,
        kind: KIND,
        args: raw,
    })
    .expect("splice serializes")
}

/// Capture fidelity over input the corpus cannot express.
///
/// `corpus::geo_int` emits compact JSON, so a capture that minified, reordered
/// or re-escaped the payload would hand back bytes equal to the source and every
/// check in `check_fidelity` would still pass. Padding makes byte-exactness
/// observable, which is exactly the line `docs/RAWJSON.md` §4 draws between the
/// positional deserializer — it slices the span it walked — and the pool, which
/// re-serialises canonically. Wiring `from_slice` to the pool must fail here and
/// must not be caught by anything else in this file.
///
/// Fidelity only, and outside the timed sweep: `check_fidelity`'s byte-count
/// assertions are about compact corpus output and would rightly fail on padded
/// input, where the two representations no longer emit the same length.
fn check_padded_capture() {
    let compact = corpus::geo_int(PADDED_FEATURES, SEED);
    // A space after every separator. This is not a pretty-printer: it is blind
    // to string contents, which is safe only because this corpus puts neither
    // `,` nor `:` inside a string.
    let mut padded = String::with_capacity(compact.len() + compact.len() / 8);
    for c in compact.chars() {
        padded.push(c);
        if c == ',' || c == ':' {
            padded.push(' ');
        }
    }
    assert!(
        padded.len() > compact.len(),
        "the corpus has no separators to pad, so this check would be vacuous"
    );

    let raw: RawJson = padded.parse().expect("padded payload is valid JSON");
    let wire = splice_wire(&raw);
    assert_eq!(
        wire,
        expected_wire(&padded),
        "padded: the envelope or the splice moved"
    );

    let captured: Captured = dacode_json::from_slice(wire.as_bytes()).expect("wire parses");
    assert_eq!(
        captured.args.get(),
        raw.get(),
        "padded: capture canonicalised the payload instead of slicing it"
    );

    println!("\n  --- fidelity: whitespace-padded payload ---");
    println!(
        "  source        {:>9} bytes  every separator padded",
        raw.get().len()
    );
    println!(
        "  captured      {:>9} bytes  byte-exact, padding preserved",
        captured.args.get().len()
    );
}

/// A label that reports what the generator actually produced rather than what
/// was asked for.
fn size_label(bytes: usize) -> String {
    if bytes >= 1 << 20 {
        format!("{}mb", (bytes + (1 << 19)) >> 20)
    } else {
        format!("{}kb", (bytes + 512) >> 10)
    }
}

/// Offset of the first byte at which two documents differ, or `None` if they
/// are identical.
fn first_difference(a: &str, b: &str) -> Option<usize> {
    match a
        .as_bytes()
        .iter()
        .zip(b.as_bytes())
        .position(|(x, y)| x != y)
    {
        Some(i) => Some(i),
        None if a.len() != b.len() => Some(a.len().min(b.len())),
        None => None,
    }
}

/// The guarantees the splice exists for, checked once per size point before
/// anything is timed.
///
/// Four properties, in the order a caller depends on them: the envelope comes
/// out exactly as written with the payload spliced into it, re-emitting the same
/// value is deterministic, capture-then-splice is a fixed point, and both
/// re-encode rows emit the byte count their MiB/s is reported against. The third
/// is the one a prompt cache rests on — an agent loop re-sends its whole history
/// every turn, so the bytes it sends on turn N+1 must be the bytes it parsed on
/// turn N.
///
/// The re-encoded alternative's *byte content* is reported, not asserted
/// against — only its byte count is, below. Whether a DOM round trip preserves
/// the source depends on `serde_json`'s map type, and failing this bench because
/// someone turned `preserve_order` on would point at the wrong thing.
fn check_fidelity(name: &str, raw: &RawJson, dom: &serde_json::Value, wire: &str) {
    let event = Spliced {
        seq: SEQ,
        ts: TS,
        kind: KIND,
        args: raw,
    };

    // Pin the whole envelope rather than look for the payload inside it: field
    // order and the prefix values are fixed too, and an escaped or
    // sentinel-wrapped payload could not match. A self-consistent corruption of
    // a prefix value — `seq` re-encoded wrongly, say — is caught here and by
    // nothing else below.
    assert_eq!(
        wire,
        expected_wire(raw.get()),
        "{name}: the envelope or the splice moved"
    );

    // Deterministic under repetition, which is weaker than it looks: `event` is
    // immutable, so this can only catch a serializer that accumulates state
    // between calls. A deterministic minify or re-escape sails through all eight
    // turns; the envelope assertion above is what catches that, and
    // `check_padded_capture` is what makes it catchable at all.
    for turn in 0..8 {
        let again = dacode_json::to_string(&event).expect("splice serializes");
        assert_eq!(
            again, wire,
            "{name}: splice is not byte-stable on turn {turn}"
        );
    }

    // Capture the wire, splice it back out: a fixed point, byte-exact. This is
    // the positional deserializer's path (`src/direct.rs`), the one callers get
    // from `from_str`/`from_slice`.
    let captured: Captured = dacode_json::from_str(wire).expect("wire parses");
    assert_eq!(
        captured.args.get(),
        raw.get(),
        "{name}: capture was not byte-exact"
    );
    let respliced = dacode_json::to_string(&Spliced {
        seq: captured.seq,
        ts: captured.ts,
        kind: &captured.kind,
        args: &captured.args,
    })
    .expect("splice serializes");
    assert_eq!(
        respliced, wire,
        "{name}: capture then splice moved the bytes"
    );

    // Both re-encode rows report MiB/s against `wire.len()`, so both have to
    // actually emit that many bytes: same fields, same values, keys permuted.
    // If that ever stops holding, the throughput numbers stop being comparable
    // and the gap between the rows stops being CPU.
    let reencoded = Reencoded {
        seq: SEQ,
        ts: TS,
        kind: KIND,
        args: dom,
    };
    let ours = dacode_json::to_string(&reencoded).expect("tree serializes");
    assert_eq!(
        ours.len(),
        wire.len(),
        "{name}: dacode-json re-encode changed the byte count"
    );
    let theirs = serde_json::to_vec(&reencoded).expect("tree serializes");
    assert_eq!(
        theirs.len(),
        wire.len(),
        "{name}: serde_json re-encode changed the byte count"
    );

    println!("\n  --- fidelity: {name} payload ---");
    println!("  source        {:>9} bytes", raw.get().len());
    println!(
        "  spliced       {:>9} bytes  source preserved verbatim",
        wire.len()
    );
    match first_difference(wire, &ours) {
        Some(i) => println!(
            "  re-encoded    {:>9} bytes  same length, first differs at byte {i}",
            ours.len()
        ),
        None => println!(
            "  re-encoded    {:>9} bytes  source preserved verbatim",
            ours.len()
        ),
    }
}

// =====================================================================
// (a) Splice throughput
// =====================================================================

/// Serializing an event whose opaque field is text versus one whose opaque
/// field is a tree. Same envelope, same payload, same output size — three rows,
/// because the tree gets re-encoded by both serializers and only `dacode-json` can
/// splice. Quote against `serde_json_dom`, the stronger of the two opponents.
fn bench_splice_serialize(c: &mut Criterion) {
    let mut group = c.benchmark_group("rawjson_splice_ser");
    group.sample_size(30);

    for case in cases() {
        group.throughput(Throughput::Bytes(case.wire.len() as u64));

        let spliced = Spliced {
            seq: SEQ,
            ts: TS,
            kind: KIND,
            args: &case.raw,
        };
        let reencoded = Reencoded {
            seq: SEQ,
            ts: TS,
            kind: KIND,
            args: &case.dom,
        };

        // Both `dacode-json` rows write into one reused buffer: a producer
        // re-emitting its history every turn keeps one, and
        // `benches/structs.rs::bench_serialize` times the same way.
        let mut buf = Vec::with_capacity(case.wire.len() + 64);
        group.bench_with_input(
            BenchmarkId::new("dacode_json_splice", &case.name),
            &spliced,
            |b, event| {
                b.iter(|| {
                    buf.clear();
                    ser::to_writer(&mut buf, black_box(event)).expect("ser");
                    black_box(buf.len())
                });
            },
        );

        group.bench_with_input(
            BenchmarkId::new("dacode_json_dom", &case.name),
            &reencoded,
            |b, event| {
                b.iter(|| {
                    buf.clear();
                    ser::to_writer(&mut buf, black_box(event)).expect("ser");
                    black_box(buf.len())
                });
            },
        );

        // `serde_json` gets `to_vec`, not `to_writer`: its writer path goes
        // through `io::Write` and is not the equivalent of `dacode-json`'s, which
        // takes a `Vec<u8>` directly. Same convention as `benches/structs.rs`.
        group.bench_with_input(
            BenchmarkId::new("serde_json_dom", &case.name),
            &reencoded,
            |b, event| {
                b.iter(|| black_box(serde_json::to_vec(black_box(event)).expect("ser").len()));
            },
        );
    }

    group.finish();
}

// =====================================================================
// (b) Capture cost
// =====================================================================

/// The same wire bytes into three destinations — captured, parsed, ignored —
/// on `dacode-json`'s positional deserializer, with `serde_json` as the reference
/// for the two of the three it can also do.
///
/// `dacode_json_raw` over `dacode_json_ignored` is an **upper bound** on the price of
/// keeping the payload, not the price. The floor row walks past the bytes and
/// keeps nothing; capture walks them and then pays for them a second time. At
/// 853 KB the delta is ~3.2 ms (5.2 against 2.0, `tools/isolate.sh`), and it
/// decomposes as:
///
/// * ~3.2 ms — `RawJson::from_string` re-validates the span it was handed
///   (`src/raw.rs:93-96` -> `crate::validate`), over bytes `src/direct.rs:504`
///   had already validated while walking them with `IgnoredAny`. That call also
///   builds a fresh `StrictParser`, index and pool included, per invocation
///   (`src/api.rs:276`): 20 of capture's 22 allocations.
/// * 14 µs — the memcpy. This is the part that is inherent to keeping the bytes,
///   and it is 0.4% of the delta.
///
/// So read the row as *capture as implemented today*. Removing the redundant
/// pass is a `src/` change and deliberately not made here — a benchmark may not
/// alter what it measures — but the ratio must not be quoted as if the overhead
/// were intrinsic. A caller weighing capture against *ignoring the field* is
/// asked to pay ~2.6× for something that ought to cost ~0.7% above that floor
/// (14 µs of memcpy over the 2.0 ms walk); a caller weighing it against *building
/// a tree* already wins by 1.5×, and would win by more.
///
/// Inherent and visible regardless: capture allocates 2 objects against the
/// tree's 114 080 for the same 873 KB — 22 as implemented today, 20 of them the
/// redundant pass — while staying byte-exact where the tree is not.
fn bench_capture_deserialize(c: &mut Criterion) {
    let mut group = c.benchmark_group("rawjson_capture_de");
    group.sample_size(30);

    for case in cases() {
        let wire = case.wire.as_bytes();
        group.throughput(Throughput::Bytes(wire.len() as u64));

        group.bench_with_input(
            BenchmarkId::new("dacode_json_raw", &case.name),
            wire,
            |b, wire| {
                b.iter(|| {
                    let e: Captured = dacode_json::from_slice(black_box(wire)).expect("de");
                    // Keep the whole event live: the capture is an owned copy, and
                    // black-boxing only its length would let the copy go.
                    let kept = black_box(&e);
                    kept.args.get().len() + kept.seq as usize + kept.ts as usize + kept.kind.len()
                });
            },
        );

        group.bench_with_input(
            BenchmarkId::new("dacode_json_dom", &case.name),
            wire,
            |b, wire| {
                b.iter(|| {
                    let e: Parsed = dacode_json::from_slice(black_box(wire)).expect("de");
                    black_box(keep_parsed(&e))
                });
            },
        );

        group.bench_with_input(
            BenchmarkId::new("dacode_json_ignored", &case.name),
            wire,
            |b, wire| {
                b.iter(|| {
                    let e: PrefixOnly = dacode_json::from_slice(black_box(wire)).expect("de");
                    black_box(e.seq as usize + e.ts as usize + e.kind.len())
                });
            },
        );

        // The reference rows. `serde_json` has no splice counterpart here —
        // its `RawValue` needs the `raw_value` feature, which is off.
        group.bench_with_input(
            BenchmarkId::new("serde_json_dom", &case.name),
            wire,
            |b, wire| {
                b.iter(|| {
                    let e: Parsed = serde_json::from_slice(black_box(wire)).expect("de");
                    black_box(keep_parsed(&e))
                });
            },
        );

        group.bench_with_input(
            BenchmarkId::new("serde_json_ignored", &case.name),
            wire,
            |b, wire| {
                b.iter(|| {
                    let e: PrefixOnly = serde_json::from_slice(black_box(wire)).expect("de");
                    black_box(e.seq as usize + e.ts as usize + e.kind.len())
                });
            },
        );
    }

    group.finish();
}

criterion_group!(benches, bench_splice_serialize, bench_capture_deserialize);
criterion_main!(benches);
