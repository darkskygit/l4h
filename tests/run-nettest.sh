#!/usr/bin/env bash
# M5 gate: L2 switches with an uplink, guest-to-guest traffic, multiple
# vNICs, switch isolation and MAC lock.
set -euo pipefail
root=$(cd "$(dirname "$0")/.." && pwd)
out=$root/build/guest
mkdir -p "$out"
"$root/tools/build-linux.sh" vmlinux >"$root/build/linux-build.log" 2>&1 \
    || { grep -E "error|undefined" "$root/build/linux-build.log"; exit 1; }
objcopy --strip-debug "$root/build/linux-l4h/vmlinux" "$root/build/linux-l4h/vmlinux.stripped"
(cd "$root/tools/l4hctl" && cargo +stable build -q --release)
"$root/tools/mkinitramfs.sh" "$out/netvm.cpio.gz" "$root/tests/guest/init-netvm" >/dev/null
cp "$root/build/linux-l4h/vmlinux.stripped" "$out/vmlinux"
"$root/tools/mkinitramfs.sh" "$out/nettest.cpio.gz" "$root/tests/guest/init-nettest" \
    "$root/tools/l4hctl/target/release/l4hctl" "$out/vmlinux" "$out/netvm.cpio.gz" \
    "$root/tests/guest/udhcpc.script" >/dev/null
L4H_DOM0_MEM_MB=384 \
L4H_QEMU_ARGS="-netdev user,id=n0 -device virtio-net-pci,netdev=n0,disable-legacy=on" \
    "$root/tools/run-dom0.sh" "$out/nettest.cpio.gz" "console=hvc0" ${NETTEST_TIMEOUT:-300} \
    | grep -E "NETTEST|NETVM|netd|nic|switch|^ +[0-9]+ |panic|l4h-root: (guest|comp)" || true
grep -q "NETTEST: PASS" "$root/build/qemu.log"
