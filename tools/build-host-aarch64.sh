#!/usr/bin/env bash
# Build the host components and l4h-root for aarch64 (QEMU virt).
set -euo pipefail
root=$(cd "$(dirname "$0")/.." && pwd)
cd "$root/host"
export SEL4_PREFIX=$root/build/sel4-aarch64
export CC_aarch64_sel4=aarch64-linux-gnu-gcc CC_aarch64_sel4_roottask=aarch64-linux-gnu-gcc
cargo build -q --target aarch64-sel4 -p l4h-vmm -p l4h-blkd -p l4h-netd
for c in vmm blkd netd; do
    export "L4H_$(echo $c | tr a-z A-Z)_ELF=$root/host/target/aarch64-sel4/debug/l4h-$c.elf"
done
cargo build -q --target aarch64-sel4-roottask -p l4h-root "$@"
