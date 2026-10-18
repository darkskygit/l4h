#!/usr/bin/env bash
# Boot an assembled AArch64 l4h image directly on Linux or macOS.
# Usage: run-image-aarch64.sh <image> [extra qemu args...]
set -euo pipefail
root=$(cd "$(dirname "$0")/.." && pwd)
image=$1; shift
source "$root/tools/aarch64-memory.sh"
accel=${L4H_QEMU_ACCEL:-tcg}
if [ "$(uname -s)" = Darwin ]; then accel=${L4H_QEMU_ACCEL:-hvf}; fi
cpu=cortex-a57
machine=virt,virtualization=${L4H_QEMU_VIRTUALIZATION:-off},secure=off,gic-version=2,highmem-ecam=off,highmem-mmio=off
if [ "$accel" = hvf ]; then
    cpu=host
    machine+=,kernel-irqchip=off
fi
exec qemu-system-aarch64 \
    -machine "$machine" -accel "$accel" -cpu "${L4H_QEMU_CPU:-$cpu}" \
    -smp "${L4H_SMP:-4}" -m "$l4h_memory_mb" \
    -nographic -serial mon:stdio -no-reboot -kernel "$image" "$@"
