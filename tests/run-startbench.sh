#!/usr/bin/env bash
# Create and boot latency of sandbox-sized guests: STB_MEM MiB (4096),
# 2 vCPUs, a 7 MB initramfs.  Usage: run-startbench.sh [iterations]
set -euo pipefail
root=$(cd "$(dirname "$0")/.." && pwd)
source "$root/tools/guest-env.sh"
out=$root/build/guest
mkdir -p "$out/stb"
"$root/tools/build-linux.sh" vmlinux >"$root/build/linux-build.log" 2>&1 \
    || { grep -E "error|undefined" "$root/build/linux-build.log"; exit 1; }
"$l4h_objcopy" --strip-debug "$l4h_linux_out/vmlinux" "$l4h_linux_out/vmlinux.stripped"
"$root/tools/build-l4hctl.sh"
"$l4h_cc" -O2 -static -o "$out/mstime" "$root/tests/guest/mstime.c"
# A sandbox's initramfs is about 7 MB: pad the test guest's with noise.
head -c 7M /dev/urandom > "$out/stb/pad"
"$root/tools/mkinitramfs.sh" "$out/big.cpio.gz" "$root/tests/guest/init-vm" "$out/stb/pad" >/dev/null
"$root/tools/mkinitramfs.sh" "$out/startbench.cpio.gz" "$root/tests/guest/init-startbench" \
    "$l4h_ctl" "$out/mstime" "$out/big.cpio.gz" >/dev/null
mem=${STB_MEM:-4096}
L4H_MEM=${L4H_MEM:-$((mem / 1024 + 2))G} L4H_DOM0_MEM_MB=256 \
    "$root/tools/run-dom0.sh" "$out/startbench.cpio.gz" "console=hvc0 STB_N=${1:-5} STB_MEM=$mem" ${STB_TIMEOUT:-900} \
    | grep -a "STARTBENCH" || true
grep -aq "STARTBENCH: done" "$root/build/qemu.log"
