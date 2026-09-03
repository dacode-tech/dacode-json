#!/usr/bin/env bash
# What does integer width cost, on machines narrower than 32 bits?
#
# `Raw::as_int::<T>()` accumulates digits in `T` and nothing wider. On a
# 32-bit machine that is worth almost nothing — a 64-bit accumulator is
# two registers instead of one. The claim being tested is that it matters
# on an 8- or 16-bit machine, where a 64-bit accumulator is eight or four
# registers and every multiply is a library call.
#
# Measured on a `staticlib`, not a linked binary: AVR and MSP430 have no
# linker here. That is sound for *this* question — every column is the
# same crate compiled the same way, with one `#[no_mangle]` entry point,
# so `--gc-sections` has nothing to do that differs between them. It
# would not be sound for comparing two different libraries, which is why
# `tools/size.sh` links.
#
# Needs nightly and `rust-src` for `-Z build-std`: AVR and MSP430 are
# tier 3 and have no precompiled `core`.
#
# Usage: tools/width.sh
set -uo pipefail

cd "$(dirname "$0")/.."

NM=""
for c in /opt/homebrew/opt/llvm/bin/llvm-nm /usr/local/opt/llvm/bin/llvm-nm \
         "$(command -v llvm-nm || true)"; do
    [ -x "$c" ] && { NM="$c"; break; }
done
[ -n "$NM" ] || { echo "need llvm-size (brew install llvm)" >&2; exit 1; }
SIZE="${NM/llvm-nm/llvm-size}"

rustup toolchain list | grep -q '^nightly' || {
    echo "needs nightly: rustup toolchain install nightly --component rust-src" >&2
    exit 1; }

W=size/width

# target                    label        extra rustflags
TARGETS=(
    "thumbv7em-none-eabihf|ARM32 (Cortex-M4F)|"
    "msp430-none-elf|MSP430 (16-bit)|"
    "avr-none|AVR (8-bit)|-C target-cpu=atmega328p"
)

# `wi64` is `as_i64` — the lenient reading, which reaches the float
# parser. It is the column everything else is being compared against.
FEATURES=(w0 w8 w16 w32 w64 wi64)
LABELS=("baseline" "as_int::<i8>" "as_int::<i16>" "as_int::<i32>" "as_int::<i64>" "as_i64")

measure() {  # target rustflags feature -> bytes of .text + .rodata
    local t="$1" rf="$2" f="$3"
    RUSTFLAGS="$rf" cargo +nightly build --release --quiet \
        --manifest-path "$W/Cargo.toml" --target "$t" --features "$f" \
        -Z build-std=core,compiler_builtins >/dev/null 2>&1 || { echo "-"; return; }
    local lib
    lib=$(ls "$W/target/$t/release/libwidth.a" 2>/dev/null) || { echo "-"; return; }
    # `-Z build-std` emits one section per function, so `.text` alone is
    # empty and every `.text.*` has to be summed. `.ARM.exidx` is left
    # out: `panic = "abort"` should keep it empty, and a stray entry must
    # not inflate a column.
    "$SIZE" -A "$lib" 2>/dev/null \
        | awk '/^\.(text|rodata)/ { s += $2 } END { print s + 0 }'
}

printf '%-22s' 'accumulator'
for l in "${LABELS[@]}"; do printf '%16s' "$l"; done
echo
printf '%-22s' ''
printf '%s\n' "$(printf '%.0s-' {1..102})"

for spec in "${TARGETS[@]}"; do
    IFS='|' read -r t label rf <<<"$spec"
    printf '  %-20s' "$label"
    base=""
    for f in "${FEATURES[@]}"; do
        v="$(measure "$t" "$rf" "$f")"
        if [ -z "$base" ]; then
            base="$v"
            printf '%16s' "$v"
        elif [ "$v" = "-" ] || [ "$base" = "-" ]; then
            printf '%16s' "-"
        else
            printf '%16s' "+$((v - base))"
        fi
    done
    echo
done

echo
echo "  Baseline is bytes of .text + .rodata in the whole staticlib, which"
echo "  includes all of core; the rest are the bytes each accessor adds to"
echo "  it, which is the only part that differs between the columns."
echo "  8051 is absent because LLVM has no 8051 back end, so rustc has no"
echo "  target for it: \`rustc --print target-list\` lists avr-none and"
echo "  msp430-none-elf and nothing narrower."
