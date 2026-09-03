# Can `.unwrap()` be avoided entirely?

The question was:

> As I understand, `.unwrap()` in Rust could be avoided. If main, tests and
> closures return `Result`, so `?` applies; pushing invariants into types.
> So `.build()` returns `T`, so failing unwrap is a compile error. But I'm
> not sure how feasible that is to implement.

**Short answer: yes, and it is much cheaper than it sounds — but the two
halves of the question have very different costs.**

* Replacing `unwrap` with `?` is free. It is a lint plus mechanical edits.
* Pushing invariants into types is not free, but the price is one type
  parameter per required field, paid once by the library author, and it
  genuinely converts a class of runtime panics into compile errors.

The parts that resist are narrow and well understood: array indexing where
the bound is obvious to you but not to the compiler, integer overflow, and
`Vec` capacity invariants around `unsafe`.

This document is backed by working code in this crate rather than by
assertion. See `examples/typestate.rs` (runnable) and the lint block at the
top of `src/lib.rs`.

---

## 1. Evidence from this crate

`dacodec` is a parser — the most panic-prone kind of code there is — and it
has zero panicking constructs in library code. This is enforced, not claimed:

```rust
// src/lib.rs, and the same block in each of src/bin/*.rs
#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unreachable,
    clippy::indexing_slicing,
    clippy::unwrap_in_result,
    clippy::exit
)]
```

`cargo clippy --all-features --all-targets` passes. `indexing_slicing` is
the one that actually bites: it bans `slice[i]`, which is where parsers
normally panic. `unreachable` is the one that bites *later* — it is where
an "obviously impossible" branch goes to assert a coupling between two
functions instead of proving it, and there was exactly one, in
`memprofile`.

The binaries were the gap. They are not library code and not tests, so
nothing linted them, and they had 38 `.expect()` calls between them.
Each `main` now returns `Result`, which costs nothing: `main` returning
`Err(e)` prints `Error: {e:?}` and exits 1 without unwinding. Two of
them wrap the error in a newtype whose `Debug` delegates to `Display`,
because the `Debug` of a `String` in a `Box<dyn Error>` prints with
quotes and reads like a bug report. `src/bin/fuzz.rs` goes further and
makes its *findings* the error type, so a fuzzer that found something
exits non-zero by returning the report.

The property is also tested, not just linted:

* `faithful_semantics.rs::arbitrary_bytes_never_panic` — 30 000 random byte
  strings up to 512 bytes, fully traversed
* `faithful_semantics.rs::truncations_of_valid_documents_never_panic` —
  every prefix of a valid document
* `faithful_semantics.rs::structurally_hostile_inputs` — 10 000 repetitions
  of `[`, `]`, `{`, `}`, `"`, `\`, `,`, `:`

Cost of doing this: **essentially zero.** The parser is faster than
`serde_json` and competitive with simd-json. Bounds checks are not what makes
JSON parsing slow; the hot loop reads 16 bytes at a time through
`<&[u8; 16]>::try_from`, which is one check per chunk rather than one per
byte.

### What replaced the panics

| Instead of | Use | Where |
|---|---|---|
| `slice[i]` | `slice.get(i)` + `let else` | everywhere |
| `slice[a..b]` | `slice.get(a..b)?` | `query.rs:as_raw_str` |
| `v.unwrap()` in a hot loop | `unwrap_or(default)` with a documented default | `builder.rs:pos_at` |
| `iter.next().unwrap()` | `let Some(x) = ... else { return }` | `builder.rs:close_container` |
| `Vec` index in a SIMD write | `spare_capacity_mut().get_mut(..n)` + `set_len` | `scan/mod.rs:write_bits` |

The last one is the interesting case. Vela's
`__simd_json_write_positions` loads the count once and stores it once, and
reproducing that means writing into uninitialised capacity. The safe
formulation:

```rust
let Some(slots) = self.positions.spare_capacity_mut().get_mut(..n) else {
    debug_assert!(false, "reserve({n}) did not yield {n} spare slots");
    return;
};
for slot in slots.iter_mut() { slot.write(...); }
// SAFETY: the loop initialised exactly `n` elements ...
unsafe { self.positions.set_len(len + n) };
```

`reserve(n)` guarantees the `else` arm is unreachable, but taking it is
*harmless* — it skips the `set_len` and leaves the vector untouched. That is
the pattern worth internalising: when you cannot convince the compiler, make
the impossible branch **safe** rather than `unreachable!()`.

---

## 2. `main`, tests and closures

All three work, with one wrinkle.

**`main`** — anything implementing `Termination`:

```rust
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cfg = load_config()?;
    run(cfg)?;
    Ok(())
}
```

Downside: the error is printed with `Debug`, not `Display`, so
`Box<dyn Error>` prints its inner `Debug`. For a real CLI, catch it:

```rust
fn main() -> std::process::ExitCode {
    match real_main() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => { eprintln!("error: {e}"); std::process::ExitCode::FAILURE }
    }
}
```

**Tests** — `#[test] fn t() -> Result<(), E>` works; the test fails if `Err`.
In practice this crate does *not* use it, because `assert_eq!` is a `panic!`
anyway and test failure is exactly what a panic is for. **Panic-freedom is a
property of library code, not of tests.** Contorting test code to avoid
`unwrap` buys nothing and costs readability. The lints above are
`--lib`-scoped for this reason.

