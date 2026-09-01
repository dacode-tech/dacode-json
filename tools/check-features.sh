#!/usr/bin/env bash
# Build and lint every supported feature combination, then cross-compile
# to a target with no allocator.
#
# The last part is the point. `std`-isms creep back in one `use` at a
# time, and on a host build nothing notices: `alloc::vec::Vec` and
# `std::vec::Vec` are the same type, `core::fmt` is re-exported from
# `std::fmt`, and a `#[cfg(feature = "std")]` that should have been
# `alloc` compiles perfectly until someone tries the target that has
# neither. `thumbv7em-none-eabihf` has no allocator at all, so anything
# reaching for one fails to compile there and only there.
#
# Usage: tools/check-features.sh [--fast]
#        --fast   skip the cross targets
set -uo pipefail

cd "$(dirname "$0")/.."

FAST=0
[ "${1:-}" = "--fast" ] && FAST=1

FAILED=0

# `cargo check` is enough for the feature matrix: the question there is
# whether the cfgs line up, and a type error is a type error.
run() {
    local label="$1"; shift
    printf '  %-34s ' "$label"
    local out
    if out=$("$@" 2>&1); then
        # A warning is a failure here: `unused import` is exactly how a
        # mis-gated item shows up.
        if grep -qE '^(warning|error)' <<<"$out"; then
            echo 'WARN'
            grep -E '^(warning|error)' <<<"$out" | head -5 | sed 's/^/      /'
            FAILED=1
        else
            echo 'ok'
        fi
    else
        echo 'FAIL'
        grep -E '^(warning|error)' <<<"$out" | head -20 | sed 's/^/      /'
        FAILED=1
    fi
}

# ---------------------------------------------------------------------
# The feature matrix
# ---------------------------------------------------------------------
#
# Ordered so that each row adds one thing to the row above, which makes a
# failure point at what added it.

echo 'feature matrix (host):'
run 'core only'            cargo check --quiet --no-default-features
run '+ alloc'              cargo check --quiet --no-default-features --features alloc
run '+ std'                cargo check --quiet --no-default-features --features std
run '+ serde'              cargo check --quiet --no-default-features --features serde
run 'std + serde'          cargo check --quiet --no-default-features --features std,serde
run 'default'              cargo check --quiet
run 'all features'         cargo check --quiet --all-features
run 'all features, tests'  cargo check --quiet --all-features --all-targets

echo
echo 'clippy:'
run 'core only'            cargo clippy --quiet --no-default-features
run '+ alloc'              cargo clippy --quiet --no-default-features --features alloc
run '+ serde'              cargo clippy --quiet --no-default-features --features serde
run 'all features, tests'  cargo clippy --quiet --all-features --all-targets

if [ "$FAST" = 1 ]; then
    echo
    [ "$FAILED" = 0 ] && echo 'all ok (cross targets skipped)' || echo 'FAILURES'
    exit "$FAILED"
fi

# ---------------------------------------------------------------------
# Cross targets
# ---------------------------------------------------------------------
#
# thumbv7em-none-eabihf: bare metal Cortex-M4F. No OS, no allocator, and
#   no `alloc` crate available unless the binary supplies a
#   `#[global_allocator]`. This is the one that catches a stray `Vec`.
#
# armv7-unknown-linux-gnueabihf: 32-bit ARM with a full `std`. Catches
#   the other class of bug — anything assuming a 64-bit `usize` or an
#   x86/aarch64 SIMD path.

BARE=thumbv7em-none-eabihf
HOSTED=armv7-unknown-linux-gnueabihf

missing=()
for t in "$BARE" "$HOSTED"; do
    rustup target list --installed | grep -qx "$t" || missing+=("$t")
done
if [ "${#missing[@]}" -gt 0 ]; then
    echo
    echo "missing targets: ${missing[*]}"
    echo "  rustup target add ${missing[*]}"
    exit 1
fi

# What this proves, and what it does not: building the library for
# `$BARE` produces an rlib, so nothing is linked. That is still the check
# that matters for `no_std`, because without `extern crate alloc` the
# name `Vec` does not resolve at all — a stray one is a compile error
# whether or not it is ever instantiated. What an rlib cannot show is
# what the *linked* binary drags in. `size/` does that, and measures it.

echo
echo "$BARE (no OS, no allocator):"
run 'core only'   cargo build --quiet --no-default-features --target "$BARE"
# `alloc` is legitimate here: an rlib may name `alloc` without supplying
# an allocator, and a binary that links it must provide one.
run '+ alloc'     cargo build --quiet --no-default-features --features alloc --target "$BARE"
run '+ serde'     cargo build --quiet --no-default-features --features serde --target "$BARE"

echo
echo "$HOSTED (32-bit, full std):"
run 'default'     cargo build --quiet --target "$HOSTED"
run 'all but dev' cargo build --quiet --features vela-compat --target "$HOSTED"

echo
if [ "$FAILED" = 0 ]; then echo 'all ok'; else echo 'FAILURES'; fi
exit "$FAILED"
