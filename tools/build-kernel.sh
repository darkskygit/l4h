#!/usr/bin/env bash
# Build the patched seL4 kernel and install libsel4 + kernel into build/sel4.
# L4H_RELEASE=1 builds without kernel assertions and debug syscalls.
set -euo pipefail
root=$(cd "$(dirname "$0")/.." && pwd)
src=$root/build/seL4-src
out=$root/build/kernel
prefix=$root/build/sel4
export PATH=$root/.venv/bin:$PATH

shopt -s nullglob
patches=("$root"/kernel/patches/*.patch)
base=$(git -C "$root/kernel/seL4" rev-parse HEAD)
want=$(cat /dev/null "${patches[@]}" | sha256sum | cut -d' ' -f1)-$base
stamp=$src/.git/l4h-patches
if [ ! -d "$src/.git" ]; then
    git clone -q "$root/kernel/seL4" "$src"
fi
# (Re)apply the patches when they or the base changed since the last time.
if [ "$(cat "$stamp" 2>/dev/null)" != "$want" ]; then
    if [ -n "$(git -C "$src" status --porcelain --untracked-files=no)" ]; then
        echo "$src has uncommitted changes and kernel/patches changed: commit and" >&2
        echo "regenerate the patches (git format-patch -o kernel/patches $base), or discard them" >&2
        exit 1
    fi
    git -C "$src" checkout -q -B l4h "$base"
    if [ ${#patches[@]} -gt 0 ]; then
        git -C "$src" -c user.name=l4h -c user.email=l4h@localhost am -q "${patches[@]}"
    fi
    echo "$want" > "$stamp"
fi

debug=ON
[ "${L4H_RELEASE:-0}" = 1 ] && debug=OFF
config=$root/kernel/configs/l4h-x86_64.cmake
# The config only seeds the cache: start afresh when it changed.
[ "$config" -nt "$out/CMakeCache.txt" ] && rm -rf "$out"
cmake -G Ninja -S "$src" -B "$out" \
    -C "$config" \
    -DKernelDebugBuild=$debug \
    -DCMAKE_INSTALL_PREFIX="$prefix" \
    -DCROSS_COMPILER_PREFIX= >/dev/null
ninja -C "$out" all
ninja -C "$out" install >/dev/null
echo "kernel installed to $prefix (debug build: $debug)"
