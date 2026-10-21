#!/usr/bin/env bash
# The guest ABI and the seL4 extensions, below Linux: dom0 gets the kernel's
# module tree (/dev/l4hkmod) and loads tests/guest/abitest, built against
# that tree as a guest would.  dom0 has a virtio-blk disk, whose sector 0
# the module reads through the I/O pool, and which no driver takes.
set -euo pipefail
root=$(cd "$(dirname "$0")/.." && pwd)
source "$root/tools/guest-env.sh"
out=$root/build/abitest
rm -rf "$out" && mkdir -p "$out/tree" "$out/mod"
"$root/tools/build-kmods.sh" >/dev/null
fsck.erofs --extract="$out/tree" "$l4h_linux_out/kmods.erofs" >/dev/null
cross=()
[ "${L4H_ARCH:-x86_64}" = aarch64 ] && cross=(CROSS_COMPILE="${CROSS_COMPILE:-aarch64-linux-gnu-}")
cp "$root"/tests/guest/abitest/* "$out/mod/"
make -s -C "$out/tree"/*/build M="$out/mod" "${cross[@]}" modules >"$out/build.log" 2>&1 \
    || { cat "$out/build.log"; exit 1; }
"$root/tools/mkinitramfs.sh" "$out/abitest.cpio.gz" "$root/tests/guest/init-abitest" \
    "$out/mod/l4h_abitest.ko" >/dev/null
truncate -s 1M "$out/disk.img"
printf 'l4h-pool-test' | dd of="$out/disk.img" conv=notrunc 2>/dev/null
L4H_KMODS=1 \
L4H_QEMU_ARGS="-drive file=$out/disk.img,if=none,id=d0,format=raw -device virtio-blk-pci,drive=d0,disable-legacy=on" \
    "$root/tools/run-dom0.sh" "$out/abitest.cpio.gz" "console=hvc0 initcall_blacklist=virtio_blk_init" \
    $((60 * l4h_test_slow)) | grep -a "ABITEST\|abitest:" || true
grep -aq "ABITEST: module tree" "$root/build/qemu.log" && grep -aq "ABITEST: PASS" "$root/build/qemu.log"
