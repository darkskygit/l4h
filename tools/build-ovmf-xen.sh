#!/usr/bin/env bash
# Build OvmfXen, the UEFI firmware for Xen HVM guests, into
# build/xen/OvmfXen.fd.  Debian's ovmf package has only the generic build,
# which no longer knows Xen: it finds none of the ACPI tables hvmloader
# builds, and GRUB then has no RSDP to give seL4.
# Needs nasm, iasl and uuid-dev.
set -euo pipefail
root=$(cd "$(dirname "$0")/.." && pwd)
out=$root/build/xen
src=$out/edk2
tag=edk2-stable202502
if [ ! -d "$src/.git" ]; then
    git clone -q --depth 1 --branch "$tag" https://github.com/tianocore/edk2.git "$src"
    git -C "$src" submodule -q update --init --depth 1
fi
export PYTHON_COMMAND=python3
make -s -C "$src/BaseTools/Source/C" -j"$(nproc)" >/dev/null
(
    cd "$src"
    set +u
    source edksetup.sh >/dev/null
    set -u
    build -q -a X64 -t GCC5 -b RELEASE -n "$(nproc)" -p OvmfPkg/OvmfXen.dsc
)
cp "$src/Build/OvmfXen/RELEASE_GCC5/FV/OVMF.fd" "$out/OvmfXen.fd"
echo "$out/OvmfXen.fd"
