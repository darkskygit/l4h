#!/usr/bin/env bash
# The host build version: an explicit release tag, or the source revision.
set -euo pipefail
root=$(cd "$(dirname "$0")/.." && pwd)
if [ -n "${L4H_VERSION:-}" ]; then
    printf '%s\n' "$L4H_VERSION"
else
    git -C "$root" describe --tags --match 'v*' --always --dirty 2>/dev/null || echo unknown
fi
