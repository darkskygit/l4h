#!/usr/bin/env bash
# Build the host components and l4h-root for aarch64 (QEMU virt), the
# components without their debug information, as in tools/build-host.sh.
set -euo pipefail
root=$(cd "$(dirname "$0")/.." && pwd)
cd "$root/host"
export SEL4_PREFIX=$root/build/sel4-aarch64
export CC_aarch64_sel4=aarch64-linux-gnu-gcc CC_aarch64_sel4_roottask=aarch64-linux-gnu-gcc
cargo build -q --target aarch64-sel4 -p l4h-vmm -p l4h-blkd -p l4h-netd
out=$root/host/target/aarch64-sel4/debug
mkdir -p "$out/stripped"
for c in vmm blkd netd; do
    aarch64-linux-gnu-objcopy --strip-debug "$out/l4h-$c.elf" "$out/stripped/l4h-$c.elf.new"
    cmp -s "$out/stripped/l4h-$c.elf.new" "$out/stripped/l4h-$c.elf" \
        && rm "$out/stripped/l4h-$c.elf.new" || mv "$out/stripped/l4h-$c.elf.new" "$out/stripped/l4h-$c.elf"
    export "L4H_$(echo $c | tr a-z A-Z)_ELF=$out/stripped/l4h-$c.elf"
done
cargo build -q --target aarch64-sel4-roottask -p l4h-root "$@"
