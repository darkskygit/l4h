#!/usr/bin/env bash
# Build the guest kernel + root task with a given dom0 initramfs/cmdline and
# boot it in QEMU.  Usage: run-dom0.sh <initrd> [cmdline] [timeout]
set -euo pipefail
root=$(cd "$(dirname "$0")/.." && pwd)
initrd=$(realpath "$1")
cmdline=${2:-console=hvc0}
tmo=${3:-60}
log=$root/build/qemu.log
# Never let a failed build leave the previous run's log to be checked.
rm -f "$log"
"$root/tools/build-linux.sh" vmlinux >"$root/build/linux-build.log" 2>&1 \
    || { grep -E "error|undefined" "$root/build/linux-build.log"; exit 1; }
objcopy --strip-debug "$root/build/linux-l4h/vmlinux" "$root/build/linux-l4h/vmlinux.stripped"
L4H_DOM0_KERNEL="$root/build/linux-l4h/vmlinux.stripped" \
    L4H_DOM0_INITRD="$initrd" L4H_DOM0_MEM_MB=${L4H_DOM0_MEM_MB:-256} \
    L4H_DOM0_VCPUS=${L4H_DOM0_VCPUS:-1} L4H_WATCHDOG_SECS=${L4H_WATCHDOG_SECS:-0} L4H_DOM0_CMDLINE="$cmdline" \
    "$root/tools/build-host.sh"
"$root/tools/run-qemu.sh" "$root/host/target/x86_64-sel4-roottask/debug/l4h-root.elf" \
    ${L4H_QEMU_ARGS:-} </dev/null >"$log" 2>&1 &
qemu=$!
# The machine never powers off by itself: stop it once every guest has
# stopped, or at the timeout.
end=$((SECONDS + tmo))
while kill -0 "$qemu" 2>/dev/null; do
    if grep -aq "l4h-root: all guests stopped" "$log" || [ "$SECONDS" -ge "$end" ]; then
        sleep 0.5
        kill "$qemu" 2>/dev/null || true
        break
    fi
    sleep 1
done
wait "$qemu" 2>/dev/null || true
sed -n '/dropped to user space/,$p' "$root/build/qemu.log"
