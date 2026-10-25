#!/usr/bin/env bash
# Run kemu's AArch64 ABI tests inside an x86_64 l4h guest.
# Usage: run-kemutest.sh [vcpus]
set -euo pipefail
root=$(cd "$(dirname "$0")/.." && pwd)
if [ "${L4H_ARCH:-x86_64}" != x86_64 ]; then
    echo "kemu translates AArch64 userspace on x86_64 guests" >&2
    exit 1
fi
out=$root/build/guest
mkdir -p "$out"
aarch64-linux-gnu-gcc -O2 -static -pthread \
    -o "$out/kemu-abi" "$root/third_party/kemu/tests/kernel/abi.c"
"$root/tools/mkinitramfs.sh" "$out/kemu.cpio.gz" "$root/tests/guest/init-kemu" \
    "$out/kemu-abi" >/dev/null
L4H_DOM0_VCPUS=${1:-4} L4H_DOM0_MEM_MB=512 \
    "$root/tools/run-dom0.sh" "$out/kemu.cpio.gz" "console=hvc0 ${KEMU_CMDLINE:-}" \
    "${L4H_TEST_TIMEOUT:-180}" | sed -n '/KEMUTEST: start/,/KEMUTEST: \(PASS\|FAIL\)/p'
grep -q 'KEMUTEST: PASS' "$root/build/qemu.log"
! grep -q 'KEMUTEST: FAIL' "$root/build/qemu.log"
