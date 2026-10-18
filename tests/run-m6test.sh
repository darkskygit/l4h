#!/usr/bin/env bash
# M6: dom0 restart (other guests survive) and CPU caps.
set -euo pipefail
root=$(cd "$(dirname "$0")/.." && pwd)
out=$root/build/guest
mkdir -p "$out"
"$root/tools/build-linux.sh" vmlinux >"$root/build/linux-build.log" 2>&1 \
    || { grep -E "error|undefined" "$root/build/linux-build.log"; exit 1; }
objcopy --strip-debug "$root/build/linux-l4h/vmlinux" "$root/build/linux-l4h/vmlinux.stripped"
(cd "$root/tools/l4hctl" && cargo +stable build -q --release)
"$root/tools/mkinitramfs.sh" "$out/densvm.cpio.gz" "$root/tests/guest/init-densvm" >/dev/null
cp "$root/build/linux-l4h/vmlinux.stripped" "$out/vmlinux"
"$root/tools/mkinitramfs.sh" "$out/m6test.cpio.gz" "$root/tests/guest/init-m6test" \
    "$root/tools/l4hctl/target/release/l4hctl" "$out/vmlinux" "$out/densvm.cpio.gz" >/dev/null
"$root/tools/run-dom0.sh" "$out/m6test.cpio.gz" "console=hvc0" ${M6TEST_TIMEOUT:-300} \
    | grep -E "M6TEST|restarting|l4h-root: guest|panic" || true
grep -q "M6TEST: PASS" "$root/build/qemu.log"
