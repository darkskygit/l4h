#!/usr/bin/env bash
# Build the l4h guest kernel (Linux 6.18 + arch/l4h).
# Usage: build-linux.sh [make targets...]   (default: vmlinux)
set -euo pipefail
root=$(cd "$(dirname "$0")/.." && pwd)
src=$root/build/linux-6.18
out=$root/build/linux-l4h
if [ ! -d "$src" ]; then
    echo "missing $src: extract linux-6.18.y there (see DESIGN.md §13)" >&2
    exit 1
fi
ln -sfn "$root/guest/linux/arch/l4h" "$src/arch/l4h"
# Out-of-tree patches outside arch/l4h, applied once.
for p in "$root"/guest/linux/patches/*.patch; do
    [ -e "$p" ] || continue
    stamp=$src/.l4h-applied-$(basename "$p")
    [ -e "$stamp" ] && continue
    patch -d "$src" -p1 --forward -s < "$p"
    touch "$stamp"
done
mkdir -p "$out"
mk=(make -C "$src" O="$out" ARCH=l4h
    L4H_SEL4_INCLUDE="$root/build/sel4/libsel4/include"
    L4H_ABI_INCLUDE="$root/include")
if [ ! -f "$out/.config" ]; then
    "${mk[@]}" defconfig
fi
"${mk[@]}" -j"$(nproc)" "${@:-vmlinux}"
