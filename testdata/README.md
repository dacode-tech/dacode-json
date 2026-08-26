# Vendored conformance test data

Copied here so the test suite is self-contained — nothing in this project
reads from the Vela tree. Obtained via yyjson's vendored copy
(`3py/yyjson/test/data`), which documents the same provenance.

| directory | source | licence | contents |
|---|---|---|---|
| `test_parsing/` | [JSONTestSuite](https://github.com/nst/JSONTestSuite) (Nicolas Seriot) | MIT | 319 files: 95 `y_`, 189 `n_`, 35 `i_` |
| `test_transform/` | JSONTestSuite | MIT | 22 files of structures parsers read differently |
| `test_checker/` | [JSON_checker](http://www.json.org/JSON_checker/) (Douglas Crockford) | Public domain / MIT-style | 36 `pass*`/`fail*` files |
| `test_encoding/` | yyjson | MIT | 10 files: UTF-8/16/32, with and without BOM |
| `num/` | yyjson | MIT | number edge cases, one literal per line |

## Naming convention

* `y_` — **must** be accepted
* `n_` — **must** be rejected
* `i_` — implementation-defined; a conformant parser may do either

`fail01.json` and `fail18.json` are absent from `test_checker` upstream:
the first was relaxed by RFC 7159 (a bare scalar is a valid document) and
the second tests a depth limit RFC 8259 does not specify.

Consumed by `tests/conformance_suite.rs` (the corpora) and
`tests/edge_cases.rs` (`test_encoding/`).

`tests/edge_cases.rs` additionally covers the categories yyjson's and
simdjson's *unit tests* exercise, which no corpus of `.json` files reaches:
encoding rejection, whitespace definition, incremental truncation, integer
and float boundaries, escape and surrogate handling, structural limits and
round-trip idempotence. Those are written from the categories rather than
translated — both upstream suites are table-driven C macros whose
assertions do not carry over.
