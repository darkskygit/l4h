#!/usr/bin/env bash
# Prepare an isolated, patched seL4 source tree for either architecture.
set -euo pipefail
root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
src=$root/build/seL4-src
shopt -s nullglob
patches=("$root"/kernel/patches/*.patch)
base=$(git -C "$root/kernel/seL4" rev-parse HEAD)
want=$(cat /dev/null "${patches[@]}" | sha256sum | cut -d' ' -f1)-$base
stamp=$src/.git/l4h-patches
if [ ! -d "$src/.git" ]; then
    git clone -q "$root/kernel/seL4" "$src"
fi
# (Re)apply the patches when they or the base changed since the last time.
if [ "$(cat "$stamp" 2>/dev/null)" != "$want" ]; then
    if [ -n "$(git -C "$src" status --porcelain --untracked-files=no)" ]; then
        echo "$src has uncommitted changes and kernel/patches changed: commit and" >&2
        echo "regenerate the patches (git format-patch -o kernel/patches $base), or discard them" >&2
        exit 1
    fi
    git -C "$src" checkout -q -B l4h "$base"
    if [ ${#patches[@]} -gt 0 ]; then
        git -C "$src" -c user.name=l4h -c user.email=l4h@localhost am -q "${patches[@]}"
    fi
    echo "$want" > "$stamp"
fi
