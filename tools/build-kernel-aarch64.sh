#!/usr/bin/env bash
# Build the patched seL4 kernel for aarch64 (QEMU virt) into build/sel4-aarch64.
set -euo pipefail
root=$(cd "$(dirname "$0")/.." && pwd)
src=$root/build/seL4-src
out=$root/build/kernel-aarch64
prefix=$root/build/sel4-aarch64
export PATH=$root/.venv/bin:$PATH
[ -d "$src/.git" ] || "$root/tools/build-kernel.sh"
cmake -G Ninja -S "$src" -B "$out" \
    -C "$root/kernel/configs/l4h-aarch64.cmake" \
    -DCMAKE_INSTALL_PREFIX="$prefix" \
    -DCROSS_COMPILER_PREFIX=aarch64-linux-gnu- >/dev/null
ninja -C "$out" all
ninja -C "$out" install >/dev/null
echo "kernel installed to $prefix"
