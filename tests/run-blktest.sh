#!/usr/bin/env bash
# Pass a virtio-blk PCI disk through to dom0 and check reads and writes.
# Usage: run-blktest.sh [vcpus]
set -euo pipefail
root=$(cd "$(dirname "$0")/.." && pwd)
out=$root/build/guest
mkdir -p "$out/blkroot"
echo "hello from the host" > "$out/blkroot/hello.txt"
rm -f "$out/blk.img"
mkfs.ext4 -q -F -d "$out/blkroot" "$out/blk.img" 64M
"$root/tools/mkinitramfs.sh" "$out/blktest.cpio.gz" "$root/tests/guest/init-blktest" >/dev/null
L4H_QEMU_ARGS="-drive file=$out/blk.img,if=none,id=d0,format=raw -device virtio-blk-pci,drive=d0,disable-legacy=on" \
L4H_DOM0_VCPUS=${1:-1} "$root/tools/run-dom0.sh" "$out/blktest.cpio.gz" "console=hvc0" 120 \
    | grep -E "BLKTEST|blkd|virtio|l4h-root|vda" || true
grep -q "BLKTEST: PASS" "$root/build/qemu.log"
debugfs -R "cat /guest.txt" "$out/blk.img" 2>/dev/null | grep -q "written by the l4h guest"
echo "host sees /guest.txt: ok"
