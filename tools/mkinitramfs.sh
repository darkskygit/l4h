#!/usr/bin/env bash
# Build a small busybox initramfs for l4h guests.
# Usage: mkinitramfs.sh <out.cpio.gz> [init-script] [extra binaries for /bin...]
set -euo pipefail
out=$1
init=${2:-}
shift 2 || shift $#
busybox=$(command -v busybox)
dir=$(mktemp -d)
trap 'rm -rf "$dir"' EXIT
mkdir -p "$dir"/{bin,sbin,etc,proc,sys,dev,tmp,root,mnt,usr/bin,usr/sbin,var/run}
cp "$busybox" "$dir/bin/busybox"
for app in $("$busybox" --list); do
    [ "$app" = busybox ] || ln -sf busybox "$dir/bin/$app"
done
if [ -n "$init" ]; then
    cp "$init" "$dir/init"
else
    cat > "$dir/init" <<'INIT'
#!/bin/sh
mount -t proc proc /proc
mount -t sysfs sys /sys
mount -t devtmpfs dev /dev 2>/dev/null
echo "l4h: init running, kernel $(uname -r) on $(nproc) vCPU(s)"
exec /bin/sh
INIT
fi
chmod +x "$dir/init"
for extra in "$@"; do
    cp "$extra" "$dir/bin/"
done
(cd "$dir" && find . | cpio -o -H newc --quiet | gzip -9) > "$out"
echo "initramfs: $out ($(stat -c %s "$out") bytes)"
