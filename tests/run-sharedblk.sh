#!/usr/bin/env bash
# Native block windows: concurrent clients, isolation, provisioning, reuse
# and destroy during I/O. SHARED_BLK_DEV=nvme, SHARED_BLK_LBA=4096 also tested.
# L4H_TEST_KERNEL may name an already-built compatible guest kernel.
set -euo pipefail
root=$(cd "$(dirname "$0")/.." && pwd)
export PATH="/usr/sbin:$PATH"
out=$root/build/sharedblk
mkdir -p "$out"
rustc +stable --edition=2024 --test "$root/tests/blk-windows.rs" -o "$out/blk-window-tests"
"$out/blk-window-tests"
if [ -n "${L4H_TEST_KERNEL:-}" ]; then
    objcopy --strip-debug "$L4H_TEST_KERNEL" "$out/vmlinux"
else
    "$root/tools/build-linux.sh" vmlinux > "$out/linux-build.log" 2>&1
    objcopy --strip-debug "$root/build/linux-l4h/vmlinux" "$out/vmlinux"
fi
"$root/tools/build-l4hctl.sh"
"$root/tools/mkinitramfs.sh" "$out/sharedvm.cpio.gz" "$root/tests/guest/init-sharedblkvm" >/dev/null
"$root/tools/mkinitramfs.sh" "$out/dom0.cpio.gz" "$root/tests/guest/init-sharedblk" \
    "$root/tools/l4hctl/target/release/l4hctl" "$out/vmlinux" "$out/sharedvm.cpio.gz" >/dev/null
L4H_DOM0_DISKS=0 L4H_DOM0_KERNEL="$out/vmlinux" L4H_DOM0_INITRD="$out/dom0.cpio.gz" \
    L4H_DOM0_MEM_MB=256 L4H_WATCHDOG_SECS=0 "$root/tools/build-host.sh" > "$out/host-build.log" 2>&1
truncate -s 64M "$out/disk.raw"
truncate -s 64M "$out/slow.raw"
case "${SHARED_BLK_DEV:-virtio}" in
nvme) slowdev=(-device "nvme,drive=d1,serial=slow,logical_block_size=${SHARED_BLK_LBA:-512},physical_block_size=${SHARED_BLK_LBA:-512}")
      dev=(-device "nvme,drive=d0,serial=shared,logical_block_size=${SHARED_BLK_LBA:-512},physical_block_size=${SHARED_BLK_LBA:-512}") ;;
*) slowdev=(-device virtio-blk-pci,drive=d1,disable-legacy=on)
   dev=(-device virtio-blk-pci,drive=d0,disable-legacy=on) ;;
esac
"$root/tools/run-qemu.sh" "$root/host/target/x86_64-sel4-roottask/debug/l4h-root.elf" \
    -drive "file=$out/disk.raw,if=none,id=d0,format=raw" "${dev[@]}" \
    -drive "file=$out/slow.raw,if=none,id=d1,format=raw,throttling.bps-read=65536" "${slowdev[@]}" > "$out/qemu.log" 2>&1 &
qemu=$!
trap 'kill "$qemu" 2>/dev/null || true; wait "$qemu" 2>/dev/null || true' EXIT
end=$((SECONDS + 300))
while kill -0 "$qemu" 2>/dev/null && [ "$SECONDS" -lt "$end" ]; do
    if grep -aqE 'SHAREDBLK: (PASS|FAIL)|panicked at|Caught cap fault' "$out/qemu.log"; then break; fi
    sleep 1
done
sed -n '/l4h-root:/,$p' "$out/qemu.log"
grep -q 'SHAREDBLK: PASS' "$out/qemu.log"
grep -q '4 MiB of I/O pool kept out of use' "$out/qemu.log"
! grep -qE 'SHAREDVM: FAIL|SHAREDBLK: FAIL' "$out/qemu.log"
