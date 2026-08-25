# vela-json

A Rust port of **all four of Vela stage2's JSON tiers**, built to answer one
question: *is the Vela algorithm slow, or was velac's code generation slow?*

Answer: **the algorithms are fine.** Ported without redesign, tier 3 runs
2.3–3.7× faster under rustc/LLVM.

| Stage | Vela ([`JSON_IMPROVEMENT_PLAN.md:52-73`][plan]) | Straight port | After profiling |
|---|---|---|---|
| Stage 1 structural scan | 914 MB/s | 1.13–2.34 GiB/s | **2.14–3.25 GiB/s** (2.4–3.8×) |
| Full DOM build | 166 MB/s | 383–606 MiB/s | **450–785 MiB/s** (2.7–4.7×) |
| 68-byte document | 5 590 ns | 75 ns | — (75×) |

The third column is the interesting one: profiling moved Stage 1 a further
1.5× past the straight port, by fixing the part the design docs were *not*
pointing at.

| document | what is in it |
|---|---|
| **[`docs/TIERS.md`](docs/TIERS.md)** | all four Vela tiers head-to-head; which should be the default |
| **[`docs/RESULTS.md`](docs/RESULTS.md)** | all benchmarks, methodology, caveats, bugs found |
| **[`docs/PROFILING.md`](docs/PROFILING.md)** | CPU hot paths for every implementation incl. yyjson/simdjson |
| **[`docs/MEMORY.md`](docs/MEMORY.md)** | peak RSS and allocation counts across all implementations |
| **[`docs/CBASELINE.md`](docs/CBASELINE.md)** | the port vs yyjson and simdjson, built from vendored source |
| **[`docs/ZEROCOPY.md`](docs/ZEROCOPY.md)** | a YaFF-style zero-copy wire format for JSON |
| **[`docs/UNWRAP_FREE.md`](docs/UNWRAP_FREE.md)** | panic-free design study |

[plan]: ../../../vela/docs/stage2/JSON_IMPROVEMENT_PLAN.md

---

## What was ported

**All four tiers**, behind one `JsonTier` trait — the common interface
`P1_2_JSON_TIERS.md:109-141` specifies.

| tier | algorithm | Vela LOC | Rust module |
|---|---|---|---|
| 0 | scalar detection only; containers stubbed | 289 | `tiers::tier0` |
| 1 | recursive descent — **Vela's default** | 590 | `tiers::tier1` |
| 2 | structural index + tape DOM | 1619 | `tiers::tier2` |
| 3 | flat node pool | 2063 | `pool`/`builder`/`query`/`workspace` |

Tier 3, in detail:

| Vela source | Rust module |
|---|---|
| `tier2/structural.vl` | `scan::scalar` |
| `tier2/structural_simd.vl` (S6, S6b) | `scan::branchless` |
| `runtime/simd_json_branchless.ll` | `scan::branchless`, `scan::neon` |
| — (specified, never built) | `scan::table` |
| `tier3/parse_indexed.vl` — `__json_build_from_si` | `builder` |
| `runtime/json_pool_write.ll` — `__json_parse_scalar_fast` | `scalar` |
| `tier3/parse_indexed.vl` — `json_workspace_create/_ws` | `workspace` |
| `tier3/parse.vl` — query API | `query` |
| — (new) | `strict`, `de`, `ser`, `flat` |

Every function carries a `file:line` reference back to the Vela original.

### Which tier should be the default?

Tier 1 — the one Vela already defaults to, though not for the reason its
docs give. It is the fastest tier for single-field access at **every**
document size from 8 to 16 384 keys, and it allocates nothing.

| one field, last key | tier 1 | tier 2 | tier 3 | serde_json |
|---|---|---|---|---|
| 64 keys | **1.13 µs** | 1.80 µs | 2.50 µs | 5.43 µs |
| 16 384 keys | **251 µs** | 379 µs | 569 µs | 2.50 ms |

The indexed tiers only win past **~16 lookups per document**, and only if
the index is cached — which tier 2's API never does. Full analysis in
[`docs/TIERS.md`](docs/TIERS.md).

## Two parsers

