# The four tiers, head to head

Vela ships four interchangeable JSON backends and picks one at build time
with `--define json_tier=N` (`BUILD.bazel:99-150`). Exactly one is compiled
per link. All four are now ported (`src/tiers/`) behind the common interface
`docs/stage2/P1_2_JSON_TIERS.md:109-141` specifies, so they can be measured
against each other on identical work.

| tier | algorithm | Vela LOC | source |
|---|---|---|---|
| 0 | scalar detection only; containers stubbed | 289 | `tier0/parse.vl` |
| 1 | recursive descent over raw bytes — **Vela's default** | 590 | `tier1/parse.vl` |
| 2 | structural index + tape DOM | 1619 | `tier2/*.vl` |
| 3 | flat node pool | 2063 | `tier3/*.vl` |

Machine: Apple M2 Max, `rustc 1.95.0`, `opt-level=3`, `lto="fat"`.
Reproduce with `cargo bench --bench tiers`.

---

## The headline

**Vela's default is correct, and the reasoning in its design docs is not.**

`P1_2_JSON_TIERS.md` frames tier 1 as the baseline and tiers 2–3 as the
performance tiers, with tier 3 "deferred until SIMD is available". The
measurements say tier 1 is the fastest tier for the operation applications
actually perform — pulling a field out of a document — at **every document
size tested**, from 8 keys to 16 384.

### One field from a flat object (worst case: the last key)

| document | tier 1 | tier 2 | tier 3 | tier 3 + workspace | serde_json |
|---|---|---|---|---|---|
| 8 keys | **145 ns** | 420 ns | 453 ns | 200 ns | 479 ns |
| 64 keys | **1.13 µs** | 1.80 µs | 2.50 µs | 1.48 µs | 5.43 µs |
| 1 024 keys | **15.4 µs** | 24.7 µs | 35.2 µs | 22.3 µs | 120 µs |
| 16 384 keys | **251 µs** | 379 µs | 569 µs | 368 µs | 2.50 ms |

Tier 1 is 1.5–2.3× faster than the indexed tiers and 3.3–10× faster than
`serde_json`, and the ratio does not improve with size. Building an index
you use once is pure overhead, and one lookup is the common case.

Note tier 1 also *allocates nothing*. It reads the input and returns a
subslice of it.

---

## Where the indexed tiers actually win

The crossover is a function of **queries per document**, not document size.
256-key object, reading *k* fields:

| k | tier 1 | tier 2 (per call) | tier 2 (cached) | tier 3 (workspace) |
|---|---|---|---|---|
| 1 | **16 ns** | 3.95 µs | 3.39 µs | 3.72 µs |
| 4 | **511 ns** | 16.4 µs | 3.69 µs | 3.90 µs |
| 16 | 9.01 µs | 68.7 µs | 7.68 µs | **5.95 µs** |
| 64 | 146 µs | 322 µs | 68.1 µs | **35.6 µs** |

(At k = 1 the key is the *first* one, so tier 1 finds it immediately — hence
16 ns. The table above uses the last key for the pessimistic view.)

**Crossover is around k ≈ 16.** Below that, scan. Above it, index — and by
k = 64 tier 3 with a reused workspace is 4.1× faster than tier 1.

The middle column is the important one. **Tier 2 as shipped never gets
there**, because `json_object_get`, `json_object_count`, `json_object_keys`
and `json_array_count` each begin with `json_tape_build(input)`
(`tier2/parse.vl:347`, `:371`, `:392`, `:452`) — a fresh structural scan and
a fresh tape, per call. At k = 64 that makes tier 2 **4.7× slower than
having no index at all**.

Caching the same tape (`Tier2Cached`, ~40 lines, no algorithmic change)
turns a 2.2× loss into a 2.1× win. That gap is the cost of the API mistake,
and it is larger than any difference between the tape and pool designs.

---

## Whole-document operations

### `array_count`, 10 000-record array

