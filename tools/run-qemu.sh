#!/usr/bin/env bash
# Boot the seL4 kernel + l4h-root in QEMU. KVM is used only as a fast CPU;
# VMX is hidden from the guest so seL4 runs exactly as on a cloud VM.
# Usage: run-qemu.sh <rootserver.elf> [extra qemu args...]
# dom0 (L4H_DOM0_KERNEL, L4H_DOM0_INITRD) and an l4h.conf (L4H_CONFIG) are
# appended to a copy of the root ELF (tools/dom0-modules.sh).
set -euo pipefail
root=$(cd "$(dirname "$0")/.." && pwd)
rootserver=$("$root/tools/dom0-modules.sh" "$1"); shift
objcopy -O elf32-i386 "$root/build/sel4/bin/kernel.elf" "$root/build/kernel32.elf"
# l4h reports its free memory to QEMU, which then drops it
# (L4H_BALLOON=0 leaves the device out).
balloon=()
[ "${L4H_BALLOON:-1}" = 1 ] && balloon=(-device virtio-balloon-pci,free-page-reporting=on,disable-legacy=on)
exec qemu-system-x86_64 \
    -enable-kvm -cpu host,-vmx,+invtsc \
    -smp "${L4H_SMP:-4}" -m "${L4H_MEM:-4G}" \
    -nographic -serial mon:stdio -no-reboot \
    -kernel "$root/build/kernel32.elf" \
    -initrd "$rootserver" "${balloon[@]}" "$@"