**`Workspace`** — the faithful port. Bit-compatible with Vela, quirks
included: numbers are `i64` only (`3.14` parses as `314`), `null` is never
validated (`nope` parses as null), strings stay as raw slices, brackets are
not matched, depth past 256 is silently dropped, and nothing ever errors.

**`strict::StrictParser`** — same data structures, same Stage 1, RFC
8259-conformant Stage 2. Real `f64` numbers, grammar validation, bracket
matching, surrogate-pair checking, and errors with byte offsets.

Benchmarking one against the other isolates the cost of correctness from the
cost of the representation. It runs 1.4–2.0×, and still beats `serde_json` by
1.3–3.0×.

```rust
use vela_json::Workspace;

let mut ws = Workspace::new();
let doc = ws.parse(br#"{"name":"vela","tiers":[0,1,2,3]}"#);
assert_eq!(doc.root().get("name").and_then(|v| v.as_str()).as_deref(), Some("vela"));
```

```rust
use vela_json::strict::StrictParser;

let mut p = StrictParser::new();
let err = p.validate(br#"{"a":1]"#).unwrap_err();
assert_eq!(err.to_string(), "mismatched bracket at byte 6");
```

## Headline findings

**The lookup-table classifier Vela's design docs specify would have made it
slower.** `P1_2_JSON_TIERS.md:91` asks for "branchless character
classification (lookup tables)" and it was never built. Built here both
ways: the nibble-shuffle table is 19% faster in isolation and **2-8% slower
end-to-end**, because classification is off Stage 1's critical path.

**Position extraction is 64-82% of Stage 1; classification is 13-25%.**
Replacing Vela's serial per-bit loop with simdjson's unconditional
eight-slot write is worth +30-77%.

**A tape is the wrong shape for struct deserialization.** The same
representation that beats simd-json by 3.5x on string-heavy DOM building
loses to `serde_json` by 1.5x when filling a `#[derive(Deserialize)]`
struct, because streaming parsers never build an intermediate at all.

**`serde_json`'s default float parser is not correctly rounded** — it
deviates on 25% of high-precision literals. Ours matches `str::parse`.

**Vela's `emit_v2.vl` is not O(n).** It is documented as a "DualBuffer-backed
O(n) emitter" but calls `json_escape_string` (`common.vl:52`), a per-byte
string-concat loop, so string output is quadratic.

**`json_validate` does not validate.** Tiers 1 and 3 accept `{`, `[1,2`,
`txxx`, `[1,]` and every other truncated document, because the skip
functions return end-of-input and `validate` only checks that position
equals `len`. Agreement with a real parser: tier 0 36.4%, tier 1 79.1%,
tier 2 86.6%, tier 3 79.1%.

**Tier 2 throws its own index away.** `json_object_get`, `object_count`,
`object_keys` and `array_count` each call `json_tape_build`, so tier 2 does
strictly more work than tier 1 for the same result — 4.7× more at 64
lookups. Caching the tape turns a 2.2× loss into a 2.1× win.

## Bugs in the Vela sources

Found while porting. Details in [`docs/RESULTS.md` §6](docs/RESULTS.md).

1. **S6/S6b disagree with the scalar scanner on invalid input.** A backslash
   outside a string escapes the next byte in the branchless scanner but not
   in the scalar reference, so tier 3's output for malformed input depends on
   which scanner is compiled in. `t859_json_branchless.vl` asserts they are
   equivalent but only tests backslashes *inside* strings.
2. `read_null` in `json_pool_write.ll:732` is a no-op whose comment claims it
   performs a word compare.
3. `pool_container_to_json` reads object children from `payload` but array
   children from `idx + 1` — works only by accident.
4. `json_pool_object_get_float` is declared `i64`.
5. `emit_v2.vl`'s quadratic escaping, above.
6. `json_validate` accepting truncated input in all tiers.
7. Tier 2's `json_array_get` (`tier2/parse.vl:417`) does not use the tape —
   it is byte-for-byte tier 1's scan.
8. Tier 2's SIMD is off by default (`structural_gate.vl:4`), so stock tier 2
   benchmarks measure the scalar scanner.
9. Tier 1's `json_skip_value` recursion is unbounded — stack exhaustion on
   untrusted nested input.

## Panic freedom

