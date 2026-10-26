#!/usr/bin/env bash
set -euo pipefail
root=$(cd "$(dirname "$0")/.." && pwd)
source "$root/tools/guest-env.sh"
out=${1:-$root/build/guest}
mkdir -p "$out"
args=()
if [ "${L4H_ARCH:-x86_64}" = aarch64 ]; then
    args=(--target aarch64-unknown-linux-gnu -C linker=aarch64-linux-gnu-gcc)
fi
rustc +stable --edition=2024 -O -C target-feature=+crt-static "${args[@]}" \
    "$root/tests/guest/flags.rs" -o "$out/flags-test"
"$root/tools/mkinitramfs.sh" "$out/flagsvm.cpio.gz" \
    "$root/tests/guest/init-flagsvm" "$out/flags-test" >/dev/null
