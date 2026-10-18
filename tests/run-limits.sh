#!/usr/bin/env bash
# dom0 exhausts host resources (guests, ASIDs, memory); l4h-root refuses
# cleanly and recovers.
set -euo pipefail
root=$(cd "$(dirname "$0")/.." && pwd)
source "$root/tools/guest-env.sh"
out=$root/build/guest
mkdir -p "$out"
"$root/tools/build-linux.sh" vmlinux >"$root/build/linux-build.log" 2>&1 \
    || { grep -E "error|undefined" "$root/build/linux-build.log"; exit 1; }
"$l4h_objcopy" --strip-debug "$l4h_linux_out/vmlinux" "$l4h_linux_out/vmlinux.stripped"
"$root/tools/build-l4hctl.sh"
"$root/tools/mkinitramfs.sh" "$out/vm.cpio.gz" "$root/tests/guest/init-vm" >/dev/null
cp "$l4h_linux_out/vmlinux.stripped" "$out/vmlinux"
"$root/tools/mkinitramfs.sh" "$out/limits.cpio.gz" "$root/tests/guest/init-limits" \
    "$l4h_ctl" "$out/vmlinux" "$out/vm.cpio.gz" >/dev/null
L4H_MEM=${L4H_MEM:-6G} "$root/tools/run-dom0.sh" "$out/limits.cpio.gz" "console=hvc0 L4H_TEST_SLOW=$l4h_test_slow" "${LIMITSTEST_TIMEOUT:-$((900 * l4h_test_slow))}" \
    | grep -aE "LIMITS|memory:|panicked|l4h-root: comp" || true
grep -aq "LIMITS: PASS" "$root/build/qemu.log"
