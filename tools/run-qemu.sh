#!/usr/bin/env bash
# Boot the seL4 kernel + l4h-root in QEMU. KVM is used only as a fast CPU;
# VMX is hidden from the guest so seL4 runs exactly as on a cloud VM.
# Usage: run-qemu.sh <rootserver.elf> [extra qemu args...]
set -euo pipefail
root=$(cd "$(dirname "$0")/.." && pwd)
rootserver=$1; shift
objcopy -O elf32-i386 "$root/build/sel4/bin/kernel.elf" "$root/build/kernel32.elf"
exec qemu-system-x86_64 \
    -enable-kvm -cpu host,-vmx,+invtsc \
    -smp "${L4H_SMP:-4}" -m "${L4H_MEM:-4G}" \
    -nographic -serial mon:stdio -no-reboot \
    -kernel "$root/build/kernel32.elf" \
    -initrd "$rootserver" "$@"
