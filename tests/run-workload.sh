#!/usr/bin/env bash
# M2/M3 gates on Debian: fio with verification and a parallel build on a
# passed-through disk, fsck of the disk on the host, data intact after a
# second boot.  Needs build/debian.img (tests/mkdebian.sh).
# Usage: run-workload.sh [vcpus]
set -euo pipefail
root=$(cd "$(dirname "$0")/.." && pwd)
vcpus=${1:-4}
L4H_DOM0_MEM_MB=2048 L4H_MEM=4G DEBIAN_ROLE=workload DEBIAN_TIMEOUT=${WORKLOAD_TIMEOUT:-1800} \
    "$root/tests/run-debian.sh" "$vcpus" | grep -a "WORKLOAD"
e2fsck -fn "$root/build/debian-run.img" >"$root/build/fsck.log" 2>&1 \
    && echo "WORKLOAD: host fsck clean" || { cat "$root/build/fsck.log"; exit 1; }
DEBIAN_KEEP=1 DEBIAN_ROLE=verify "$root/tests/run-debian.sh" 1 | grep -a "WORKLOAD"
grep -aq "WORKLOAD: data intact after reboot" "$root/build/qemu-debian.log"
echo "WORKLOAD: PASS"
