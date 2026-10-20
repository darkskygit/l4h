#!/usr/bin/env bash
# Build a UEFI-bootable disk image: GPT with an EFI system partition that
# holds a standalone GRUB (multiboot2) plus the seL4 kernel and l4h-root.
# This is the image format GCE imports (tar -Sczf image.tar.gz disk.raw;
# gcloud compute images create ... --guest-os-features=UEFI_COMPATIBLE).
# Usage: mkimage.sh <out.raw> [l4h-root.elf]
# L4H_SERIAL_PORT (default 0x3f8, COM1) is the console port of GRUB and
# seL4, e.g. 0x2f8 (COM2) for Supermicro serial-over-LAN.
# L4H_FB_CONSOLE=1 also shows GRUB and the seL4 console on the screen
# (kernel option fb_console), for machines whose serial port is out of
# reach but whose graphical console is not.
set -euo pipefail
serial=${L4H_SERIAL_PORT:-0x3f8}
terminal=serial fbcon=
if [ "${L4H_FB_CONSOLE:-0}" = 1 ]; then terminal="serial console" fbcon=" fb_console"; fi
root=$(cd "$(dirname "$0")/.." && pwd)
out=$1
rootserver=${2:-$root/host/target/x86_64-sel4-roottask/debug/l4h-root.elf}
kernel=$root/build/sel4/bin/kernel.elf
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

cat > "$work/grub.cfg" <<CFG
set timeout=0
serial --port=$serial --speed=115200
terminal_input serial
terminal_output $terminal
search --no-floppy --set=root --file /boot/l4h-root.elf
menuentry "l4h" {
    multiboot2 /boot/kernel.elf console_port=$serial debug_port=$serial$fbcon
    module2 /boot/l4h-root.elf
    boot
}
CFG
grub-mkstandalone -O x86_64-efi -o "$work/BOOTX64.EFI" \
    --modules="part_gpt fat search search_fs_file multiboot2 serial terminal normal configfile efi_gop video video_fb" \
    --locales= --fonts= --themes= \
    "boot/grub/grub.cfg=$work/grub.cfg"

# EFI system partition: FAT32, sized for the payload.
payload=$(( ($(stat -c %s "$kernel") + $(stat -c %s "$rootserver") + $(stat -c %s "$work/BOOTX64.EFI")) / 1048576 ))
esp_mb=$(( payload + 40 ))
dd if=/dev/zero of="$work/esp.img" bs=1M count="$esp_mb" status=none
mformat -i "$work/esp.img" -F -v L4HEFI ::
mmd -i "$work/esp.img" ::/EFI ::/EFI/BOOT ::/boot
mcopy -i "$work/esp.img" "$work/BOOTX64.EFI" ::/EFI/BOOT/BOOTX64.EFI
mcopy -i "$work/esp.img" "$kernel" ::/boot/kernel.elf
mcopy -i "$work/esp.img" "$rootserver" ::/boot/l4h-root.elf

# GPT disk: ESP from 1 MiB; GCE wants a whole number of GiB.
disk_gb=$(( (esp_mb + 2 + 1023) / 1024 ))
rm -f "$out"
truncate -s "${disk_gb}G" "$out"
printf 'label: gpt\nstart=2048, size=%sMiB, type=C12A7328-F81F-11D2-BA4B-00A0C93EC93B, name="EFI"\n' "$esp_mb" \
    | sfdisk -q "$out"
dd if="$work/esp.img" of="$out" bs=1M seek=1 conv=notrunc status=none
echo "$out: ${disk_gb} GiB, ESP ${esp_mb} MiB"
