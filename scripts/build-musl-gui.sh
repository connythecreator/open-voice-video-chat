#!/usr/bin/env bash
# Build the GUI as a dynamic musl binary for a musl userland.
#
# A static musl GUI cannot work: it loads X11, Wayland and Vulkan with dlopen.
# Alpine: apk add bash build-base. The binary needs X11 or Wayland,
# libxkbcommon and a Vulkan driver at runtime. It does not run on glibc.
#
#   scripts/build-musl-gui.sh
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
        echo "  Alpine: apk add bash build-base" >&2
        echo "  Debian or Ubuntu: sudo apt-get install musl-tools" >&2
        echo "  Arch: sudo pacman -S musl" >&2
        echo "  or set MUSL_CC to the musl compiler" >&2
        exit 1
    fi
fi

if ! command -v "$linker" >/dev/null 2>&1; then
    echo "error: $linker not found; check MUSL_CC" >&2
    exit 1
fi

if ! cc -dumpmachine 2>/dev/null | grep -q musl; then
    echo "warning: this host is not musl, so the musl GUI will not run here" >&2
    echo "  build the glibc GUI with: cargo build --release -p ovvc-gui" >&2
fi

if ! test -d "$(rustc --print sysroot)/lib/rustlib/$target"; then
    if command -v rustup >/dev/null 2>&1; then
        rustup target add "$target"
    else
        echo "error: $target is not installed and rustup is unavailable" >&2
        exit 1
    fi
fi

target_env="${target^^}"
target_env="${target_env//-/_}"
target_cc="CC_${target//-/_}"

export CC="$linker"
export "$target_cc=$linker"
# musl-gcc is the right Rust linker here because the binary is dynamic and its
# libc must be musl's libc.so, not the host glibc. Only the static headless
# build uses the default cc driver with the bundled musl libc.a.
export "CARGO_TARGET_${target_env}_LINKER=$linker"
export RUSTFLAGS="${RUSTFLAGS:-} -C target-feature=-crt-static"

cargo build --locked --release --target "$target" -p ovvc-gui "$@"

bin="${CARGO_TARGET_DIR:-target}/$target/release/ovvc-gui"
echo "built $bin for $target" >&2
echo "run it on a musl userland with X11 or Wayland, libxkbcommon and a Vulkan driver" >&2
if command -v file >/dev/null 2>&1; then
    file "$bin" >&2
fi
