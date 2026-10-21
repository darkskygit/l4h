#!/usr/bin/env bash
# Boot l4h on aarch64 (QEMU virt, TCG) with the ABI test guest as dom0.
set -euo pipefail
root=$(cd "$(dirname "$0")/.." && pwd)
[ -f "$root/build/sel4-aarch64/bin/kernel.elf" ] || "$root/tools/build-kernel-aarch64.sh" >/dev/null
"$root/tools/build-loader-aarch64.sh"
make -s -C "$root/tests/testguest" -f Makefile.aarch64
export L4H_DOM0_KERNEL="$root/build/testguest-aarch64/testguest.elf" L4H_DOM0_INITRD=
L4H_DOM0_MEM_MB=64 "$root/tools/build-host-aarch64.sh"
log=$root/build/testguest-aarch64/qemu.log
disk=$root/build/testguest-aarch64/disk.img
truncate -s 1M "$disk"
printf 'l4h-pool-test' | dd of="$disk" conv=notrunc 2>/dev/null
"$root/tools/run-qemu-aarch64.sh" "$root/host/target/aarch64-sel4-roottask/debug/l4h-root.elf" \
    -drive "file=$disk,if=none,id=d0,format=raw" -device virtio-blk-pci,drive=d0,disable-legacy=on \
    </dev/null >"$log" 2>&1 &
qemu=$!
end=$((SECONDS + 300))
while kill -0 "$qemu" 2>/dev/null; do
    if grep -aq "l4h-root: all guests stopped\|TESTGUEST: FAIL" "$log" || [ "$SECONDS" -ge "$end" ]; then
        kill "$qemu" 2>/dev/null || true
        break
    fi
    sleep 1
done
wait "$qemu" 2>/dev/null || true
if grep -q "TESTGUEST: PASS" "$log"; then
    echo "testguest (aarch64): PASS"
else
    sed -n '/Entering kernel/,$p' "$log" | tail -30
    echo "testguest (aarch64): FAIL"
    exit 1
fi
