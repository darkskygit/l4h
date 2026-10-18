#!/usr/bin/env bash
# Build rust-sel4's kernel loader for aarch64 (with l4h's patches: entering
# EL1 kernels) and its payload tool.
set -euo pipefail
root=$(cd "$(dirname "$0")/.." && pwd)
rs=$root/third_party/rust-sel4
for p in "$root"/third_party/patches/rust-sel4/*.patch; do
    if git -C "$rs" apply --check "$p" 2>/dev/null; then
        git -C "$rs" apply "$p"
    elif ! git -C "$rs" apply --check --reverse "$p" 2>/dev/null; then
        echo "cannot apply $p" >&2
        exit 1
    fi
done
cd "$rs"
CC=aarch64-linux-gnu-gcc SEL4_PREFIX=$root/build/sel4-aarch64 cargo build -q \
    -Z build-std=core,alloc,compiler_builtins -Z build-std-features=compiler-builtins-mem \
    --target aarch64-unknown-none --release -p sel4-kernel-loader --target-dir "$root/build/loader-target"
cargo build -q --release -p sel4-kernel-loader-add-payload --target-dir "$root/build/loader-target"
