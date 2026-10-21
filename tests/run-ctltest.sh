#!/usr/bin/env bash
# M4 gate: dom0 creates, talks to and destroys guests through the control
# API, and passes a disk through to them; last, it resets the host.
set -euo pipefail
root=$(cd "$(dirname "$0")/.." && pwd)
source "$root/tools/guest-env.sh"
out=$root/build/guest
mkdir -p "$out/ctlroot"
"$root/tools/build-linux.sh" vmlinux >"$root/build/linux-build.log" 2>&1 \
    || { grep -E "error|undefined" "$root/build/linux-build.log"; exit 1; }
"$l4h_objcopy" --strip-debug "$l4h_linux_out/vmlinux" "$l4h_linux_out/vmlinux.stripped"
"$root/tools/build-l4hctl.sh"
"$root/tools/mkinitramfs.sh" "$out/vm.cpio.gz" "$root/tests/guest/init-vm" >/dev/null
rm -rf "$out/ctlroot"/* && mkdir -p "$out/ctlroot"
cp "$out/vm.cpio.gz" "$out/ctlroot/"
rm -f "$out/ctl.img"
mkfs.ext4 -q -F "$out/ctl.img" 32M
# Disks 1 and 2: a byte pattern, read slowly (throttled) so that requests
# are still in flight when their guest is destroyed.  The 4 MiB I/O pool
# must drain within blkd's five-second detach deadline: 2 MiB/s leaves
# room for throttling and scheduling delays.
for i in 1 2; do head -c 32M /dev/zero | tr '\000' '\245' > "$out/dma$i.img"; done
slow=throttling.bps-read=2097152
# mkinitramfs copies extra files into /bin; move the payload to / in init.
"$root/tools/mkinitramfs.sh" "$out/ctltest.cpio.gz" "$root/tests/guest/init-ctltest" \
    "$l4h_ctl" "$out/ctlroot/vm.cpio.gz" >/dev/null
L4H_DOM0_DISKS=0 L4H_DOM0_MEM_MB=512 \
L4H_QEMU_ARGS="-drive file=$out/ctl.img,if=none,id=d0,format=raw -device virtio-blk-pci,drive=d0,disable-legacy=on \
    -drive file=$out/dma1.img,if=none,id=d1,format=raw,$slow -device virtio-blk-pci,drive=d1,disable-legacy=on \
    -drive file=$out/dma2.img,if=none,id=d2,format=raw,$slow -device nvme,drive=d2,serial=l4hdma" \
    "$root/tools/run-dom0.sh" "$out/ctltest.cpio.gz" "console=hvc0 L4H_TEST_SLOW=$l4h_test_slow" ${CTLTEST_TIMEOUT:-$((600 * l4h_test_slow))} \
    | grep -E "CTLTEST|l4hctl|GUEST|vm [0-9]|disk [0-9]|^ *[0-9]+ |memory|l4h-root: guest|panic" || true
grep -q "CTLTEST: PASS" "$root/build/qemu.log"
grep -aq "l4h-root: host reboot (dom0)" "$root/build/qemu.log" || { echo "CTLTEST: no host reboot"; exit 1; }
! grep -aq "the host did not reset\|host reboot failed" "$root/build/qemu.log" || { echo "CTLTEST: the host did not reset"; exit 1; }
echo "CTLTEST: host reset"