**Closures** — the wrinkle. `?` inside a closure returns from *the closure*,
not the enclosing function. So the closure must return `Result` and the
caller must consume it:

```rust
// Does NOT propagate out of `parse_all`:
inputs.iter().map(|s| s.parse::<u16>().unwrap())

// Does, via the FromIterator<Result<..>> impl:
inputs.iter()
      .map(|s| -> Result<Port, AppError> {
          let raw = s.parse().map_err(|_| AppError::BadPort(0))?;
          Port::new(raw).ok_or(AppError::BadPort(raw))
      })
      .collect::<Result<Vec<_>, _>>()
```

The tools:

| Need | Use |
|---|---|
| `Vec<Result<T, E>>` → `Result<Vec<T>, E>` | `.collect::<Result<Vec<_>, _>>()` |
| fold with a fallible step | `.try_fold(init, |acc, x| ...)` |
| side effects, fallible | `.try_for_each(...)` |
| skip failures instead of propagating | `.filter_map(|x| f(x).ok())` |
| first success | `.find_map(...)` |

This crate uses `filter_map` in `query.rs` (a missing field is not an error,
it is an absence) and `let else` everywhere else.

---

## 3. Typestate builders: `.build() -> T`

This is the part the question was least sure about. It works, and it is less
machinery than expected. `examples/typestate.rs` implements all of it and
runs.

### Technique 1 — required arguments in the constructor

Free. Solves most real builders. If `name` and `port` are mandatory, they are
parameters of `new`, and `build` has nothing to check:

```rust
SimpleBuilder::new("edge", 8080).retries(5).build()  // -> SimpleConfig
```

Reach for anything fancier only when required fields cannot go in the
constructor: too many of them, arbitrary arrival order, or the builder is
filled in by different subsystems.

### Technique 2 — typestate with marker parameters

Track "is this field set?" in the type:

```rust
struct Set;
struct Unset;

struct ConnBuilder<HOST, PORT> {
    host: Option<String>,
    port: Option<u16>,
    timeout_ms: u32,
    _marker: PhantomData<(HOST, PORT)>,
}

impl<HOST, PORT> ConnBuilder<HOST, PORT> {
    fn host(self, h: impl Into<String>) -> ConnBuilder<Set, PORT> { ... }
    fn port(self, p: u16)               -> ConnBuilder<HOST, Set> { ... }
    fn timeout_ms(mut self, ms: u32)    -> Self                   { ... }  // optional
}

// `build` exists only here:
impl ConnBuilder<Set, Set> {
    fn build(self) -> Connection { ... }
}
```

Setters are order-independent, and the failure mode is:

```
error[E0599]: no method named `build` found for struct `ConnBuilder<Set, Unset>`
```

Verified — that is the actual compiler output from this repository.

**The catch:** inside `build`, the `Option`s are statically `Some`, but the
compiler cannot see that through `PhantomData`. So you have hidden an
`unwrap` in the implementation while removing it from the API. That is a real
improvement (one auditable site instead of every call site) but it is not
elimination.

### Technique 2b — let the type parameter *be* the storage

This removes the internal `unwrap` too. Use `()` for unset and the real type
for set:

```rust
struct Conn2<H, P> { host: H, port: P, timeout_ms: u32 }

impl Conn2<(), ()> { fn new() -> Self { Conn2 { host: (), port: (), .. } } }

impl<H, P> Conn2<H, P> {
    fn host(self, h: impl Into<String>) -> Conn2<String, P> { ... }
    fn port(self, p: u16)               -> Conn2<H, u16>    { ... }
}

impl Conn2<String, u16> {
    fn build(self) -> Connection {
        Connection { host: self.host, port: self.port, .. }  // no Option, no unwrap
    }
}
```

