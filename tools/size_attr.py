#!/usr/bin/env python3
"""Measure and attribute the code size of the linked size-harness binaries.

`llvm-size -A` gives `.text` and `.rodata`, which is the figure to quote:
file size includes headers and debug sections that vary by toolchain, and
an rlib is not a size at all until it has been monomorphised and linked.

The per-crate breakdown below is *indicative*, not exact. With `lto =
"fat"` a library function inlined into the harness is attributed to the
harness, so a library's own line understates it. What is exact is the
total, and the split between the Rust runtime (`core`,
`compiler_builtins`, `alloc`) and everything else — which is the split
that matters, because a C binary gets the runtime's equivalent from libc
and does not pay for it here.

Attribution rules:

* Rust v0 mangling (`_R...`) names the defining crate after a `Cs<hash>_`
  disambiguator, length-prefixed.
* Legacy mangling (`_ZN...`) is length-prefixed from the start. Trait-impl
  symbols begin with a `_$LT$..$GT$` blob; for those the first recognised
  crate name in the symbol is used.
* ARM EABI aliases, the `mem*` routines and LLVM's outlined functions are
  `compiler_builtins` by definition.
* `.Lanon.*` rodata cannot be attributed by name at all and is reported
  as `anonymous`.
* Section bytes no sized symbol accounts for are `unattributed`, reported
  rather than dropped.

Usage: size_attr.py <llvm-nm> <floor.elf> <elf> [<elf> ...]
"""

import os
import re
import subprocess
import sys
from collections import defaultdict

RUNTIME = {"core", "compiler_builtins", "alloc", "std"}

V0_CRATE = re.compile(r"Cs[0-9A-Za-z]+_(\d+)([A-Za-z_][A-Za-z0-9_]*)")
LEGACY_HEAD = re.compile(r"^_ZN(\d+)([A-Za-z_][A-Za-z0-9_]*)")
LEGACY_ANY = re.compile(r"(?:^|[^A-Za-z0-9_])([a-z][a-z0-9_]{2,})\.\.")

# Compiler-supplied, on both sides of the comparison. ARMv7-M has no
# 64-bit divide and no double-precision FPU, so these are unavoidable and
# identical for every contender; a hosted binary would get them from libc
# and libgcc instead.
BUILTIN = re.compile(
    r"^(__aeabi_|__udivmoddi4|__udivsi3|__divdf3|__muldf3|__adddf3|__subdf3"
    r"|__floatdi|__fixdf|OUTLINED_FUNCTION_"
    r"|mem(cpy|set|cmp|move)$|strlen$)"
)
RUST_SHIM = re.compile(r"^_*rust_(alloc|dealloc|realloc|alloc_zeroed|no_alloc)")


def crate_of(sym, known):
    if BUILTIN.match(sym):
        return "compiler_builtins"
    if RUST_SHIM.match(sym.lstrip("_")) or sym.lstrip("_").startswith("rust_alloc"):
        return "alloc"
    if sym == "_start":
        return "entry"
    if sym.startswith(".Lanon") or sym.startswith(".L"):
        return "anonymous"
    if sym.startswith("_R"):
        m = V0_CRATE.search(sym)
        if m and len(m.group(2)) >= int(m.group(1)):
            c = m.group(2)[: int(m.group(1))]
            # rustc emits the allocator shims into a synthetic crate.
            return "alloc" if c.strip("_") == "rustc" else c
        return "other"
    if sym.startswith("_ZN"):
        m = LEGACY_HEAD.match(sym)
        if m:
            name = m.group(2)[: int(m.group(1))]
            if not name.startswith("_"):
                return name
        for h in LEGACY_ANY.findall(sym):
            if h in known:
                return h
        hits = LEGACY_ANY.findall(sym)
        return hits[0] if hits else "other"
    # No Rust mangling at all: a C symbol, or one of the `mem*` routines
    # the shim defines. In a C binary that is the library under test.
    return "unmangled (C)"


def sections(nm, elf):
    out = subprocess.run(
        [nm.replace("llvm-nm", "llvm-size"), "-A", elf],
        capture_output=True, text=True, check=True,
    ).stdout
    got = {}
    for line in out.splitlines():
        f = line.split()
        if len(f) >= 2 and f[0].startswith("."):
            try:
                got[f[0]] = int(f[1])
            except ValueError:
                pass
    return got


def symbols(nm, elf):
    out = subprocess.run(
        [nm, "--print-size", "--radix=d", elf], capture_output=True, text=True
    ).stdout
    for line in out.splitlines():
        f = line.split()
        if len(f) != 4:
            continue
        try:
            size = int(f[1])
        except ValueError:
            continue
        if f[2] in "tTrR":
            yield size, f[3]


def measure(nm, elf, harness=None):
    sec = sections(nm, elf)
    total = sec.get(".text", 0) + sec.get(".rodata", 0)
    syms = [(s, n) for s, n in symbols(nm, elf) if s > 0]

    known = {c for c in (crate_of(n, set()) for _, n in syms) if c != "other"}
    by = defaultdict(int)
    for s, n in syms:
        c = crate_of(n, known)
        # The binary crate is the harness, and with `lto = "fat"` it has
        # absorbed whatever the library inlined into it. Say so.
        if harness and c == harness:
            c = f"{harness} (harness + inlined)"
        by[c] += s
    by["unattributed"] = total - sum(by.values())
    return {
        "text": sec.get(".text", 0),
        "rodata": sec.get(".rodata", 0),
        "total": total,
        "by": dict(by),
    }


def main():
    if len(sys.argv) < 4:
        print(__doc__)
        return 2
    nm = sys.argv[1]
    floor_elf, elves = sys.argv[2], sys.argv[3:]

    floor = measure(nm, floor_elf)
    results = [
        (os.path.basename(e), measure(nm, e, harness=os.path.basename(e)))
        for e in elves
    ]

    print(f"floor (the bare-metal shim alone): {floor['total']} B "
          f"(.text {floor['text']}, .rodata {floor['rodata']})")
    print()
    print(f"{'':<14}{'.text':>9}{'.rodata':>9}{'total':>9}{'- floor':>9}"
          f"{'runtime':>9}{'library':>9}")
    print("  " + "-" * 68)
    for name, m in results:
        runtime = sum(v for k, v in m["by"].items() if k in RUNTIME)
        job = m["total"] - floor["total"]
        print(f"  {name:<12}{m['text']:>9}{m['rodata']:>9}{m['total']:>9}"
              f"{job:>9}{runtime:>9}{job - runtime:>9}")
    print()
    print("  runtime = core + compiler_builtins + alloc. A C binary gets the")
    print("  equivalent from libc and does not pay for it in its own .text.")
    print()

    for name, m in results:
        print(f"{name}:")
        for c, v in sorted(m["by"].items(), key=lambda kv: -kv[1]):
            if v <= 0:
                continue
            mark = " *" if c in RUNTIME else ""
            print(f"    {c:<24}{v:>8}{mark}")
        print()
    return 0


if __name__ == "__main__":
    sys.exit(main())
