# A YaFF-style zero-copy wire format for JSON

> *"There is a near zero-copy wire format for Protobuf: yandex/yaff. How
> feasible is something like that for JSON in Rust?"*

**Very feasible, and Vela's tier-3 pool is already 80% of the design.** The
prototype is `src/flat.rs` (`jsonflat`); it is 700 lines, has O(1) open and
O(log n) field lookup, and reads a field 520 000× faster than re-parsing the
JSON.

The interesting part is not that it works. It is what the numbers say about
*when* it is worth it, and about the one thing JSON cannot have that YaFF
does: a schema.

---

## 1. Why the tier-3 pool is nearly there already

YaFF's core claim is an mmap-compatible layout read through a proto-like
interface with no parsing step. FlatBuffers is the same idea. What such a
format needs:

| requirement | tier-3 pool | gap |
|---|---|---|
| flat array of fixed-size records | ✅ 16-byte nodes | — |
| addressing by index, not pointer | ✅ | — |
| contiguous, no fixups on load | ✅ pre-order | — |
| **self-contained** | ❌ strings are `(offset, len)` into a *separate* input | must move strings in |
| no decoding at read time | ❌ escapes decoded per read | decode once at build |
| bounds-safe on hostile input | ❌ no validation at all | add a header + checks |
| random field access | ❌ linear scan | sort keys, binary search |

Only one of those is structural. The rest are refinements. This is worth
saying plainly: **Vela already has a zero-copy JSON format and does not know
it.** Making `json_pool_parse_ws` write strings into the workspace instead of
referencing the input would turn its output into a shippable buffer.

### What `jsonflat` changes

```text
offset  size  field
     0     4  magic "JFL1"
     4     2  version
     6     2  flags        (bit 0: object children sorted by key)
     8     4  node_count
    12     4  root index
    16    12  section offsets (nodes / numbers / strings)
    28     4  total length
    32   ...  nodes    — 8 bytes each
   ...   ...  numbers  — 8 bytes each (i64 or f64 bits)
   ...   ...  strings  — UTF-8, already unescaped, optionally interned
```

Node, 8 bytes instead of the pool's 16:

```text
tag:     u32   bits 0..3  kind (10 kinds)
               bits 4..31 aux (child count or string length, max 2^28)
payload: u32   inline i32, table index, blob offset, or first-child index
```

Three deliberate departures from the pool:

1. **Children occupy a contiguous fixed-width run.** The pool stores
   variable-width subtrees, so `array[i]` costs a `skip_subtree` walk per
   preceding element — O(n). Here it is `first_child + i`, one
   multiply-add. Profiling shows `skip_subtree` is 7.6% of self time during
   struct deserialization from the pool; in `jsonflat` it does not exist.
2. **Small integers inline.** `i32`-range values live in the payload; only
   wider ones spill to the numbers table. JSON integers are overwhelmingly
   small.
3. **Object children sorted by key at build time**, so `get` binary
   searches. Optional (`Builder::sort_keys(false)`) because it discards
   document key order.

---

## 2. Measured

Apple M2 Max, `rustc 1.95.0`, 1 MiB `records` corpus, `benches/zerocopy.rs`.

### Opening a buffer

| | time | what it does |
|---|---|---|
| `rkyv` access, unchecked | **0.57 ns** | pointer cast |
| `jsonflat` header only | **1.53 ns** | magic, version, section bounds |
| `rkyv` access, checked | 370 µs | full structural validation |
| `jsonflat` `validate_deep` | 780 µs | every node, ref and string |
| Vela pool parse | 1.32 ms | full parse |
| `serde_json` → `Value` | 7.07 ms | full parse |

O(1) versus O(n) is the entire point. Both zero-copy formats offer a
trusted-input path that is a constant and an untrusted-input path that is a
cheap linear pass — and even the untrusted path beats parsing by 9–18×.

### Reading

| | read one field | read all 10 000 records |
|---|---|---|
| `rkyv` | **1.02 ns** | **4.88 µs** |
| `jsonflat` | 13.5 ns | 111 µs |
| Vela pool (re-parse) | 1.32 ms | 1.70 ms |
| `serde_json` (re-parse) | 7.05 ms | 7.37 ms |

**`jsonflat` reads one field 522 000× faster than `serde_json` re-parses**,
and 98 000× faster than the tier-3 pool. That number is silly, and it is the
correct number: one is O(1), the other is O(document).

### Amortising over N reads

Total time for N reads of one field from a 1 MiB document:

| N | `jsonflat` | Vela pool | `serde_json` |
|---|---|---|---|
| 1 | 12 ns | 1.32 ms | 7.08 ms |
| 4 | 49 ns | 5.30 ms | 28.4 ms |
| 16 | 190 ns | 21.1 ms | 113 ms |
| 64 | 745 ns | 84.8 ms | 455 ms |

The crossover is at **N = 1**. There is no read count at which re-parsing
wins, because `jsonflat`'s open is a constant. The only question is whether
you can pay the encode cost and the size.

### Encoding — where the cost moves

