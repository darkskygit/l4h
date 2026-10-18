#!/usr/bin/env bash
# Boot l4h with the ABI test guest as dom0 and check that it passes.  It
# gets a virtio-blk disk, whose sector 0 it reads through the I/O pool.
set -euo pipefail
root=$(cd "$(dirname "$0")/.." && pwd)
make -s -C "$root/tests/testguest"
L4H_DOM0_KERNEL="$root/build/testguest/testguest.elf" L4H_DOM0_INITRD= L4H_DOM0_MEM_MB=64 \
    "$root/tools/build-host.sh"
log=$root/build/testguest/qemu.log
disk=$root/build/testguest/disk.img
rm -f "$disk"
truncate -s 1M "$disk"
printf 'l4h-pool-test' | dd of="$disk" conv=notrunc 2>/dev/null
timeout 60 "$root/tools/run-qemu.sh" "$root/host/target/x86_64-sel4-roottask/debug/l4h-root.elf" \
    -drive "file=$disk,if=none,id=d0,format=raw" -device virtio-blk-pci,drive=d0,disable-legacy=on \
    </dev/null >"$log" 2>&1 || true
if grep -q "TESTGUEST: PASS" "$log"; then
    grep -a "testguest: I/O pool" "$log" || true
    echo "testguest: PASS"
else
    sed -n '/dropped to user/,$p' "$log"
    echo "testguest: FAIL"
    exit 1
fi
