#!/usr/bin/env bash
# M6: dom0 restart (other guests survive) and CPU caps.
set -euo pipefail
root=$(cd "$(dirname "$0")/.." && pwd)
source "$root/tools/guest-env.sh"
out=$root/build/guest
mkdir -p "$out"
"$root/tools/build-linux.sh" vmlinux >"$root/build/linux-build.log" 2>&1 \
    || { grep -E "error|undefined" "$root/build/linux-build.log"; exit 1; }
"$l4h_objcopy" --strip-debug "$l4h_linux_out/vmlinux" "$l4h_linux_out/vmlinux.stripped"
"$root/tools/build-l4hctl.sh"
"$root/tools/mkinitramfs.sh" "$out/densvm.cpio.gz" "$root/tests/guest/init-densvm" >/dev/null
"$root/tools/mkinitramfs.sh" "$out/m6test.cpio.gz" "$root/tests/guest/init-m6test" \
    "$l4h_ctl" "$out/densvm.cpio.gz" >/dev/null
"$root/tools/run-dom0.sh" "$out/m6test.cpio.gz" "console=hvc0 L4H_TEST_SLOW=$l4h_test_slow" ${M6TEST_TIMEOUT:-$((300 * l4h_test_slow))} \
    | grep -E "M6TEST|restarting|l4h-vmmgr: guest|panic" || true
grep -q "M6TEST: PASS" "$root/build/qemu.log"