| | time |
|---|---|
| `rkyv` from structs | **401 µs** |
| `serde_json` from structs | 1.31 ms |
| strict parse alone (of the JSON) | 2.46 ms |
| `jsonflat`, no interning | 6.33 ms |
| `jsonflat`, intern keys | 6.73 ms |
| `jsonflat`, intern everything | 7.08 ms |
| `jsonflat`, keys unsorted | 6.21 ms |

Building `jsonflat` from JSON costs 6.3 ms, of which 2.5 ms is parsing the
JSON in the first place. The build itself is ~3.9 ms and is dominated by
`write_into` (10.9%), `unescape` (9.8%) and `put_str` (9.1%) — see
`docs/PROFILING.md`. Key sorting costs ~8%; interning keys costs ~6%.

This is the honest shape of every zero-copy format: **you move work from
every read to the single write.** It pays whenever writes are rarer than
reads, which is the cache/RPC/mmap case YaFF targets.

### Size — the price

Buffer size relative to the source JSON, 1 MiB corpora:

| corpus | no interning | intern keys | intern all | `rkyv` |
|---|---|---|---|---|
| `records` | 1.76× | 1.48× | **1.30×** | 0.59× |
| `int_array` | 1.64× | 1.64× | 1.64× | — |
| `strings` | 0.97× | 0.97× | **0.97×** | — |
| `geo_int` | 2.32× | 2.16× | **2.10×** | — |
| `geo_float` | 1.74× | 1.66× | **1.63×** | — |

For reference, the tier-3 pool is **2.61×** on `records` — *and* it still
needs the original input alongside it, so the real figure is 3.61×.
`jsonflat` at 1.30× is less than half that.

Reading the table:

* **`strings` is smaller than the JSON** (0.97×). Escapes are decoded once,
  so `\u00e9` becomes two bytes instead of six, and that outweighs the node
  overhead.
* **`int_array` is the worst case** (1.64×). A four-character JSON integer
  becomes an eight-byte node. Nothing to be done without a columnar layout.
* **Interning keys is worth 16% on `records`** and nothing on `int_array`,
  exactly as expected — it only helps when keys repeat.

---

## 3. Why `rkyv` wins, and what that costs

`rkyv` is 22× faster at reading and 2.2× smaller. It is not a better
implementation of the same idea; it is a different idea:

| | `rkyv` | `jsonflat` |
|---|---|---|
| schema | compile-time Rust type | none |
| field access | fixed struct offset, resolved by the compiler | binary search over stored key strings |
| keys in the buffer | **not stored at all** | stored (interned) |
| type tags | none | 4 bits per node |
| accepts unknown shapes | no | yes |
| readable without the schema | no | yes |

Both differences trace to the same root: **`rkyv` knows the shape, so it
stores only the data.** `jsonflat` stores the shape too, because JSON is
self-describing and a consumer may not know what is coming.

That is not a defect to be optimised away, it is the specification. The
comparison worth drawing is not `jsonflat` vs `rkyv` but `jsonflat` vs *JSON*
— and against JSON, one field costs 13.5 ns instead of 7.05 ms.

YaFF sits in between: it has a `.proto` schema like `rkyv`, but keeps
Protobuf's field numbers and optionality, so it can skip unknown fields and
convert back to Protobuf. A `jsonflat` with an optional schema — key strings
replaced by field IDs when the writer and reader agree — would land in the
same place. That is the obvious next step and is not implemented here.

---

## 4. Is it worth doing for Vela?

**Yes, and it is a small change.** Concretely:

1. **Make `json_pool_parse_ws` able to write strings into the workspace.**
   That single change makes the pool self-contained, which is the only
   structural gap. Everything after this is optional.
2. **Add a header and a `validate_deep`.** Tier 3 currently does no
   validation whatsoever; a buffer read from disk or a socket is a memory
   safety problem waiting to happen. `jsonflat`'s
   `every_single_byte_mutation_is_safe` and `random_buffers_are_safe` tests
   (40 000 hostile buffers) exist because the buffer *is* the data
   structure.
3. **Lay container children out at fixed width.** This is worth doing
   regardless of the wire format: it makes `json_pool_array_get` O(1)
   instead of O(n) and removes `pool_skip_subtree` from the hot path, which
   profiling puts at 7.6% of struct deserialization.
4. **Halve the node** from 16 bytes to 8. The pool's 16-byte node is mostly
   empty; 4 bits of kind and 28 bits of aux cover every real document.
5. **Intern keys.** 16% on record-shaped data for a hash lookup per key.

The natural extension after that is a **columnar layout for arrays of
homogeneous objects** — store `records` as seven parallel arrays instead of
eleven nodes plus repeated keys per record. That is precisely what YaFF has
on its own roadmap ("Columnar Layout — compact representation for large
repeated fields"), and for the `records` corpus it should bring the size
below 1.0× and the read cost close to `rkyv`'s.

### When not to bother

If the JSON is parsed once and thrown away, this whole document is
irrelevant — you want a streaming deserializer, and `docs/RESULTS.md` §5
shows `sonic_rs::get` doing a point query in 54 ns without building
anything. Zero-copy formats are for data that is *stored*: caches, mmapped
files, RPC payloads read by several consumers, anything where the write
happens once and reads happen many times.
