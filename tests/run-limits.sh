#!/usr/bin/env bash
# dom0 exhausts host resources (guests, ASIDs, memory); l4h-root refuses
# cleanly and recovers.
set -euo pipefail
root=$(cd "$(dirname "$0")/.." && pwd)
out=$root/build/guest
mkdir -p "$out"
"$root/tools/build-linux.sh" vmlinux 2>&1 | grep -E "error|undefined" && exit 1 || true
objcopy --strip-debug "$root/build/linux-l4h/vmlinux" "$root/build/linux-l4h/vmlinux.stripped"
(cd "$root/tools/l4hctl" && cargo +stable build -q --release)
"$root/tools/mkinitramfs.sh" "$out/vm.cpio.gz" "$root/tests/guest/init-vm" >/dev/null
cp "$root/build/linux-l4h/vmlinux.stripped" "$out/vmlinux"
"$root/tools/mkinitramfs.sh" "$out/limits.cpio.gz" "$root/tests/guest/init-limits" \
    "$root/tools/l4hctl/target/release/l4hctl" "$out/vmlinux" "$out/vm.cpio.gz" >/dev/null
L4H_MEM=${L4H_MEM:-6G} "$root/tools/run-dom0.sh" "$out/limits.cpio.gz" "console=hvc0" 900 \
    | grep -aE "LIMITS|memory:|panicked|l4h-root: comp" || true
grep -aq "LIMITS: PASS" "$root/build/qemu.log"
