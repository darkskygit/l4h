#!/usr/bin/env bash
# Micro-benchmarks: syscalls, faults, fork, pipes, block and network I/O.
# Usage: run-bench.sh [vcpus]
set -euo pipefail
root=$(cd "$(dirname "$0")/.." && pwd)
out=$root/build/guest
mkdir -p "$out"
"$root/tools/build-linux.sh" vmlinux 2>&1 | grep -E "error|undefined" && exit 1 || true
objcopy --strip-debug "$root/build/linux-l4h/vmlinux" "$root/build/linux-l4h/vmlinux.stripped"
(cd "$root/tools/l4hctl" && cargo +stable build -q --release)
gcc -O2 -static -o "$out/bench" "$root/tests/guest/bench.c"
"$root/tools/mkinitramfs.sh" "$out/benchvm.cpio.gz" "$root/tests/guest/init-benchvm" >/dev/null
"$root/tools/mkinitramfs.sh" "$out/benchsrc.cpio.gz" "$root/tests/guest/init-benchsrc" >/dev/null
cp "$root/build/linux-l4h/vmlinux.stripped" "$out/vmlinux"
"$root/tools/mkinitramfs.sh" "$out/bench.cpio.gz" "$root/tests/guest/init-bench" \
    "$out/bench" "$root/tools/l4hctl/target/release/l4hctl" "$out/vmlinux" \
    "$out/benchvm.cpio.gz" "$out/benchsrc.cpio.gz" >/dev/null
rm -f "$out/bench.img" && truncate -s 1G "$out/bench.img"
L4H_DOM0_VCPUS=${1:-1} L4H_DOM0_MEM_MB=512 \
L4H_QEMU_ARGS="-drive file=$out/bench.img,if=none,id=d0,format=raw,cache=none,aio=native -device virtio-blk-pci,drive=d0,disable-legacy=on" \
    "$root/tools/run-dom0.sh" "$out/bench.cpio.gz" "console=hvc0" 600 | grep -a "BENCH" || true
grep -aq "BENCH: done" "$root/build/qemu.log"
