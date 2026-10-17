#!/usr/bin/env bash
# M4 gate: dom0 creates, talks to and destroys guests through the control
# API, and passes a disk through to them.
set -euo pipefail
root=$(cd "$(dirname "$0")/.." && pwd)
out=$root/build/guest
mkdir -p "$out/ctlroot"
"$root/tools/build-linux.sh" vmlinux 2>&1 | grep -E "error|undefined" && exit 1 || true
objcopy --strip-debug "$root/build/linux-l4h/vmlinux" "$root/build/linux-l4h/vmlinux.stripped"
(cd "$root/tools/l4hctl" && cargo +stable build -q --release)
"$root/tools/mkinitramfs.sh" "$out/vm.cpio.gz" "$root/tests/guest/init-vm" >/dev/null
rm -rf "$out/ctlroot"/* && mkdir -p "$out/ctlroot"
cp "$out/vm.cpio.gz" "$out/ctlroot/"
cp "$root/build/linux-l4h/vmlinux.stripped" "$out/ctlroot/vmlinux"
rm -f "$out/ctl.img"
mkfs.ext4 -q -F "$out/ctl.img" 32M
# mkinitramfs copies extra files into /bin; move the payload to / in init.
"$root/tools/mkinitramfs.sh" "$out/ctltest.cpio.gz" "$root/tests/guest/init-ctltest" \
    "$root/tools/l4hctl/target/release/l4hctl" "$out/ctlroot/vmlinux" "$out/ctlroot/vm.cpio.gz" >/dev/null
L4H_DOM0_DISKS=0 L4H_DOM0_MEM_MB=512 \
L4H_QEMU_ARGS="-drive file=$out/ctl.img,if=none,id=d0,format=raw -device virtio-blk-pci,drive=d0,disable-legacy=on" \
    "$root/tools/run-dom0.sh" "$out/ctltest.cpio.gz" "console=hvc0" ${CTLTEST_TIMEOUT:-300} \
    | grep -E "CTLTEST|l4hctl|GUEST|vm [0-9]|disk [0-9]|^ *[0-9]+ |memory|l4h-root: guest|panic" || true
grep -q "CTLTEST: PASS" "$root/build/qemu.log"
