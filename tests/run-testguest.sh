#!/usr/bin/env bash
# Boot l4h with the ABI test guest as dom0 and check that it passes.
set -euo pipefail
root=$(cd "$(dirname "$0")/.." && pwd)
make -s -C "$root/tests/testguest"
(cd "$root/host" && L4H_DOM0_KERNEL="$root/build/testguest/testguest.elf" L4H_DOM0_MEM_MB=64 \
    cargo build -q)
log=$root/build/testguest/qemu.log
timeout 60 "$root/tools/run-qemu.sh" "$root/host/target/x86_64-sel4-roottask/debug/l4h-root.elf" \
    </dev/null >"$log" 2>&1 || true
if grep -q "TESTGUEST: PASS" "$log"; then
    echo "testguest: PASS"
else
    sed -n '/dropped to user/,$p' "$log"
    echo "testguest: FAIL"
    exit 1
fi
