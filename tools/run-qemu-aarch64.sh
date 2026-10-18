#!/usr/bin/env bash
# Boot l4h on QEMU's aarch64 virt machine (TCG: no KVM for arm64 here).
# The machine starts at EL2 because the kernel loader expects to and drops
# to EL1; seL4 runs at EL1 and l4h uses no hardware virtualization.
# Usage: run-qemu-aarch64.sh <l4h-root.elf> [extra qemu args...]
set -euo pipefail
root=$(cd "$(dirname "$0")/.." && pwd)
rootserver=$1; shift
prefix=$root/build/sel4-aarch64
loader=$root/build/loader-target/aarch64-unknown-none/release/sel4-kernel-loader
"$root/build/loader-target/release/sel4-kernel-loader-add-payload" \
    --sel4-prefix "$prefix" --loader "$loader" --app "$rootserver" -o "$root/build/l4h-aarch64.img"
exec qemu-system-aarch64 \
    -machine virt,virtualization=on,secure=off,gic-version=2 -cpu cortex-a57 \
    -smp "${L4H_SMP:-4}" -m 2048 \
    -nographic -serial mon:stdio -no-reboot \
    -kernel "$root/build/l4h-aarch64.img" "$@"
