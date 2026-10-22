#!/usr/bin/env bash
# Host-side allocator fragmentation, reclaim and container panic checks.
set -euo pipefail
root=$(cd "$(dirname "$0")/.." && pwd)
mkdir -p "$root/build/alloctest"
rustc +stable --edition=2024 --test "$root/tests/alloc.rs" -o "$root/build/alloctest/unit-tests"
"$root/build/alloctest/unit-tests"
