#!/usr/bin/env bash
# Build the headless binaries as static musl.
#
# Needs a musl C compiler for ring's C and assembly. Debian or Ubuntu:
# musl-tools. Alpine: build-base. Arch: musl. The GUI is excluded because it
# loads X11, Wayland and Vulkan at runtime.
#
#   scripts/build-musl.sh
#   OVVC_MUSL_TARGET=aarch64-unknown-linux-musl scripts/build-musl.sh
#   MUSL_CC=x86_64-linux-musl-gcc scripts/build-musl.sh
set -euo pipefail

ROOT="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

target="${OVVC_MUSL_TARGET:-x86_64-unknown-linux-musl}"
linker="${MUSL_CC:-}"

if [ -z "$linker" ]; then
    if command -v musl-gcc >/dev/null 2>&1; then
        linker=musl-gcc
    elif cc -dumpmachine 2>/dev/null | grep -q musl; then
        linker=cc
    else
        echo "error: no musl C compiler found" >&2
        echo "  Debian or Ubuntu: sudo apt-get install musl-tools" >&2
        echo "  Alpine: apk add bash build-base" >&2
        echo "  Arch: sudo pacman -S musl" >&2
        echo "  or set MUSL_CC to the musl compiler" >&2
        exit 1
    fi
fi

if ! command -v "$linker" >/dev/null 2>&1; then
    echo "error: $linker not found; check MUSL_CC" >&2
    exit 1
fi

if ! test -d "$(rustc --print sysroot)/lib/rustlib/$target"; then
    if command -v rustup >/dev/null 2>&1; then
        rustup target add "$target"
    else
        echo "error: $target is not installed and rustup is unavailable" >&2
        echo "install the target with rustup or your distribution package" >&2
        exit 1
    fi
fi

target_cc="CC_${target//-/_}"

export CC="$linker"
export "$target_cc=$linker"
# Pin static linking. Do not set the Rust linker to musl-gcc: rustc's default
# cc driver with the bundled musl crt and libc.a produces a static-pie binary,
# while Debian's musl-gcc spec can turn the same link into a dynamic one. The
# musl compiler is only needed for C and assembly in dependencies such as ring
# and wayland-backend.
export RUSTFLAGS="${RUSTFLAGS:-} -C target-feature=+crt-static"

echo "building headless binaries for $target with $linker" >&2
exec cargo build --locked --release --target "$target" \
    -p ovvc-cli -p ovvc-daemon -p ovvc-tui -p ovvc-server "$@"
