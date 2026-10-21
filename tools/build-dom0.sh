#!/usr/bin/env bash
# Build the shipped dom0 package, build/efi/l4h-dom0-x86_64.tar.xz: the
# ARCH=l4h guest kernel and the dom0 userspace tools, for a dom0 of any
# distribution (see the user manual):
#   vmlinux            the kernel, without debug information: install it as
#                      l4h/dom0/vmlinux on the EFI system partition
#   config             its configuration
#   bin/l4hctl         the control CLI (static)
#   l4h.conf.example   host settings, for l4h/l4h.conf
# Needs the seL4 headers (tools/build-kernel.sh) and the Linux source
# (tools/ci-setup.sh).
set -euo pipefail
root=$(cd "$(dirname "$0")/.." && pwd)
out=$root/build/efi
mkdir -p "$out"
"$root/tools/build-linux.sh" vmlinux >"$out/linux.log" 2>&1 \
    || { grep -E "error|undefined" "$out/linux.log"; exit 1; }
"$root/tools/build-l4hctl.sh"
pkg=$(mktemp -d)
trap 'rm -rf "$pkg"' EXIT
dir=$pkg/l4h-dom0-x86_64
mkdir -p "$dir/bin"
objcopy --strip-debug "$root/build/linux-l4h/vmlinux" "$dir/vmlinux"
cp "$root/build/linux-l4h/.config" "$dir/config"
cp "$root/tools/l4hctl/target/release/l4hctl" "$dir/bin/l4hctl"
cat > "$dir/l4h.conf.example" <<'CONF'
# l4h host settings: copy to l4h/l4h.conf on the EFI system partition.
# One key=value per line; GRUB reads this file too, so no spaces around
# "=", and double quotes around values with spaces.  See the user manual
# for every key.
dom0_mem_mb=1024
dom0_vcpus=2
dom0_cmdline="console=hvc0"
# serial_port=0x2f8
# serial_irq=3
# uplink_nic=01:00.1
CONF
tar -C "$pkg" -cJf "$out/l4h-dom0-x86_64.tar.xz" l4h-dom0-x86_64
echo "$out/l4h-dom0-x86_64.tar.xz: $(( $(stat -c %s "$out/l4h-dom0-x86_64.tar.xz") >> 10 )) KiB"