No `unwrap`, `expect`, `panic!` or slice indexing in library code, enforced
by `#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic,
clippy::indexing_slicing, ...)]` and tested by 30 000 random-byte inputs,
every prefix of a valid document, and 40 000 corrupted `jsonflat` buffers
(where the buffer *is* the data structure, so a flipped byte is an attack).

## Layout

```
src/
  tiers/
    mod.rs       the JsonTier contract shared by all four
    common.rs    common.vl - skip/scan helpers
    tier0.rs     scalar only
    tier1.rs     recursive descent (Vela's default)
    tier2/       structural index + tape DOM
    tier3.rs     adapter over the pool implementation below
  tag.rs         node tag encoding (aux << 8 | type)
  pool.rs        the flat 16-byte node pool
  scan/
    mod.rs       StructuralIndex + unrolled bit extraction
    scalar.rs    byte-at-a-time reference scanner
    branchless.rs  S6 / S6b, generic over the classifier
    neon.rs      AArch64 compare classifier
    table.rs     the lookup tables Vela specified but never built
  scalar.rs      __json_parse_scalar_fast, quirks intact
  builder.rs     Stage 2 state machine
  query.rs       Doc / Value / entries / elements / skip_subtree
  unescape.rs    correct decoder + Vela's lossy one, for comparison
  workspace.rs   reusable arena
  strict.rs      RFC 8259 parser over the same pool
  de.rs          serde::Deserializer over the pool
  ser.rs         serde::Serializer with a vectorised escaper
  flat/
    mod.rs       jsonflat: dynamic zero-copy wire format
    typed.rs     jsonflat: schema-driven layout (0.67x vs dynamic 1.48x)
  corpus.rs      deterministic test/bench data
  bin/profile.rs profiling workloads
tests/
  tier_contract.rs         all four tiers vs each other and serde_json
  scanner_equivalence.rs   Stage 1 oracle tests (port of t859 + fuzz)
  classifier.rs            all 256 bytes, all 9 scanners
  faithful_semantics.rs    quirks pinned + panic freedom
  strict_conformance.rs    JSONTestSuite-style + serde_json differential
  serde_de.rs              deserializer vs serde_json
  serde_ser.rs             serializer, byte-identical to serde_json
  flat.rs                  roundtrip + 40k hostile buffers
  flat_typed.rs            schema layout, layout/schema confusion, fuzz
benches/
  tiers.rs  scan.rs  parse.rs  query.rs  structs.rs  zerocopy.rs
examples/
  typestate.rs   runnable unwrap-free demo
  sizes.rs       jsonflat buffer sizes
tools/
  profile.sh  symbolicate.py     CPU profiling with symbols
  memprofile.sh                  memory matrix, one process per impl
  cbench.sh                      isolated cross-language benchmarks
```

## Running

```bash
cargo test                          # 173 tests
cargo bench --bench tiers           # all four Vela tiers head-to-head
cargo bench --bench scan            # classifiers, phase breakdown, scanners
cargo bench --bench parse           # DOM parse vs the field
cargo bench --bench query           # parse + access patterns
cargo bench --bench structs         # struct de/ser + escaping
cargo bench --bench zerocopy        # jsonflat vs rkyv vs re-parsing
cargo clippy --lib                  # enforces panic freedom
cargo run --example typestate
cargo run --release --example sizes
tools/profile.sh vela_de 200        # CPU profile with symbols
tools/profile.sh yyjson 200         # ...including the C baselines
tools/memprofile.sh records 10485760   # memory matrix
./target/release/memprofile selftest   # calibrate the memory instrument
tools/cbench.sh c_parse_10mb        # isolated cross-language benchmark
tools/isolate.sh parse '^parse_10mb/'  # one process per benchmark, any bench
```

Toolchain: `rustc 1.95.0`. Nothing depends on a feature newer than 1.61.

## Attribution

The algorithms descend from **simdjson** (Apache-2.0; Lemire, Langdale,
Keiser) via Vela's tier 2, and **yyjson** (MIT; YaoYuan/ibireme) via Vela's
tier 3, as recorded in
`bootstrap/stage2/src/stdlib/encoding/json/AUTHORS`. No C/C++ code was
copied — only strategies.
