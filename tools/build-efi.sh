#!/usr/bin/env bash
# Build the shipped l4h UEFI applications, one per seL4 variant, into
# build/efi/l4h-VARIANT.efi.  The host components and l4h-root are the same
# in each (dom0 comes from the EFI system partition, see tools/mkefi.sh);
# the kernels differ in options that seL4 fixes at build time:
#   generic       x2APIC, PCID: QEMU/KVM and GCE
#   xapic-nopcid  xAPIC, no PCID: machines whose firmware leaves x2APIC off
#                 and CPUs without PCID (Latitude.sh m4.metal.small)
# L4H_RELEASE=1 builds release kernels and host components.
# build/efi/l4h-debug-x86_64.tar.xz holds the same build unstripped (root,
# the components and each variant's kernel), to decode fault addresses
# from a shipped application.
set -euo pipefail
root=$(cd "$(dirname "$0")/.." && pwd)
out=$root/build/efi
mkdir -p "$out"
variants=(
    "generic X2APIC ON"
    "xapic-nopcid XAPIC OFF"
)
for v in "${variants[@]}"; do
    read -r name lapic pcid <<<"$v"
    L4H_KERNEL_VARIANT=$name L4H_LAPIC_MODE=$lapic L4H_PCID=$pcid \
        "$root/tools/build-kernel.sh" >"$out/kernel-$name.log" 2>&1 \
        || { tail -20 "$out/kernel-$name.log"; exit 1; }
done
# libsel4 does not depend on these options: build the host against the
# default kernel build, as the other scripts do.
[ -d "$root/build/sel4" ] || "$root/tools/build-kernel.sh" >"$out/kernel.log" 2>&1
"$root/tools/build-host.sh"
debug=$(mktemp -d)
trap 'rm -rf "$debug"' EXIT
mkdir "$debug/l4h-debug-x86_64"
cp "$root/host/target/x86_64-sel4-roottask/debug/l4h-root.elf" \
    "$root"/host/target/x86_64-sel4/debug/l4h-{vmm,blkd,netd,trustd}.elf "$debug/l4h-debug-x86_64/"
for v in "${variants[@]}"; do
    read -r name _ <<<"$v"
    "$root/tools/mkefi.sh" "$out/l4h-$name.efi" \
        "$root/host/target/x86_64-sel4-roottask/debug/l4h-root.elf" \
        "$root/build/sel4-$name/bin/kernel.elf"
    cp "$root/build/sel4-$name/bin/kernel.elf" "$debug/l4h-debug-x86_64/kernel-$name.elf"
done
tar -C "$debug" -cJf "$out/l4h-debug-x86_64.tar.xz" l4h-debug-x86_64
echo "$out/l4h-debug-x86_64.tar.xz: $(( $(stat -c %s "$out/l4h-debug-x86_64.tar.xz") >> 10 )) KiB"
