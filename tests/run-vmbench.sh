#!/usr/bin/env bash
# Per-guest host memory overhead and guest create / boot / shutdown /
# destroy latency.  Usage: run-vmbench.sh [iterations]
set -euo pipefail
root=$(cd "$(dirname "$0")/.." && pwd)
source "$root/tools/guest-env.sh"
out=$root/build/guest
mkdir -p "$out"
"$root/tools/build-linux.sh" vmlinux >"$root/build/linux-build.log" 2>&1 \
    || { grep -E "error|undefined" "$root/build/linux-build.log"; exit 1; }
"$l4h_objcopy" --strip-debug "$l4h_linux_out/vmlinux" "$l4h_linux_out/vmlinux.stripped"
"$root/tools/build-l4hctl.sh"
"$l4h_cc" -O2 -static -o "$out/mstime" "$root/tests/guest/mstime.c"
"$root/tools/mkinitramfs.sh" "$out/vm.cpio.gz" "$root/tests/guest/init-vm" >/dev/null
"$root/tools/mkinitramfs.sh" "$out/vmbench.cpio.gz" "$root/tests/guest/init-vmbench" \
    "$l4h_ctl" "$out/mstime" "$out/vm.cpio.gz" >/dev/null
L4H_MEM=${L4H_MEM:-2G} L4H_DOM0_MEM_MB=256 \
    "$root/tools/run-dom0.sh" "$out/vmbench.cpio.gz" "console=hvc0 VMB_N=${1:-10}" ${VMBENCH_TIMEOUT:-900} \
    | grep -a "VMBENCH" || true
grep -aq "VMBENCH: done" "$root/build/qemu.log"
