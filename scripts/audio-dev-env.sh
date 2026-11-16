#!/usr/bin/env bash
# Stage the PipeWire and SPA development headers without root.
#
# PipeWire's runtime and tools are usually installed but the -dev packages are
# not. This script downloads the two .deb archives with `apt-get download`
# (which needs no privileges), extracts them under target/audio-sysroot, and
# exports the three variables the pipewire-sys build script and the resulting
# binary need:
#
#   PKG_CONFIG_PATH       points at the staged .pc files
#   PKG_CONFIG_SYSROOT_DIR makes those .pc files resolve inside the sysroot
#   LD_LIBRARY_PATH       lets the built probe find the runtime libraries
#
# It is idempotent and Linux-only. It never runs `apt-get install`, never uses
# sudo, and writes nothing in the repository outside target/audio-sysroot.
#
# Usage:
#   source scripts/audio-dev-env.sh
#   cargo test -p ovvc-audio --features pipewire
#
# Running it directly (bash scripts/audio-dev-env.sh) only prints the exports,
# which is useful in a Makefile or a one-off shell.

_audio_dev_script="${BASH_SOURCE[0]}"
_audio_dev_sourced=0
if [ "${_audio_dev_script}" != "$0" ]; then
    _audio_dev_sourced=1
fi

# Nounset protects this script, but leaving it enabled in a sourcing shell
# would abort unrelated commands that test unset variables. Remember whether
# the caller already had it and restore that before returning.
case $- in
    *u*)
        _audio_dev_restore_u=0
        ;;
    *)
        _audio_dev_restore_u=1
        set -u
        ;;
esac

main() {
    if [ "$(uname -s)" != "Linux" ]; then
        echo "audio-dev-env: the PipeWire backend is Linux-only; nothing staged" >&2
        return 1
    fi

    if ! command -v apt-get >/dev/null 2>&1; then
        echo "audio-dev-env: apt-get is not available on this system" >&2
        echo "audio-dev-env: install libpipewire-0.3-dev and libspa-0.2-dev with your" >&2
        echo "audio-dev-env: distro's package manager, then put their pkgconfig directory" >&2
        echo "audio-dev-env: on PKG_CONFIG_PATH yourself" >&2
        return 1
    fi
    if ! command -v dpkg-deb >/dev/null 2>&1; then
        echo "audio-dev-env: dpkg-deb is required to extract the development packages" >&2
        return 1
    fi

    local root
    root="$(cd "$(dirname "${_audio_dev_script}")/.." && pwd)" || return 1
    local sys="$root/target/audio-sysroot"

    local pc_dir
    pc_dir="$(find "$sys" -type d -path '*/usr/lib/*/pkgconfig' 2>/dev/null | head -n 1)"
    if [ -z "$pc_dir" ] || [ ! -f "$pc_dir/libpipewire-0.3.pc" ]; then
        local tmp
        tmp="$(mktemp -d "${TMPDIR:-/tmp}/ovvc-audio-debs.XXXXXX")" || return 1

        echo "audio-dev-env: downloading PipeWire development packages into $tmp" >&2
        if ! (cd "$tmp" && apt-get download libpipewire-0.3-dev libspa-0.2-dev); then
            rm -rf "$tmp"
            echo "audio-dev-env: apt-get download failed; check the package names for your distro" >&2
            return 1
        fi

        mkdir -p "$sys" || {
            rm -rf "$tmp"
            return 1
        }
        local deb
        for deb in "$tmp"/*.deb; do
            if ! dpkg-deb -x "$deb" "$sys"; then
                rm -rf "$tmp"
                echo "audio-dev-env: could not extract $deb" >&2
                return 1
            fi
        done
        rm -rf "$tmp"

        pc_dir="$(find "$sys" -type d -path '*/usr/lib/*/pkgconfig' 2>/dev/null | head -n 1)"
        if [ -z "$pc_dir" ] || [ ! -f "$pc_dir/libpipewire-0.3.pc" ]; then
            echo "audio-dev-env: extraction completed but libpipewire-0.3.pc was not found under $sys" >&2
            return 1
        fi
    else
        echo "audio-dev-env: reusing staged headers in $sys" >&2
    fi

    # The -dev packages do not ship the versioned runtime libraries, so copy
    # the host ones into the sysroot. cp -n keeps this idempotent.
    local lib_dir
    lib_dir="$(dirname "$pc_dir")"
    local host_lib_dir
    host_lib_dir="/usr/lib/$(uname -m)-linux-gnu"
    mkdir -p "$lib_dir" || return 1
    local lib
    for lib in libpipewire-0.3.so.0 libspa-0.2.so.0; do
        if [ -f "$host_lib_dir/$lib" ]; then
            cp -n "$host_lib_dir/$lib" "$lib_dir/" 2>/dev/null || true
        fi
    done

    export PKG_CONFIG_PATH="$pc_dir${PKG_CONFIG_PATH:+:$PKG_CONFIG_PATH}"
    export PKG_CONFIG_SYSROOT_DIR="$sys"
    export LD_LIBRARY_PATH="$lib_dir${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"

    printf 'export PKG_CONFIG_PATH="%s"\n' "$PKG_CONFIG_PATH"
    printf 'export PKG_CONFIG_SYSROOT_DIR="%s"\n' "$PKG_CONFIG_SYSROOT_DIR"
    printf 'export LD_LIBRARY_PATH="%s"\n' "$LD_LIBRARY_PATH"
    echo "audio-dev-env: these exports apply to this shell; use a dedicated shell" >&2
    echo "audio-dev-env: for the PipeWire build because PKG_CONFIG_SYSROOT_DIR" >&2
    echo "audio-dev-env: redirects later pkg-config lookups into $sys" >&2
    return 0
}

if [ "$_audio_dev_sourced" -eq 1 ]; then
    main
    _audio_dev_status=$?
    if [ "$_audio_dev_restore_u" -eq 1 ]; then
        set +u
    fi
    return "$_audio_dev_status"
else
    main || exit $?
fi
