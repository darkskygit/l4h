#!/usr/bin/env bash
# Time fork+exec and CPU-bound work in a guest, uncapped and under caps.
# Usage: run-capperf.sh [caps, comma-separated; default 50]
set -euo pipefail
root=$(cd "$(dirname "$0")/.." && pwd)
out=$root/build/guest
mkdir -p "$out"
"$root/tools/build-linux.sh" vmlinux >"$root/build/linux-build.log" 2>&1 \
    || { grep -E "error|undefined" "$root/build/linux-build.log"; exit 1; }
objcopy --strip-debug "$root/build/linux-l4h/vmlinux" "$root/build/linux-l4h/vmlinux.stripped"
(cd "$root/tools/l4hctl" && cargo +stable build -q --release)
"$root/tools/mkinitramfs.sh" "$out/perfvm.cpio.gz" "$root/tests/guest/init-capperfvm" >/dev/null
cp "$root/build/linux-l4h/vmlinux.stripped" "$out/vmlinux"
"$root/tools/mkinitramfs.sh" "$out/capperf.cpio.gz" "$root/tests/guest/init-capperf" \
    "$root/tools/l4hctl/target/release/l4hctl" "$out/vmlinux" "$out/perfvm.cpio.gz" >/dev/null
"$root/tools/run-dom0.sh" "$out/capperf.cpio.gz" "console=hvc0 capperf_caps=${1:-50}" ${CAPPERF_TIMEOUT:-900} \
    | grep -E "CAPPERF|panic" || true
