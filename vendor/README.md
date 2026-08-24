# Vendored C/C++ baselines

Copied here so this project builds and benchmarks **without any reference to
the Vela tree**. Compiled from source at `-O3` by `build.rs`, only when the
`cbench` feature is enabled.

| directory | library | version | licence |
|---|---|---|---|
| `yyjson/` | [yyjson](https://github.com/ibireme/yyjson) | 0.12.0 | MIT (`yyjson/LICENSE`) |
| `simdjson/` | [simdjson](https://github.com/simdjson/simdjson) | 4.6.1 | Apache-2.0 (`simdjson/LICENSE`) |

`shim.c` and `shim_simdjson.cpp` are ours. Most of yyjson's API is
`static inline` in the header and therefore has no linkable symbols, so C
wrappers are required regardless; both shims expose **whole workloads**
rather than per-field accessors, so no FFI boundary is crossed inside a
timed loop.

These are the libraries Vela's tiers were modelled on —
`bootstrap/stage2/src/stdlib/encoding/json/AUTHORS` credits yyjson for
tier 3 and simdjson for tier 2. See `docs/CBASELINE.md` for results.

Neither is a runtime dependency: `cargo build`, `cargo test` and every
benchmark except `cbaseline` work without a C or C++ compiler.
