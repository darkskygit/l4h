#!/usr/bin/env bash
# Build the guest kernel + root task with a given dom0 initramfs/cmdline and
# boot it in QEMU.  Usage: run-dom0.sh <initrd> [cmdline] [timeout]
set -euo pipefail
root=$(cd "$(dirname "$0")/.." && pwd)
initrd=$(realpath "$1")
cmdline=${2:-console=hvc0}
tmo=${3:-60}
"$root/tools/build-linux.sh" vmlinux 2>&1 | grep -E "error|undefined" && exit 1 || true
objcopy --strip-debug "$root/build/linux-l4h/vmlinux" "$root/build/linux-l4h/vmlinux.stripped"
(cd "$root/host" && L4H_DOM0_KERNEL="$root/build/linux-l4h/vmlinux.stripped" \
    L4H_DOM0_INITRD="$initrd" L4H_DOM0_MEM_MB=${L4H_DOM0_MEM_MB:-256} \
    L4H_DOM0_VCPUS=${L4H_DOM0_VCPUS:-1} L4H_WATCHDOG_SECS=${L4H_WATCHDOG_SECS:-0} L4H_DOM0_CMDLINE="$cmdline" cargo build -q)
timeout "$tmo" "$root/tools/run-qemu.sh" "$root/host/target/x86_64-sel4-roottask/debug/l4h-root.elf" \
    </dev/null >"$root/build/qemu.log" 2>&1 || true
sed -n '/dropped to user space/,$p' "$root/build/qemu.log"
