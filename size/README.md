# `size/` — the code-size harness

A separate cargo project, not a member of the parent workspace: it needs
its own `[profile.release]` (`panic = "abort"`, `opt-level = "z"`) and
must not inherit the parent's dev-dependencies, which unwind.

Run it with `tools/size.sh`. Results and method: `docs/SIZE.md`.

```
size/
  src/lib.rs          the bare-metal shim: input, panic handler, bump
                      allocator. Shared, so the only difference between
                      two binaries is the JSON library.
  src/bin/floor.rs    the shim alone — the number subtracted from the rest
  src/bin/pull.rs     dacodec::pull, no allocator
  src/bin/pullbytes.rs  the same, stopping before number conversion
  src/bin/write.rs    dacodec::write, no allocator
  src/bin/direct.rs   dacodec through serde, with an allocator
  src/bin/serdejson.rs  serde_json, no_std + alloc
  cbase/              the C contenders and their freestanding libc shims
  link.x              a minimal Cortex-M layout, so there is something to
                      link to
```

## Nothing here runs

The binaries exist to be **linked**. An rlib holds generic code
uninstantiated, so a library has no size until monomorphisation and
`--gc-sections` have run. `link.x` and `_start` exist so that can happen;
`_start` ends in `wfi` and no hardware is ever involved.

## Adding a contender

One bin, one feature, one `[[bin]]` entry with `required-features`. The
feature matters: cargo unifies features per invocation, so `dacodec`
cannot be both `--no-default-features` and `--features serde` in the same
build. `tools/size.sh` therefore invokes `cargo build` once per
contender.

The bin must use `size::input()` for its input and `size::finish()` for
its answer, or the optimiser will constant-fold the whole job and the
binary will measure the size of nothing.

## `cbase/include/`

Six headers with declarations only, so that yyjson — which includes
`<stdlib.h>`, `<string.h>`, `<stdio.h>` and `<math.h>` — can be compiled
for a target that has no libc. Nothing is *defined* there; `shim.c`
defines the handful of functions the job actually reaches, and
`--gc-sections` drops the rest along with the yyjson code that referenced
them (the file API, and with it `fopen` and friends).
