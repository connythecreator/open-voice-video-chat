#!/usr/bin/env bash
# Build the headless ovvc binaries for a static musl target.
#
# The GUI is deliberately excluded. A static musl GUI would need static X11,
# Wayland and Vulkan libraries, which this project does not support. The
# daemon, CLI, TUI and relay server do not need those and can link statically.
#
# Opus is built from source with the musl C compiler. Without
# LIBOPUS_NO_PKG=1, audiopus_sys probes the host pkg-config, finds the glibc
# opus.pc, and emits a plain -lopus that a static musl link cannot use.
#
# Debian or Ubuntu needs musl-tools. Alpine is already musl, so its build-base
# package provides the compiler. The Rust musl target is installed if missing.
#
# Usage:
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
        echo "  Debian or Ubuntu: sudo apt-get install musl-tools cmake" >&2
        echo "  Alpine: apk add bash build-base cmake pkgconf" >&2
        echo "  Arch: sudo pacman -S musl cmake" >&2
        echo "  or set MUSL_CC to the musl compiler" >&2
        exit 1
    fi
fi

if ! command -v "$linker" >/dev/null 2>&1; then
    echo "error: $linker not found; check MUSL_CC" >&2
    exit 1
fi

if ! command -v cmake >/dev/null 2>&1; then
    echo "error: cmake not found; audiopus_sys needs it to build libopus" >&2
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

export LIBOPUS_NO_PKG=1
export OPUS_STATIC=1
export CC="$linker"
export "$target_cc=$linker"
# Pin static linking. Do not set the Rust linker to musl-gcc: rustc's default
# cc driver with the bundled musl crt and libc.a produces a static-pie binary,
# while Debian's musl-gcc spec can turn the same link into a dynamic one. The
# musl compiler is only needed for C dependencies such as libopus.
export RUSTFLAGS="${RUSTFLAGS:-} -C target-feature=+crt-static"

echo "building headless binaries for $target with $linker" >&2
exec cargo build --locked --release --target "$target" \
    -p ovvc-cli -p ovvc-daemon -p ovvc-tui -p ovvc-server "$@"
