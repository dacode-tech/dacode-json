# `RawJson` — an owned value that splices verbatim

> *"An event-sourced protocol carries opaque JSON — a tool call's `args`, a
> provider's raw body. It is parsed once, stored, then re-emitted on every
> subsequent turn. How do we embed it in a larger document without the round
> trip perturbing it?"*

**With `RawJson`, behind the `raw_value` feature.** It is `dacodec`'s
counterpart of `serde_json`'s `RawValue`: an owned, validated JSON document
carried as text and written back out **byte-for-byte** — no re-escaping, no
re-encoding, no key reordering. `src/raw.rs`, no `unsafe`.

The interesting part is not the splice. It is that **capture fidelity depends on
which deserializer runs**, and why one of the three cannot be byte-exact.

---

## 1. Why verbatim matters

The motivating caller is a prompt cache. Providers key a cache on a byte-stable
prefix of the request; re-send the same bytes and the prefix hits, change one
byte and everything after it misses. An agent loop re-sends its whole history
every turn, and that history contains tool-call `args` it parsed from an earlier
response.

So `args` must survive parse → store → re-emit **identically**. Two naive
approaches both fail:

| approach | what breaks |
|---|---|
| store an owned `Value` tree, re-serialize | serialization can reorder keys or reformat `1.0`/`1e3`/bigints → different bytes → cold cache |
| store the JSON as a `String` field | it gets escaped: `"args":"{\"path\":…}"` — a string, not a nested object |

`RawJson` stores the text and splices it raw, so the bytes that go out are the
bytes that came in. That is the whole feature.

## 2. The type

```rust
pub struct RawJson(Box<str>);   // validated JSON text; Clone + Send + Sync + 'static

let raw: RawJson = r#"{"a":1}"#.parse()?;   // FromStr; validates one well-formed value
let raw = RawJson::from_string(owned)?;     // avoids a copy when you already own it
let raw = RawJson::from_doc(doc)?;          // capture a parsed subtree (canonicalises, validates)
raw.get();          // &str
raw.as_bytes();     // &[u8]
```

Construction **validates** (`crate::validate`). That is what makes the splice
safe: a `RawJson` always holds exactly one well-formed JSON value, so embedding
it cannot corrupt the surrounding document. `Box<str>` rather than `Box<[u8]>`
because JSON is UTF-8 text and `str` makes `Display`/`Debug`/the splice natural.

It serializes like any other value — nest it in a `#[derive(Serialize)]` struct
and the field comes out as raw JSON, not a quoted string:

```rust
#[derive(Serialize, Deserialize)]
struct Event { name: String, args: RawJson }

let e = Event { name: "edit".into(), args: r#"{"path":"a.rs"}"#.parse()? };
assert_eq!(dacodec::to_string(&e)?, r#"{"name":"edit","args":{"path":"a.rs"}}"#);
```

## 3. How the splice works

serde's `Serializer` trait has no "write these bytes raw" method, so the trick is
the one `serde_json` uses: a **sentinel**. `RawJson::serialize` emits
`serialize_struct(TOKEN, 1)` + `serialize_field(TOKEN, &text)`, where

```rust
pub const TOKEN: &str = "$dacodec::private::RawJson";
```

`dacodec`'s serializer recognizes `TOKEN` in `serialize_struct` (`ser.rs`), opens
**no brace** (`Close::RawValue`), and routes the single field through a
`RawEmitter` whose `serialize_str` does `out.extend_from_slice(text.as_bytes())`
— verbatim. `finish()` closes with nothing. A *foreign* serializer that does not
know `TOKEN` falls back to emitting the sentinel object, exactly as `serde_json`
does; it never silently produces wrong bytes.

## 4. Capture fidelity — the subtle part

`dacodec` has three deserializers, and a `RawJson` field is captured differently
by each. This is inherent to their representations, not an oversight:

