#!/usr/bin/env bash
# A guest whose vCPU is capped from the start (SC_U often exhausted when
# the guest switches processes) runs the self-test and fork-heavy loops
# without corrupting user state.  Usage: run-captest.sh [cap%] [rounds]
set -euo pipefail
root=$(cd "$(dirname "$0")/.." && pwd)
out=$root/build/guest
mkdir -p "$out"
"$root/tools/build-linux.sh" vmlinux >"$root/build/linux-build.log" 2>&1 \
    || { grep -E "error|undefined" "$root/build/linux-build.log"; exit 1; }
objcopy --strip-debug "$root/build/linux-l4h/vmlinux" "$root/build/linux-l4h/vmlinux.stripped"
(cd "$root/tools/l4hctl" && cargo +stable build -q --release)
gcc -O2 -static -pthread -o "$out/selftest" "$root/tests/guest/selftest.c"
"$root/tools/mkinitramfs.sh" "$out/capvm.cpio.gz" "$root/tests/guest/init-capvm" \
    "$out/selftest" >/dev/null
cp "$root/build/linux-l4h/vmlinux.stripped" "$out/vmlinux"
"$root/tools/mkinitramfs.sh" "$out/captest.cpio.gz" "$root/tests/guest/init-captest" \
    "$root/tools/l4hctl/target/release/l4hctl" "$out/vmlinux" "$out/capvm.cpio.gz" >/dev/null
"$root/tools/run-dom0.sh" "$out/captest.cpio.gz" \
    "console=hvc0 captest_cap=${1:-25} captest_rounds=${2:-2}" ${CAPTEST_TIMEOUT:-900} \
    | grep -E "CAPTEST|l4h-root: guest|panic" || true
grep -q "CAPTEST: PASS" "$root/build/qemu.log"
