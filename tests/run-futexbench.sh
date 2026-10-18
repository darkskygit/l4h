#!/usr/bin/env bash
# Run the cross-process shared-futex ping-pong benchmark inside an L4H guest.
# Usage: run-futexbench.sh [vcpus] [rounds]
set -euo pipefail
root=$(cd "$(dirname "$0")/.." && pwd)
out=$root/build/guest
mkdir -p "$out"
rounds=${2:-1000}
[[ $rounds =~ ^[0-9]+$ ]] || { echo "rounds must be an integer" >&2; exit 2; }
gcc -O2 -static -o "$out/futexbench" "$root/tests/guest/futexbench.c"
sed "s@/bin/futexbench 1000@/bin/futexbench $rounds@" \
    "$root/tests/guest/init-futexbench" >"$out/init-futexbench"
"$root/tools/mkinitramfs.sh" "$out/futexbench.cpio.gz" "$out/init-futexbench" \
    "$out/futexbench" >/dev/null
L4H_DOM0_VCPUS=${1:-1} "$root/tools/run-dom0.sh" "$out/futexbench.cpio.gz" \
    "console=hvc0" "${FUTEXBENCH_TIMEOUT:-120}" | grep 'FUTEXBENCH'
grep -aq 'FUTEXBENCH: done' "$root/build/qemu.log"
