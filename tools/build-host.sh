#!/usr/bin/env bash
# Build the host components and the root task that embeds them.  The dom0
# images are taken from the L4H_DOM0_* environment variables (see
# host/l4h-root/build.rs).  Extra arguments go to the root task's cargo build.
set -euo pipefail
root=$(cd "$(dirname "$0")/.." && pwd)
cd "$root/host"
cargo build -q --target x86_64-sel4 -p l4h-vmm -p l4h-blkd -p l4h-netd
for c in vmm blkd netd; do
    export "L4H_$(echo $c | tr a-z A-Z)_ELF=$root/host/target/x86_64-sel4/debug/l4h-$c.elf"
done
cargo build -q -p l4h-root "$@"
