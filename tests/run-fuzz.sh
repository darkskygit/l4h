#!/usr/bin/env bash
# Host-side randomised tests of guest-facing parsers.
set -euo pipefail
root=$(cd "$(dirname "$0")/.." && pwd)
(cd "$root/tests/fuzz-virtq" && cargo +stable run -q --release -- "${1:-200000}")
