#!/usr/bin/env bash
# Boot l4h the way GCE does: UEFI firmware, GRUB multiboot2 from the image
# built by tools/mkimage.sh, NVMe boot disk, virtio-net.
set -euo pipefail
root=$(cd "$(dirname "$0")/.." && pwd)
out=$root/build/guest
mkdir -p "$out"
"$root/tools/build-linux.sh" vmlinux 2>&1 | grep -E "error|undefined" && exit 1 || true
objcopy --strip-debug "$root/build/linux-l4h/vmlinux" "$root/build/linux-l4h/vmlinux.stripped"
(cd "$root/tools/l4hctl" && cargo +stable build -q --release)
"$root/tools/mkinitramfs.sh" "$out/uefi.cpio.gz" "$root/tests/guest/init-uefi" \
    "$root/tools/l4hctl/target/release/l4hctl" "$root/tests/guest/udhcpc.script" >/dev/null
# The boot disk is a host disk too: keep it away from dom0.
L4H_DOM0_KERNEL="$root/build/linux-l4h/vmlinux.stripped" L4H_DOM0_INITRD="$out/uefi.cpio.gz" \
    L4H_DOM0_DISKS=0 L4H_DOM0_CMDLINE=console=hvc0 "$root/tools/build-host.sh"
"$root/tools/mkimage.sh" "$root/build/l4h.raw"
log=$root/build/qemu-uefi.log
timeout "${UEFITEST_TIMEOUT:-120}" "$root/tools/run-uefi.sh" "$root/build/l4h.raw" </dev/null >"$log" 2>&1 &
qemu=$!
while kill -0 "$qemu" 2>/dev/null; do
    grep -aq "UEFITEST: done\|l4h-root: all guests stopped" "$log" && { sleep 1; kill "$qemu"; break; }
    sleep 1
done
wait "$qemu" 2>/dev/null || true
grep -aE "UEFITEST|disk [0-9]|nic [0-9]|memory:|cores:|l4h-root: [0-9]+ core|panic|Boot" "$log" || true
grep -aq "UEFITEST: network ok" "$log"
