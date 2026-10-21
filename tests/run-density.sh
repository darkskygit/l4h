#!/usr/bin/env bash
# M6 gate: many guests on a 4-core host stay responsive.
# Usage: run-density.sh [guests] [MiB per guest]
set -euo pipefail
root=$(cd "$(dirname "$0")/.." && pwd)
source "$root/tools/guest-env.sh"
out=$root/build/guest
n=${1:-30}
mem=${2:-256}
mkdir -p "$out"
"$root/tools/build-linux.sh" vmlinux >"$root/build/linux-build.log" 2>&1 \
    || { grep -E "error|undefined" "$root/build/linux-build.log"; exit 1; }
"$l4h_objcopy" --strip-debug "$l4h_linux_out/vmlinux" "$l4h_linux_out/vmlinux.stripped"
"$root/tools/build-l4hctl.sh"
"$root/tools/mkinitramfs.sh" "$out/densvm.cpio.gz" "$root/tests/guest/init-densvm" >/dev/null
"$root/tools/mkinitramfs.sh" "$out/density.cpio.gz" "$root/tests/guest/init-density" \
    "$l4h_ctl" "$out/densvm.cpio.gz" >/dev/null
L4H_MEM=${L4H_MEM:-16G} L4H_DOM0_MEM_MB=256 \
    "$root/tools/run-dom0.sh" "$out/density.cpio.gz" "console=hvc0 DENS_N=$n DENS_MEM=$mem ${DENS_EXTRA:-}" ${DENSITY_TIMEOUT:-900} \
    | grep -E "DENSITY|memory|panic|l4h-root: (comp|.*fault)" || true
grep -q "DENSITY: PASS" "$root/build/qemu.log"