| document | tier 0 | tier 1 | tier 2 | tier 3 | tier 3 + workspace |
|---|---|---|---|---|---|
| 1 KB | 0.91 ns¹ | **7.79 µs** | 9.42 µs | 8.21 µs | 7.89 µs |
| 64 KB | 0.88 ns¹ | 87.0 µs | 105 µs | 87.0 µs | **83.9 µs** |
| 1 MB | 0.98 ns¹ | 1.43 ms | 1.90 ms | 1.62 ms | **1.33 ms** |

¹ Tier 0 returns a constant 0. It is not counting anything.

Even here — where tiers 2 and 3 get the count from a header field in O(1)
after parsing, and tier 1 has to walk and skip every element — tier 1 is
within 8% of the best. Skipping a value is cheap; building a node for it is
not.

### `validate`

| document | tier 1 | tier 2 | tier 3 | `strict` (RFC 8259) | serde_json |
|---|---|---|---|---|---|
| 1 KB | **7.66 µs** | 13.2 µs | 7.69 µs | 14.7 µs | 42.7 µs |
| 64 KB | 85.6 µs | 142 µs | **85.4 µs** | 153 µs | 463 µs |
| 1 MB | 1.40 ms | 2.39 ms | **1.40 ms** | 2.45 ms | 8.33 ms |

These numbers are close to meaningless on their own, because the tiers are
not validating. See below.

### DOM construction — tape vs pool, the only fair comparison

| document | tier 2 tape, fresh | tape, reused | tier 3 pool, fresh | pool, reused |
|---|---|---|---|---|
| 1 KB | 9.26 µs | 8.59 µs | 8.18 µs | **7.96 µs** |
| 64 KB | 100 µs | 89.5 µs | 90.4 µs | **83.3 µs** |
| 1 MB | 1.79 ms | 1.42 ms | 1.62 ms | **1.32 ms** |

The pool beats the tape by a consistent **7–8%**, and reusing buffers is
worth **19–20%** to both. So tier 3's representation is modestly better than
tier 2's, and both are dominated by allocation behaviour.

Why the pool wins: the tape emits a matched open/close pair per container
and back-patches the link (`tape.vl:216`), so a container costs two entries
and a second pass over the opener. The pool emits one node carrying a child
count. The tape also has to sniff for scalars at three separate places
(after `[`, after `:`, after `,`), because scalars produce no structural
character — `tape.vl:235`, `:273`, `:286`.

---

## `json_validate` does not validate

This is the most serious finding of the tier comparison.

`tier1/parse.vl:403` documents `json_validate` as *"Validate entire JSON
input (recursive). Returns true if valid RFC 8259"*. It accepts:

```text
{        [        [1,2        {"a":1        {"a"        txxx        [1,]
}        ]        [,]         nxxx          fxxxx
```

The mechanism: the skip functions return the end-of-input position when they
run off the end (`skip_object` falls out of its loop and returns `p`,
`tier1/parse.vl:80`), and `validate` only asks whether that position equals
`len`. **Truncated input therefore always validates**, because running out
of bytes lands exactly on `len`. Separately, `skip_value` checks only the
first byte of `true`/`false`/`null` plus that the length fits
(`tier1/parse.vl:39-43`), so `txxx` passes.

Tier 3 shares the implementation verbatim. Tier 2 adds a bracket-balance
check on the structural index (`tier2/parse.vl:336`), which catches the
truncated-container cases but not the bad literals or stray commas.

Measured agreement with `serde_json` over 20 000 mutated documents
(`tests/tier_contract.rs::validate_accuracy_against_serde_json`):

| tier | agrees | note |
|---|---|---|
| 0 | 36.4% | returns `false` unconditionally, so right only on invalid input |
| 1 | 79.1% | |
| 2 | **86.6%** | the bracket check is worth 7.5 points |
| 3 | 79.1% | identical to tier 1 |

For comparison, `strict::StrictParser` in this crate agrees 100% and costs
1.75× tier 1 (2.45 ms vs 1.40 ms on 1 MB) — still 3.4× faster than
`serde_json`.

---

## The "common interface" is not common

