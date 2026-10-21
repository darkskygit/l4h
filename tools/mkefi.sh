#!/usr/bin/env bash
# Build l4h as one UEFI application: a standalone GRUB whose memdisk holds
# the seL4 kernel and l4h-root.  dom0 comes from the EFI system partition
# the application was loaded from (DESIGN.md §10):
#   /l4h/dom0/vmlinux   dom0's kernel (an ARCH=l4h vmlinux)
#   /l4h/dom0/initrd    dom0's initramfs (optional)
#   /l4h/l4h.conf       host and dom0 settings (optional; see the manual)
# GRUB passes the three files to l4h-root as boot modules.  The partition
# is the one the application was loaded from or, when GRUB cannot tell, the
# first with /l4h/dom0/vmlinux (or /l4h/l4h.conf).  Without a dom0
# kernel there, root starts no dom0.
# Measured boot: the firmware measures this application, with seL4 and
# root in it, into PCR 4; GRUB's tpm module measures what it reads (the
# files above) into PCR 9 and its commands into PCR 8 (DESIGN.md §9.2).
# Usage: mkefi.sh <out.efi> [l4h-root.elf] [kernel.elf]
# L4H_SERIAL_PORT (default 0x3f8) and L4H_FB_CONSOLE=1 set the defaults of
# the serial_port and fb_console settings, which l4h.conf overrides.
set -euo pipefail
root=$(cd "$(dirname "$0")/.." && pwd)
out=$1
rootserver=${2:-$root/host/target/x86_64-sel4-roottask/debug/l4h-root.elf}
kernel=${3:-$root/build/sel4/bin/kernel.elf}
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

# l4h.conf is key=value lines, which GRUB sources as variable assignments:
# it reads serial_port and fb_console itself.
cat > "$work/grub.cfg" <<CFG
set timeout=0
serial_port=${L4H_SERIAL_PORT:-0x3f8}
fb_console=${L4H_FB_CONSOLE:-0}
# The partition this application was loaded from, when GRUB knows it
# (\$cmdpath is empty under OVMF), else the first one with l4h's files.
esp=
regexp --set=1:esp '^\(([^)]*)\)' "\$cmdpath"
if [ -z "\$esp" ]; then search --no-floppy --set=esp --file /l4h/dom0/vmlinux; fi
if [ -z "\$esp" ]; then search --no-floppy --set=esp --file /l4h/l4h.conf; fi
if [ -n "\$esp" ]; then
    if [ -f "(\$esp)/l4h/l4h.conf" ]; then source "(\$esp)/l4h/l4h.conf"; fi
fi
serial --port=\$serial_port --speed=115200
terminal_input serial
fbcon=
if [ "\$fb_console" = 1 ]; then
    terminal_output serial console
    fbcon=" fb_console"
else
    terminal_output serial
fi
multiboot2 (memdisk)/boot/kernel.elf console_port=\$serial_port debug_port=\$serial_port\$fbcon
module2 (memdisk)/boot/l4h-root.elf
if [ -n "\$esp" ]; then
    if [ -f "(\$esp)/l4h/dom0/vmlinux" ]; then module2 "(\$esp)/l4h/dom0/vmlinux" dom0-kernel; fi
    if [ -f "(\$esp)/l4h/dom0/initrd" ]; then module2 "(\$esp)/l4h/dom0/initrd" dom0-initrd; fi
    if [ -f "(\$esp)/l4h/l4h.conf" ]; then module2 "(\$esp)/l4h/l4h.conf" l4h-config; fi
fi
boot
echo "l4h: boot failed"
CFG
# seL4 loads only the segments: drop the debug information.
objcopy --strip-debug "$rootserver" "$work/l4h-root.elf"
grub-mkstandalone -O x86_64-efi -o "$out" --compress=xz \
    --modules="part_gpt fat search search_fs_file multiboot2 serial terminal normal configfile test regexp echo efi_gop video video_fb tpm" \
    --locales= --fonts= --themes= \
    "boot/grub/grub.cfg=$work/grub.cfg" \
    "boot/kernel.elf=$kernel" \
    "boot/l4h-root.elf=$work/l4h-root.elf"
echo "$out: $(( $(stat -c %s "$out") >> 10 )) KiB"
