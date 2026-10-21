#!/usr/bin/env bash
# Boot l4h the way GCE does: UEFI firmware, GRUB multiboot2 from the image
# built by tools/mkimage.sh, NVMe boot disk, virtio-net.  GRUB loads dom0
# and l4h.conf from the EFI partition.
set -euo pipefail
root=$(cd "$(dirname "$0")/.." && pwd)
out=$root/build/guest
mkdir -p "$out"
"$root/tools/build-linux.sh" vmlinux >"$root/build/linux-build.log" 2>&1 \
    || { grep -E "error|undefined" "$root/build/linux-build.log"; exit 1; }
objcopy --strip-debug "$root/build/linux-l4h/vmlinux" "$root/build/linux-l4h/vmlinux.stripped"
(cd "$root/tools/l4hctl" && cargo +stable build -q --release)
"$root/tools/mkinitramfs.sh" "$out/uefi.cpio.gz" "$root/tests/guest/init-uefi" \
    "$root/tools/l4hctl/target/release/l4hctl" "$root/tests/guest/udhcpc.script" >/dev/null
env -u L4H_DOM0_DISKS -u L4H_DOM0_CMDLINE "$root/tools/build-host.sh"
# The boot disk is a host disk too: keep it away from dom0.
cat > "$out/uefi.conf" <<'CONF'
# tests/run-uefitest.sh
dom0_disks=0
dom0_cmdline="console=hvc0 uefitest=esp"
CONF
"$root/tools/mkimage.sh" -k "$root/build/linux-l4h/vmlinux.stripped" -i "$out/uefi.cpio.gz" \
    -c "$out/uefi.conf" "$root/build/l4h.raw"
log=$root/build/qemu-uefi.log
timeout "${UEFITEST_TIMEOUT:-120}" "$root/tools/run-uefi.sh" "$root/build/l4h.raw" </dev/null >"$log" 2>&1 &
qemu=$!
while kill -0 "$qemu" 2>/dev/null; do
    grep -aq "UEFITEST: done\|l4h-root: all guests stopped" "$log" && { sleep 1; kill "$qemu"; break; }
    sleep 1
done
wait "$qemu" 2>/dev/null || true
grep -aE "UEFITEST|boot module|disk [0-9]|nic [0-9]|memory:|cores:|l4h-root: [0-9]+ core|panic|Boot" "$log" || true
grep -aq "UEFITEST: cmdline .*uefitest=esp" "$log"
grep -aq "UEFITEST: network ok" "$log"