`()` is zero-sized, so the unset builder is strictly smaller than the
`Option` version. **This is the version to use.** It is the same amount of
code and strictly better.

Cost either way: one type parameter per required field, and setters must
reconstruct the struct rather than mutate in place. For 2–4 required fields
this is fine. For 10, generate it — `typed-builder` and `bon` do exactly
this, including the `T`-not-`Result` return.

### Technique 3 — refinement types

The most valuable of the three, and the cheapest:

```rust
#[derive(Clone, Copy)]
struct Port(u16);

impl Port {
    fn new(v: u16) -> Option<Self> { (v >= 1024).then_some(Port(v)) }
    fn get(self) -> u16 { self.0 }
}

fn bind(port: Port) -> String { ... }   // total. cannot fail. nothing to unwrap.
```

Validate once at the boundary; after that the invariant is a *fact the
compiler enforces*, and every downstream function is total. This is "parse,
don't validate", and it is where the real win is — not in builders.

This crate uses it in two places, both replacing a Vela footgun:

* **`Doc<'a>` bundles `(input, pool)`.** Vela's
  `json_pool_object_get(input, pool, key)` takes them as loose arguments with
  nothing tying them together; passing the wrong string silently returns
  garbage. `Doc` makes the pairing a type.

* **`Workspace::parse(&mut self) -> Doc<'_>`.** Vela documents "the returned
  pool pointer is invalidated by the next call" in a *comment*
  (`parse_indexed.vl:693-694`). Here it is a borrow, so the misuse is a
  compile error — proved by a `compile_fail` doctest in `src/workspace.rs`:

  ```rust
  let first  = ws.parse(br#"{"a":1}"#);
  let second = ws.parse(br#"{"b":2}"#);  // error: cannot borrow `ws` as mutable
  let _ = first.root();                  //        more than once at a time
  ```

  Zero runtime cost, zero extra code — it falls out of taking `&mut self`.

---

## 4. Where it genuinely does not work

Being straight about the limits.

### Runtime input is fallible, and that is correct

Parsing, IO, and network data cannot be made infallible by any type. The goal
is not to eliminate `Result` — it is to eliminate `unwrap`. Convert at the
edge with `?`, into a domain type, once.

### Indexing where you know the bound and the compiler does not

The common real case. Options, in order of preference:

1. **Restructure to iterators.** `for (i, x) in xs.iter().enumerate()` has no
   bounds check at all. This is usually possible and usually also faster.
2. **`get()` + `let else` with a safe fallback.** What this crate does.
3. **Fixed-size arrays.** `<&[u8; 16]>::try_from(slice)` checks once, then
   all 16 accesses are free. This is why the SIMD path costs nothing.
4. **Accept it.** A `debug_assert!` plus a benign fallback is honest. An
   `unreachable!()` is not — it is an `unwrap` wearing a hat.

### Integer overflow

`clippy::arithmetic_side_effects` catches it but is genuinely painful; every
`i + 1` needs attention. This crate does not enable it, and instead uses
explicit `wrapping_*`/`saturating_*`/`checked_*` where overflow is reachable
(`scalar.rs` reproduces Vela's wrapping number accumulator deliberately).
This is the one place where "pragmatic" is the right call.

### Mutex poisoning

`lock().unwrap()` is the most common `unwrap` in real Rust. Fixes: use
`parking_lot` (no poisoning), or handle it with
`.unwrap_or_else(|e| e.into_inner())` if you know your critical section is
panic-free — which, given everything above, it now is.

### Allocation failure

`Vec::push` aborts on OOM. Nothing to do about it on stable without
`try_reserve` everywhere, which is only worth it for kernel or allocator code.

---

## 5. Recommendation

For a compiler and its standard library — which is what Vela is — this is
worth doing, and the order matters:

1. **Turn on the lints for library crates only.** `unwrap_used`,
   `expect_used`, `panic`, `indexing_slicing`. Leave tests alone. This is a
   day of mechanical work and catches the majority of the risk.
2. **Refinement types at every boundary.** Highest value per line of code in
   the whole list. Validate once, then it is a compiler fact.
3. **Typestate builders (technique 2b) where required fields cannot go in
   the constructor.** Not before.
4. **Skip `arithmetic_side_effects`** unless writing a runtime or an
   allocator. The signal-to-noise ratio is bad.

The evidence that this is affordable is the crate you are reading: a JSON
parser with no panicking construct in library code, faster than `serde_json`
on every corpus and within 1.8× of the fastest SIMD parser in the ecosystem.