`P1_2_JSON_TIERS.md:110` says all tiers export the same functions "so
consumers don't need to know which tier is active". They do need to know.

Tier 0 stubs every container operation (`tier0/parse.vl:171-197`) and the
stubs return plausible values, not errors:

```rust
Tier0::object_count(br#"{"a":1,"b":2}"#)  // 0
Tier0::object_get(br#"{"a":1}"#, "a")     // None
Tier0::array_count(b"[1,2,3]")            // 0
Tier0::validate(br#"{"a":1}"#)            // false
Tier0::skip_value(input, 0)               // 0 — never advances
```

That last one is the worst: `json_skip_value` returns `pos` unchanged, so
any caller looping until it advances hangs. Code written against the
contract and linked against tier 0 does not fail, it silently produces
wrong answers.

The port surfaces this as `JsonTier::HANDLES_CONTAINERS`, a compile-time
constant, so a generic consumer can at least be made to acknowledge it.

Where the contract does hold — tiers 1, 2 and 3 on valid JSON — it holds
exactly. `tests/tier_contract.rs` checks all three against each other and
against `serde_json` on every corpus, 20 000 random documents, and 20 000
random scalars, plus 20 000 arbitrary byte strings for panic freedom.

---

## Other things the port turned up

**`json_array_get` in tier 2 does not use the tape.** `tier2/parse.vl:417`
is a byte scan character-for-character identical to tier 1's. So the one
array operation you would most want indexed is not.

**Tier 2's SIMD is off by default.** `structural_gate.vl:4-7`: *"Default:
SIMD off (scalar). Call `json_simd_accel_enable()` to switch."* The tape
builder goes through `json_structural_scan_gated` (`tape.vl:301`), so stock
tier 2 runs the scalar scanner. Anyone benchmarking tier 2 without calling
the enable function is measuring the wrong thing.

**Tiers disagree about numbers.** Tiers 0–2 stop at the first non-digit, so
`3.14` parses as `3`. Tier 3 *skips* `.`, `e`, `E`, `+`, `-` and keeps
accumulating, so `3.14` parses as `314`
(`runtime/json_pool_write.ll:771-784`). Both are wrong; switching tiers
changes the answer.

**Tier 1 has unbounded recursion.** `json_skip_value` → `json_skip_object` →
`json_skip_value` has no depth limit, so nested input from an untrusted
source exhausts the stack. The port caps it at 256, matching tier 3's
`IDX_STACK_MAX`.

**A bug in this port, found by benchmarking.** `unescape_vela_lossy` pushed
each byte as a `char`, which turns `é` (`C3 A9`) into `Ã©`. Vela appends
`input.slice(i, i+1)` — one raw byte — so multi-byte UTF-8 passes through
intact. Fixed, and bulk-copying the runs between escapes made it 1.7×
faster (41 µs → 24 µs on a 30 KB string).

---

## Recommendations

1. **Keep tier 1 as the default.** It is the fastest tier for one-shot field
   access at every size, it allocates nothing, and it is the smallest. The
   design docs' framing of it as the low-capability fallback is not
   supported by measurement.

2. **Fix tier 2's API before optimising anything in it.** Rebuilding the
   tape per call makes tier 2 slower than having no tape. One cached
   builder turns a 2.2× loss into a 2.1× win at k = 64. This is the single
   highest-value change in the JSON stack.

3. **Document tier selection by query count, not document size.** The rule
   is "index if you will read more than ~16 fields from the same document";
   size barely matters.

4. **Either fix `json_validate` or rename it.** It agrees with a real
   parser 79% of the time and accepts every truncated document. Callers
   using it as a gate on untrusted input are not protected. A conformant
   implementation costs 1.75× and is still 3.4× faster than `serde_json`.

5. **Make tier 0's stubs impossible to call by accident.** A build-time
   error, or a distinct trait, or anything other than returning `0` from
   `object_count`.

6. **Turn tier 2's SIMD gate on by default,** or delete it. A runtime flag
   that defaults to the slow path and is documented in a comment is a
   benchmark trap.
