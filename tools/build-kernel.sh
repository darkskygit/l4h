#!/usr/bin/env bash
# Build the patched seL4 kernel and install libsel4 + kernel into build/sel4.
set -euo pipefail
root=$(cd "$(dirname "$0")/.." && pwd)
src=$root/build/seL4-src
out=$root/build/kernel
prefix=$root/build/sel4
export PATH=$root/.venv/bin:$PATH

if [ ! -d "$src/.git" ]; then
    git clone -q "$root/kernel/seL4" "$src"
    git -C "$src" checkout -q "$(git -C "$root/kernel/seL4" rev-parse HEAD)"
    git -C "$src" checkout -q -b l4h
    shopt -s nullglob
    patches=("$root"/kernel/patches/*.patch)
    if [ ${#patches[@]} -gt 0 ]; then
        git -C "$src" -c user.name=l4h -c user.email=l4h@localhost am -q "${patches[@]}"
    fi
fi

cmake -G Ninja -S "$src" -B "$out" \
    -C "$root/kernel/configs/l4h-x86_64.cmake" \
    -DCMAKE_INSTALL_PREFIX="$prefix" \
    -DCROSS_COMPILER_PREFIX= >/dev/null
ninja -C "$out" all
ninja -C "$out" install >/dev/null
echo "kernel installed to $prefix"
