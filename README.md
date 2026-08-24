# vela-json

A Rust port of **Vela stage2's tier-3 JSON parser**, built to answer one
question: *is the Vela algorithm slow, or was velac's code generation slow?*

Answer: **the algorithm is fine.** The same algorithm, ported without
redesign, runs 2.3–3.7× faster under rustc/LLVM.

| Stage | Vela ([`JSON_IMPROVEMENT_PLAN.md:52-73`][plan]) | This port | Speedup |
|---|---|---|---|
| Stage 1 structural scan | 914 MB/s | 1.13–2.34 GiB/s | **1.3–2.6×** |
| Full DOM build | 166 MB/s | 383–606 MiB/s | **2.3–3.7×** |
| 68-byte document | 5 590 ns | 73 ns | **76×** |

Full numbers, methodology and caveats: **[`docs/RESULTS.md`](docs/RESULTS.md)**
The `unwrap`-free design study: **[`docs/UNWRAP_FREE.md`](docs/UNWRAP_FREE.md)**

[plan]: ../../../vela/docs/stage2/JSON_IMPROVEMENT_PLAN.md

---

## What was ported

Vela ships four JSON tiers. Tier 3 is the fastest end-to-end path — a
yyjson-style flat 16-byte node pool fed by a simdjson-style structural index
(Vela's own tier-2 Stage 1).

| Vela source | Rust module |
|---|---|
| `tier2/structural.vl` | `scan::scalar` |
| `tier2/structural_simd.vl` (S6, S6b) | `scan::branchless` |
| `runtime/simd_json_branchless.ll` | `scan::branchless`, `scan::neon` |
| `tier3/parse_indexed.vl` — `__json_build_from_si` | `builder` |
| `runtime/json_pool_write.ll` — `__json_parse_scalar_fast` | `scalar` |
| `tier3/parse_indexed.vl` — `json_workspace_create/_ws` | `workspace` |
| `tier3/parse.vl` — query API | `query` |
| — (new) | `strict` |

Every function carries a `file:line` reference back to the Vela original.

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

## Findings fed back to Vela

Four issues in the original, found while porting. Details in
[`docs/RESULTS.md` §6](docs/RESULTS.md).

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

## Panic freedom

No `unwrap`, `expect`, `panic!` or slice indexing in library code, enforced
by `#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic,
clippy::indexing_slicing, ...)]` and tested by 30 000 random-byte and
every-prefix fuzz cases.

## Layout

```
src/
  tag.rs         node tag encoding (aux << 8 | type)
  pool.rs        the flat 16-byte node pool
  scan/
    mod.rs       StructuralIndex + batch bit extraction
    scalar.rs    byte-at-a-time reference scanner
    branchless.rs  S6 / S6b, classify / find_escaped / prefix_xor
    neon.rs      AArch64 classify
  scalar.rs      __json_parse_scalar_fast, quirks intact
  builder.rs     Stage 2 state machine
  query.rs       Doc / Value / entries / elements / skip_subtree
  unescape.rs    correct decoder + Vela's lossy one, for comparison
  workspace.rs   reusable arena
  strict.rs      RFC 8259 parser over the same pool
  corpus.rs      deterministic test/bench data
tests/
  scanner_equivalence.rs   Stage 1 oracle tests (port of t859 + fuzz)
  faithful_semantics.rs    quirks pinned + panic freedom
  strict_conformance.rs    JSONTestSuite-style + serde_json differential
benches/
  scan.rs   parse.rs   query.rs
examples/
  typestate.rs   runnable unwrap-free demo
docs/
  RESULTS.md   UNWRAP_FREE.md
```

## Running

```bash
cargo test                       # 73 tests, incl. 60k differential cases
cargo bench --bench scan
cargo bench --bench parse
cargo bench --bench query
cargo clippy --lib               # enforces panic freedom
cargo run --example typestate
```

Toolchain: `rustc 1.95.0`. Nothing depends on a feature newer than 1.61.

## Attribution

The algorithms descend from **simdjson** (Apache-2.0; Lemire, Langdale,
Keiser) via Vela's tier 2, and **yyjson** (MIT; YaoYuan/ibireme) via Vela's
tier 3, as recorded in
`bootstrap/stage2/src/stdlib/encoding/json/AUTHORS`. No C/C++ code was
copied — only strategies.
