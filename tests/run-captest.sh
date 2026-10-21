#!/usr/bin/env bash
# A guest whose vCPU is capped from the start (SC_U often exhausted when
# the guest switches processes) runs the self-test and fork-heavy loops
# without corrupting user state.  Usage: run-captest.sh [cap%] [rounds]
set -euo pipefail
root=$(cd "$(dirname "$0")/.." && pwd)
source "$root/tools/guest-env.sh"
out=$root/build/guest
mkdir -p "$out"
"$root/tools/build-linux.sh" vmlinux >"$root/build/linux-build.log" 2>&1 \
    || { grep -E "error|undefined" "$root/build/linux-build.log"; exit 1; }
"$l4h_objcopy" --strip-debug "$l4h_linux_out/vmlinux" "$l4h_linux_out/vmlinux.stripped"
"$root/tools/build-l4hctl.sh"
"$l4h_cc" -O2 -static -pthread -o "$out/selftest" "$root/tests/guest/selftest.c"
"$root/tools/mkinitramfs.sh" "$out/capvm.cpio.gz" "$root/tests/guest/init-capvm" \
    "$out/selftest" >/dev/null
"$root/tools/mkinitramfs.sh" "$out/captest.cpio.gz" "$root/tests/guest/init-captest" \
    "$l4h_ctl" "$out/capvm.cpio.gz" >/dev/null
"$root/tools/run-dom0.sh" "$out/captest.cpio.gz" \
    "console=hvc0 captest_cap=${1:-25} captest_rounds=${2:-2}" ${CAPTEST_TIMEOUT:-900} \
    | grep -E "CAPTEST|l4h-root: guest|panic" || true
grep -q "CAPTEST: PASS" "$root/build/qemu.log"
