#!/usr/bin/env bash
# Build the shipped dom0 package, build/efi/l4h-dom0-x86_64.tar.xz: the
# dom0 userspace tools, for a dom0 of any distribution (see the user
# manual; the kernel is in the UEFI application, tools/mkefi.sh):
#   bin/l4hctl         the control CLI (static)
#   share/licenses/l4hctl/LICENSE   the CLI's BSD 2-Clause license
#   l4h.conf.example   host settings, for l4h/l4h.conf
set -euo pipefail
root=$(cd "$(dirname "$0")/.." && pwd)
out=$root/build/efi
mkdir -p "$out"
"$root/tools/build-l4hctl.sh"
pkg=$(mktemp -d)
trap 'rm -rf "$pkg"' EXIT
dir=$pkg/l4h-dom0-x86_64
mkdir -p "$dir/bin" "$dir/share/licenses/l4hctl"
cp "$root/tools/l4hctl/target/release/l4hctl" "$dir/bin/l4hctl"
cp "$root/tools/l4hctl/LICENSE" "$dir/share/licenses/l4hctl/LICENSE"
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
