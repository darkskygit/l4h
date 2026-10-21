#!/usr/bin/env bash
# The l4h-root serial monitor: list guests and restart dom0 from COM1.
set -euo pipefail
root=$(cd "$(dirname "$0")/.." && pwd)
source "$root/tools/guest-env.sh"
out=$root/build/guest
mkdir -p "$out"
"$root/tools/build-linux.sh" vmlinux >"$root/build/linux-build.log" 2>&1 \
    || { grep -E "error|undefined" "$root/build/linux-build.log"; exit 1; }
"$l4h_objcopy" --strip-debug "$l4h_linux_out/vmlinux" "$l4h_linux_out/vmlinux.stripped"
"$root/tools/mkinitramfs.sh" "$out/vm.cpio.gz" "$root/tests/guest/init-vm" >/dev/null
export L4H_GUEST_KERNEL="$l4h_linux_out/vmlinux.stripped" L4H_DOM0_INITRD="$out/vm.cpio.gz"
L4H_DOM0_CMDLINE=console=hvc0 "$l4h_host_build"
log=$root/build/qemu-monitor.log
fifo=$(mktemp -u)
mkfifo "$fifo"
trap 'rm -f "$fifo"' EXIT
"$l4h_qemu" "$root/host/target/$l4h_target/debug/l4h-root.elf" <"$fifo" >"$log" 2>&1 &
qemu=$!
exec 3>"$fifo"
wait_for() { # pattern count timeout
    local end=$((SECONDS + $3))
    until [ "$(grep -ac "$1" "$log")" -ge "$2" ]; do
        [ "$SECONDS" -lt "$end" ] || { echo "timeout waiting for: $1"; kill "$qemu"; exit 1; }
        sleep 0.5
    done
}
wait_for "GUEST: ready" 1 120
printf 'list\r' >&3
wait_for "monitor: vm 0 'dom0'" 1 30
printf 'restart-dom0\r' >&3
wait_for "GUEST: ready" 2 120
printf 'info\r' >&3
wait_for "monitor: 4 cores" 1 30
kill "$qemu" 2>/dev/null || true
grep -aE "monitor:|restarting dom0" "$log"
echo "monitor: PASS"
