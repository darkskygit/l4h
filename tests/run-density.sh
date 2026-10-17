#!/usr/bin/env bash
# M6 gate: many guests on a 4-core host stay responsive.
# Usage: run-density.sh [guests] [MiB per guest]
set -euo pipefail
root=$(cd "$(dirname "$0")/.." && pwd)
out=$root/build/guest
n=${1:-30}
mem=${2:-256}
mkdir -p "$out"
"$root/tools/build-linux.sh" vmlinux 2>&1 | grep -E "error|undefined" && exit 1 || true
objcopy --strip-debug "$root/build/linux-l4h/vmlinux" "$root/build/linux-l4h/vmlinux.stripped"
(cd "$root/tools/l4hctl" && cargo +stable build -q --release)
"$root/tools/mkinitramfs.sh" "$out/densvm.cpio.gz" "$root/tests/guest/init-densvm" >/dev/null
cp "$root/build/linux-l4h/vmlinux.stripped" "$out/vmlinux"
"$root/tools/mkinitramfs.sh" "$out/density.cpio.gz" "$root/tests/guest/init-density" \
    "$root/tools/l4hctl/target/release/l4hctl" "$out/vmlinux" "$out/densvm.cpio.gz" >/dev/null
L4H_MEM=${L4H_MEM:-16G} L4H_DOM0_MEM_MB=256 \
    "$root/tools/run-dom0.sh" "$out/density.cpio.gz" "console=hvc0 DENS_N=$n DENS_MEM=$mem ${DENS_EXTRA:-}" ${DENSITY_TIMEOUT:-900} \
    | grep -E "DENSITY|memory|panic|l4h-root: (comp|.*fault)" || true
grep -q "DENSITY: PASS" "$root/build/qemu.log"
