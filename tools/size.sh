#!/usr/bin/env bash
# Code size of one JSON job, linked for bare-metal ARM32.
#
# Every contender does the same thing: parse a fixed buffer of three
# seven-field records and sum an integer field. Same input, same shim,
# same allocator, same linker script, same optimisation settings — so the
# difference between two binaries is the JSON library.
#
# Linked, not compiled. An rlib holds generic code uninstantiated, so it
# has no size until monomorphisation and `--gc-sections` have run;
# comparing rlibs, or an rlib to a `.o`, measures nothing. The C side gets
# the same treatment for the same reason: a `.o` of yyjson is the whole
# library, not the part this job reaches. `size/link.x` exists so there is
# something to link *to*. Nothing ever runs.
#
# The C binaries link Rust's own `compiler_builtins` rlib, because ARMv7-M
# has no 64-bit integer division and no double-precision FPU, so both
# sides need the same soft-float and long-division routines. Using the
# same copy keeps that out of the comparison.
#
# Usage: tools/size.sh [opt-level]     default: z; try s and 3 as well
set -euo pipefail

cd "$(dirname "$0")/.."

OPT="${1:-z}"
TRIPLE=thumbv7em-none-eabihf

# --- tools ------------------------------------------------------------

NM=""
for c in /opt/homebrew/opt/llvm/bin/llvm-nm /usr/local/opt/llvm/bin/llvm-nm \
         "$(command -v llvm-nm || true)"; do
    [ -x "$c" ] && { NM="$c"; break; }
done
[ -n "$NM" ] || { echo "need llvm-nm and llvm-size (brew install llvm)" >&2; exit 1; }

CLANG="$(dirname "$NM")/clang"
# From the *active* toolchain's sysroot, not a glob over ~/.rustup: a
# second toolchain installed for something else (a `-Z build-std` target,
# say) would otherwise be picked up, and a minimal-profile one ships a
# `rust-lld` with no LLVM dylib beside it.
SYSROOT="$(rustc --print sysroot)"
LLD="$(ls "$SYSROOT"/lib/rustlib/*/bin/rust-lld 2>/dev/null | head -1)"
CB="$(ls "$SYSROOT/lib/rustlib/$TRIPLE/lib/"libcompiler_builtins-*.rlib 2>/dev/null | head -1)"

rustup target list --installed | grep -qx "$TRIPLE" || {
    echo "rustup target add $TRIPLE" >&2; exit 1; }

# --- Rust contenders --------------------------------------------------

# `strip` is off so the symbols exist to attribute. It changes no section
# `llvm-size` counts.
export CARGO_PROFILE_RELEASE_STRIP=false
export CARGO_PROFILE_RELEASE_OPT_LEVEL="$OPT"

OUT="size/target/$TRIPLE/release"
BINS=(pull pullint pullint3 pullfixed pullbytes write flatread direct serdejson)

echo "building rust (opt-level = $OPT) ..."
( cd size && cargo build --release --quiet --bin floor )
for b in "${BINS[@]}"; do
    f="$b"; case "$b" in pullbytes|pullint|pullint3|pullfixed) f=pull ;; esac
    ( cd size && cargo build --release --quiet --features "$f" --bin "$b" )
done

# --- C contenders -----------------------------------------------------

CDIR=size/cbase
COUT="$CDIR/target"
mkdir -p "$COUT"

CFLAGS=(
    "--target=$TRIPLE" -mfloat-abi=hard -mfpu=fpv4-sp-d16 "-O$OPT"
    -ffreestanding
    # Clang's own headers (stddef, stdint, limits, float) stay; the system
    # ones go, and `size/cbase/include` supplies the handful of libc
    # declarations yyjson needs. Nothing there is *defined* except in
    # `shim.c`, and `--gc-sections` drops whatever the job does not reach
    # (yyjson's file API, and with it `fopen` and friends).
    -nostdlibinc "-I$CDIR/include"
    -ffunction-sections -fdata-sections -fno-stack-protector
    # As `build.rs` does for the throughput benchmarks. It drops yyjson's
    # comment/inf/nan extensions, which `dacodec` does not have either, so
    # setting it is what makes the two parsers answer the same question.
    -DYYJSON_DISABLE_NON_STANDARD
)

if [ -f vendor/yyjson/yyjson.c ] && [ -n "$LLD" ] && [ -n "$CB" ]; then
    echo "building c   (-O$OPT) ..."
    "$CLANG" "${CFLAGS[@]}" -c "$CDIR/shim.c"        -o "$COUT/shim.o"
    "$CLANG" "${CFLAGS[@]}" -c "$CDIR/floor_job.c"   -o "$COUT/floor_job.o"
    "$CLANG" "${CFLAGS[@]}" -Ivendor/yyjson -c "$CDIR/yyjson_job.c" \
        -o "$COUT/yyjson_job.o"
    "$CLANG" "${CFLAGS[@]}" -Ivendor/yyjson -c vendor/yyjson/yyjson.c \
        -o "$COUT/yyjson.o"
    "$LLD" -flavor gnu -T size/link.x --nmagic --gc-sections \
        -o "$COUT/cfloor" "$COUT/shim.o" "$COUT/floor_job.o" "$CB"
    "$LLD" -flavor gnu -T size/link.x --nmagic --gc-sections \
        -o "$COUT/yyjson" "$COUT/shim.o" "$COUT/yyjson_job.o" "$COUT/yyjson.o" "$CB"
    CBINS=("$COUT/yyjson")
else
    echo "skipping c   (needs vendor/yyjson, rust-lld and compiler_builtins)"
    CBINS=()
fi

# simdjson is attempted every run rather than assumed to fail, so that the
# finding stays true rather than becoming folklore.
SIMD_ERR=""
if [ -f vendor/simdjson/simdjson.cpp ]; then
    SIMD_ERR=$("${CLANG}++" "${CFLAGS[@]}" -std=c++17 -fno-exceptions -fno-rtti \
        -Ivendor/simdjson -c vendor/simdjson/simdjson.cpp -o /dev/null 2>&1 \
        | grep -m1 'fatal error' || true)
fi

echo
echo "opt-level = $OPT, lto = fat, codegen-units = 1, panic = abort"
echo "target    = $TRIPLE (Cortex-M4F: hard f32, soft f64, no 64-bit divide)"
echo
python3 tools/size_attr.py "$NM" "$OUT/floor" "${BINS[@]/#/$OUT/}"

if [ "${#CBINS[@]}" -gt 0 ]; then
    echo "C, same job, same shim, same linker script:"
    echo
    python3 tools/size_attr.py "$NM" "$COUT/cfloor" "${CBINS[@]}"
fi

echo "not in the table:"
if [ -n "$SIMD_ERR" ]; then
    echo "  simdjson  does not compile for $TRIPLE:"
    echo "            ${SIMD_ERR#*: }"
    echo "            It needs ~38 C++ standard library headers and there is"
    echo "            no C++ standard library for a bare-metal target."
fi
echo "  sonic-rs  is std-only: it builds for armv7-unknown-linux-gnueabihf"
echo "            but not for $TRIPLE."
