#!/usr/bin/env bash
# Build the l4h guest kernel (Linux 6.18 + arch/l4h).
# Usage: build-linux.sh [make targets...]   (default: vmlinux)
set -euo pipefail
root=$(cd "$(dirname "$0")/.." && pwd)
src=$root/build/linux-6.18
arch=${L4H_ARCH:-x86_64}
case "$arch" in
    x86_64) out=$root/build/linux-l4h; sel4=$root/build/sel4 ;;
    aarch64) out=$root/build/linux-l4h-aarch64; sel4=$root/build/sel4-aarch64 ;;
    *) echo "unsupported L4H_ARCH: $arch" >&2; exit 1 ;;
esac
if [ ! -d "$src" ]; then
    echo "missing $src: extract linux-6.18.y there (see DESIGN.md §13)" >&2
    exit 1
fi
ln -sfn "$root/guest/linux/arch/l4h" "$src/arch/l4h"
# Out-of-tree patches outside arch/l4h.  $src/.l4h-patches keeps a copy of
# each patch applied: from the first one that changed or went away, the
# applied patches are reverted (last first) and the current ones applied.
shopt -s nullglob
applied=$src/.l4h-patches
mkdir -p "$applied"
# Trees patched before the copies were kept have only stamps.
for s in "$src"/.l4h-applied-*.patch; do
    n=${s##*/.l4h-applied-}
    p=$root/guest/linux/patches/$n
    if [ ! -e "$p" ] || ! patch -d "$src" -p1 -R --dry-run -s -f < "$p" >/dev/null; then
        echo "$src: $n changed since it was applied: extract linux-6.18.y there again" >&2
        exit 1
    fi
    cp "$p" "$applied/$n"
    rm "$s"
done
have=("$applied"/*.patch)
want=("$root"/guest/linux/patches/*.patch)
keep=0
while [ $keep -lt ${#have[@]} ] && [ $keep -lt ${#want[@]} ] &&
      cmp -s "${have[$keep]}" "${want[$keep]}" &&
      [ "$(basename "${have[$keep]}")" = "$(basename "${want[$keep]}")" ]; do
    keep=$((keep + 1))
done
for ((i = ${#have[@]} - 1; i >= keep; i--)); do
    patch -d "$src" -p1 -R -f -s < "${have[$i]}"
    rm "${have[$i]}"
done
for p in "${want[@]:$keep}"; do
    patch -d "$src" -p1 --forward -s < "$p"
    cp "$p" "$applied/"
done
mkdir -p "$out"
mk=(make -C "$src" O="$out" ARCH=l4h SUBARCH="$arch"
    L4H_SEL4_INCLUDE="$sel4/libsel4/include"
    L4H_ABI_INCLUDE="$root/include")
if [ "$arch" = aarch64 ]; then
    mk+=(CROSS_COMPILE="${CROSS_COMPILE:-aarch64-linux-gnu-}")
fi
# L4H_LINUX_CC overrides the compiler, e.g. "ccache gcc" in CI.
if [ -n "${L4H_LINUX_CC:-}" ]; then
    mk+=(CC="$L4H_LINUX_CC")
fi
# Regenerate the config when the defconfig changed.
if [ ! -f "$out/.config" ] || [ "$root/guest/linux/arch/l4h/configs/defconfig" -nt "$out/.config" ]; then
    "${mk[@]}" defconfig
fi
"${mk[@]}" -j"${L4H_JOBS:-$(nproc)}" "${@:-vmlinux}"
