#!/usr/bin/env bash
# Build the host components and the root task that embeds them, without
# their debug information, which only takes memory there; the unstripped
# ELFs stay in host/target/x86_64-sel4/debug.  dom0 is not built in (see
# host/l4h-root/build.rs).  Extra arguments go to the root task's cargo
# build.
set -euo pipefail
root=$(cd "$(dirname "$0")/.." && pwd)
cd "$root/host"
cargo build -q --target x86_64-sel4 -p l4h-vmm -p l4h-blkd -p l4h-netd
out=$root/host/target/x86_64-sel4/debug
mkdir -p "$out/stripped"
for c in vmm blkd netd; do
    # Replace the copy only when it changes: root rebuilds when it does.
    objcopy --strip-debug "$out/l4h-$c.elf" "$out/stripped/l4h-$c.elf.new"
    cmp -s "$out/stripped/l4h-$c.elf.new" "$out/stripped/l4h-$c.elf" \
        && rm "$out/stripped/l4h-$c.elf.new" || mv "$out/stripped/l4h-$c.elf.new" "$out/stripped/l4h-$c.elf"
    export "L4H_$(echo $c | tr a-z A-Z)_ELF=$out/stripped/l4h-$c.elf"
done
cargo build -q -p l4h-root "$@"
