#!/usr/bin/env bash
# Build the static dom0 CLI for the selected Linux guest architecture.
set -euo pipefail
root=$(cd "$(dirname "$0")/.." && pwd)
cd "$root/tools/l4hctl"
case "${L4H_ARCH:-x86_64}" in
    x86_64) cargo +stable build -q --release ;;
    aarch64)
        CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER=aarch64-linux-gnu-gcc \
        CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_RUSTFLAGS='-C target-feature=+crt-static' \
            cargo +stable build -q --release --target aarch64-unknown-linux-gnu
        ;;
    *) echo "unsupported L4H_ARCH: $L4H_ARCH" >&2; exit 1 ;;
esac