| deserializer | entry points | capture | why |
|---|---|---|---|
| **positional** (`direct.rs`) | `from_str`, `from_slice`, `from_reader`, `Parser::deserialize` | **byte-exact** | it walks the input by byte offset; record `start`, skip one validated value (`IgnoredAny`), slice `input[start..pos]` |
| **pool** (`de.rs`) | `de::from_doc`, `de::from_slice` | **canonical** (`to_json`) | the node pool stores object/array nodes as *(first-child-index, count)*, **not** a byte span — the source range is not retained, so it re-serializes |
| **stream** (`stream/deserializer.rs`) | `stream::from_slice_with` | **error** | not wired; a `RawJson` field is a clean `invalid_type` error, never corruption |

The common path — `from_slice`/`from_str`/`Parser` — is the positional one, so
**in practice capture is byte-exact**, matching `serde_json`'s borrowed
`RawValue`. The pool path is canonical: deterministic (the same parsed value
always yields the same bytes), so still byte-*stable* across re-splices, just not
identical to the original whitespace. For the cache use-case either suffices —
capture once, splice the same bytes forever — but byte-exact also round-trips a
document unchanged, which is nicer.

Making the pool path byte-exact would mean widening the 16-byte node to carry a
span (or a side table of spans) and touching the SIMD parser. That cost buys
nothing the positional path does not already give, so it is deliberately not
paid. The stream path could be wired the same way as positional if a caller needs
it; until then it fails loudly rather than guessing.

## 5. Guarantees

- **No panics.** `raw.rs` and the `ser`/`de`/`direct` hooks compile under the
  crate's `#![deny(clippy::unwrap_used, expect_used, panic, unreachable,
  indexing_slicing, …)]`. Spans are sliced with `.get(..)`, UTF-8 with
  `from_utf8`, both erroring rather than trapping.
- **Valid by construction.** Every `RawJson` holds one validated JSON value, so
  the verbatim splice cannot emit a malformed document.
- **Owned and thread-safe.** `Box<str>` is `Clone + Send + Sync + 'static`, so a
  `RawJson` lives in an event that is stored, broadcast, and persisted without
  borrowing the input.
- **Default build untouched.** Everything is `#[cfg(feature = "raw_value")]`;
  with the feature off, the serializer/deserializer hot paths are byte-identical
  to before and the 202-test suite is unaffected.

## 6. Where it loses

- **Capture is not always source-exact.** Only the positional deserializer is
  byte-exact; the pool re-serializes (§4). If you need byte-exact capture, parse
  with `from_slice`/`Parser`, not `from_doc`.
- **One extra `Box<str>` per value** versus borrowing `&RawValue` from the input
  the way `serde_json` allows. `dacodec`'s `RawJson` is always owned — there is
  no borrowed `&RawJson` form — because the use-case (store and re-emit later)
  needs ownership anyway.
- **The sentinel is a struct named `TOKEN`.** A schema that legitimately has a
  struct named `$dacodec::private::RawJson` would collide. The name is chosen so
  that this does not happen by accident.

## 7. Reproduce

```bash
cargo test --features raw_value --lib raw::    # the RawJson unit tests
cargo test --features raw_value --doc raw      # the module doc example
cargo bench --features raw_value --bench rawjson # splice vs re-encode, capture cost
cargo clippy --all-targets --features raw_value
cargo build --no-default-features --features raw_value   # no_std + alloc
```

Quote bench numbers from `tools/isolate.sh rawjson '^rawjson' --features
raw_value`, one process per benchmark, for consistency with the rest of the
repo. Isolation buys little for this bench specifically: it stages its inputs
once through `bench_with_input` rather than per batch, so the cross-benchmark
interference `isolate.sh` exists to remove barely applies, and within a single
build the two harnesses agree to ~0.3% (13.92 µs shared against 13.97 µs
isolated for the 853 KB splice). What moves these numbers is the *build*: with
`lto = "fat"` and `codegen-units = 1`, one edit to this file moved the same
in-process measurement from 14.43 µs to 17.57 µs (+22%) — the inlining drift
`tools/isolate.sh`'s header documents at up to ~2.5×, and which its header
assigns to the per-file wrappers rather than to isolation, so no harness choice
removes it. Take both arms of any ratio from one binary, and never mix figures
across builds. The bench also asserts the verbatim guarantees before it times
anything — including over a whitespace-padded payload, which is the only input
that can tell a byte-exact capture from a canonicalising one (§4).
