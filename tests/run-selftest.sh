#!/usr/bin/env bash
# Boot a Linux guest that runs tests/guest/selftest.c and check the result.
# Usage: run-selftest.sh [vcpus]
set -euo pipefail
root=$(cd "$(dirname "$0")/.." && pwd)
out=$root/build/guest
mkdir -p "$out"
gcc -O2 -static -pthread -o "$out/selftest" "$root/tests/guest/selftest.c"
"$root/tools/mkinitramfs.sh" "$out/selftest.cpio.gz" "$root/tests/guest/init-selftest" \
    "$out/selftest" >/dev/null
L4H_DOM0_VCPUS=${1:-1} "$root/tools/run-dom0.sh" "$out/selftest.cpio.gz" "console=hvc0" "${SELFTEST_TIMEOUT:-120}" \
    | sed -n '/SELFTEST: start/,/SELFTEST: \(PASS\|FAIL\)/p'
grep -q "SELFTEST: PASS" "$root/build/qemu.log"
! grep -q 'deschedule .* SC failed\|SchedContext UnbindObject: object not bound' "$root/build/qemu.log"
