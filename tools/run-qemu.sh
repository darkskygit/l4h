#!/usr/bin/env bash
# Boot the seL4 kernel + l4h-root in QEMU. KVM is used only as a fast CPU;
# VMX is hidden from the guest so seL4 runs exactly as on a cloud VM.
# Usage: run-qemu.sh <rootserver.elf> [extra qemu args...]
# The guest kernel and its module tree (L4H_GUEST_KERNEL, L4H_GUEST_KMODS),
# dom0's initramfs (L4H_DOM0_INITRD) and an l4h.conf (L4H_CONFIG) are
# appended to a copy of the root ELF (tools/dom0-modules.sh).
set -euo pipefail
root=$(cd "$(dirname "$0")/.." && pwd)
rootserver=$("$root/tools/dom0-modules.sh" "$1"); shift
objcopy -O elf32-i386 "$root/build/sel4/bin/kernel.elf" "$root/build/kernel32.elf"
# l4h reports its free memory to QEMU, which then drops it
# (L4H_BALLOON=0 leaves the device out).
balloon=()
[ "${L4H_BALLOON:-1}" = 1 ] && balloon=(-device virtio-balloon-pci,free-page-reporting=on,disable-legacy=on)
# L4H_QEMU_IOMMU=amd adds an AMD IOMMU, which needs the q35 machine and
# must come before the devices it translates.  NVMe always goes through
# it; virtio disks and NICs are made to (iommu_platform), not the
# balloon, whose free page reports name memory it is not given.
iommu=()
[ "${L4H_QEMU_IOMMU:-}" = amd ] && iommu=(-machine q35 -device amd-iommu,intremap=off
    -global virtio-blk-pci.iommu_platform=on -global virtio-scsi-pci.iommu_platform=on
    -global virtio-net-pci.iommu_platform=on)
exec qemu-system-x86_64 \
    -enable-kvm -cpu host,-vmx,+invtsc \
    -smp "${L4H_SMP:-4}" -m "${L4H_MEM:-4G}" \
    -nographic -serial mon:stdio -no-reboot "${iommu[@]}" \
    -kernel "$root/build/kernel32.elf" \
    -initrd "$rootserver" "${balloon[@]}" "$@"
