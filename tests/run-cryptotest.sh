#!/usr/bin/env bash
# Boot an x86_64 Linux guest that runs tests/guest/cryptotest.c: dm-crypt
# with AES-NI (kernel-mode SIMD), checked against the generic C cipher,
# generic vs AES-NI throughput, and a user SIMD-state stress while the
# kernel encrypts on every vCPU.
# Usage: run-cryptotest.sh [vcpus] [stress-seconds] [bench-MiB]
set -euo pipefail
root=$(cd "$(dirname "$0")/.." && pwd)
if [ "${L4H_ARCH:-x86_64}" != x86_64 ]; then
    echo "run-cryptotest.sh: x86_64 only" >&2
    exit 1
fi
out=$root/build/guest
mkdir -p "$out"
secs=${2:-10}
mib=${3:-256}
gcc -O2 -static -pthread -o "$out/cryptotest" "$root/tests/guest/cryptotest.c"
"$root/tools/mkinitramfs.sh" "$out/cryptotest.cpio.gz" "$root/tests/guest/init-cryptotest" \
    "$out/cryptotest" >/dev/null
L4H_DOM0_VCPUS=${1:-2} "$root/tools/run-dom0.sh" "$out/cryptotest.cpio.gz" \
    "console=hvc0 cryptotest=$secs,$mib" "${L4H_TEST_TIMEOUT:-$((secs * 4 + 180))}" \
    | sed -n '/CRYPTOTEST: start/,/CRYPTOTEST: \(PASS\|FAIL\)/p'
grep -q "CRYPTOTEST: PASS" "$root/build/qemu.log"
! grep -q "WARNING:\|BUG:" "$root/build/qemu.log"
