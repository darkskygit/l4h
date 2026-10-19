#!/usr/bin/env bash
# Pass a virtio-blk PCI disk through to dom0 and check reads and writes.
# Usage: run-blktest.sh [vcpus]
# BLKTEST_DEV=nvme uses an NVMe controller, BLKTEST_DEV=scsi a virtio-scsi
# controller (scsi-legacy: a legacy-only one, as on GCE), instead of
# virtio-blk; BLKTEST_LBA=4096 gives either 4 KiB blocks.
set -euo pipefail
root=$(cd "$(dirname "$0")/.." && pwd)
out=$root/build/guest
mkdir -p "$out/blkroot"
echo "hello from the host" > "$out/blkroot/hello.txt"
rm -f "$out/blk.img"
mkfs.ext4 -q -F -b 4096 -d "$out/blkroot" "$out/blk.img" 64M
case "${BLKTEST_DEV:-virtio}" in
nvme) lba=${BLKTEST_LBA:-512}
      dev="-device nvme,drive=d0,serial=l4h0,logical_block_size=$lba,physical_block_size=$lba" ;;
scsi|scsi-legacy)
      lba=${BLKTEST_LBA:-512}
      # Target 1, as GCE places its boot disk.
      if [ "$BLKTEST_DEV" = scsi ]; then mode=disable-legacy=on; else mode=disable-modern=on; fi
      dev="-device virtio-scsi-pci,id=s0,$mode -device scsi-hd,drive=d0,bus=s0.0,scsi-id=1,logical_block_size=$lba,physical_block_size=$lba" ;;
*)    dev="-device virtio-blk-pci,drive=d0,disable-legacy=on" ;;
esac
"$root/tools/mkinitramfs.sh" "$out/blktest.cpio.gz" "$root/tests/guest/init-blktest" >/dev/null
L4H_QEMU_ARGS="-drive file=$out/blk.img,if=none,id=d0,format=raw $dev" \
L4H_DOM0_VCPUS=${1:-1} "$root/tools/run-dom0.sh" "$out/blktest.cpio.gz" "console=hvc0" 120 \
    | grep -E "BLKTEST|blkd|virtio|l4h-root|vda" || true
grep -q "BLKTEST: PASS" "$root/build/qemu.log"
debugfs -R "cat /guest.txt" "$out/blk.img" 2>/dev/null | grep -q "written by the l4h guest"
echo "host sees /guest.txt: ok"
