# l4h

l4h is a hypervisor for Linux VMs built on the seL4 microkernel. It uses
**no hardware virtualization**: guests run a paravirtualized Linux
(`ARCH=l4h`, derived from user-mode Linux), whose processes are seL4 threads
and address spaces. l4h therefore runs anywhere seL4 runs, including cloud
VMs without nested virtualization.

The user manual is in [docs/manual](docs/manual/l4h-manual.tex); render it
with `make -C docs/manual`. See [DESIGN.md](DESIGN.md) for the design, the decisions behind it and the
current status of each part.

## What works

- **Guests.**
  - Linux 6.18 guests with SMP: tested up to 4 vCPUs.
  - Debian 13 with systemd boots, gets DHCP and installs packages with
    apt.
  - 30 guests of 256 MiB run on 4 cores, with CPU caps, a vCPU balancer
    and steal time.
- **Block.** Passed-through virtio-blk and NVMe disks, or read-only and
  partial (LBA window) slices of them. I/O is zero-copy: devices DMA
  straight into guest memory.
- **Network.**
  - L2 virtual switches with MAC learning and MAC lock.
  - Multiple vNICs per VM, and a physical NIC as a switch's uplink.
  - Checksum and TSO offloads.
  - Routing and NAT are done by a VM, for example Debian with nftables.
- **Control.** dom0, the first guest, controls the host through
  `/dev/l4hctl` and the `l4hctl` CLI: create, load, start, shutdown and
  destroy VMs; console access; disks; switches; NICs; CPU caps. Destroying
  a VM returns all its memory.
- **Boot.** Multiboot under QEMU, or a UEFI disk image for GCE
  (`tools/mkimage.sh`, `tools/gce-image.sh`).
- **aarch64.** seL4, the host components and the ABI test guest run on
  QEMU `virt` at EL1. The Linux guest is still x86_64-only.

## Layout

```
DESIGN.md            design, decisions, status
docs/manual/         user manual (LaTeX; make -C docs/manual renders the PDF)
include/l4h/abi.h    guest <-> host ABI (single source of truth)
kernel/              seL4 submodule, l4h patches (kernel/patches), configs
guest/linux/         arch/l4h for Linux 6.18 and patches outside arch/
host/                Rust workspace: l4h-root (root task), l4h-vmm (per guest),
                     l4h-blkd (disks), l4h-netd (switches, NICs), shared crates
tools/               build and run scripts, l4hctl (dom0 CLI)
tests/               integration tests (QEMU) and fuzzing
third_party/         rust-sel4, sDDF submodules; patches/ for rust-sel4
```

## Building and running

The host needs:

- QEMU with KVM, which is used only as a fast CPU: VMX is hidden from the
  guest;
- gcc, Rust (stable, plus the nightly pinned by rust-sel4), cmake, ninja
  and the Python modules seL4 needs, in `.venv`;
- busybox and e2fsprogs;
- for the UEFI image: GRUB EFI, mtools, sfdisk and OVMF;
- for aarch64: `gcc-aarch64-linux-gnu` and `qemu-system-arm`.

```sh
git submodule update --init
tools/build-kernel.sh                 # patched seL4 -> build/sel4
tests/run-selftest.sh 4               # build Linux + host, boot a 4-vCPU guest self-test
```

To run your own dom0 initramfs:

```sh
tools/mkinitramfs.sh build/my.cpio.gz my-init-script
tools/run-dom0.sh build/my.cpio.gz "console=hvc0" 600   # output in build/qemu.log
```

Inside dom0, `l4hctl help` lists the control commands, for example:

```sh
l4hctl create web --kernel /vmlinux --initrd /web.cpio.gz --mem 512 --vcpus 2 \
    --disk 0 --nic 0
l4hctl console 1 --follow
```

## Tests

| Test | What it checks |
|------|----------------|
| `tests/run-testguest.sh`, `tests/run-testguest-aarch64.sh` | guest ABI and seL4 extensions (freestanding guest) |
| `tests/run-selftest.sh [vcpus]` | syscalls, memory, signals, fork, threads, futex, SMP stress |
| `tests/run-blktest.sh` (`BLKTEST_DEV=nvme`) | disk pass-through, data integrity |
| `tests/run-ctltest.sh` | VM lifecycle, console, clean shutdown, disk re-attach, windows, reclaim |
| `tests/run-nettest.sh`, `tests/run-router.sh` | switches, uplink, multi-NIC, isolation, MAC lock, NAT router VM |
| `tests/run-density.sh 30 256` | 30 guests on 4 cores stay responsive |
| `tests/run-m6test.sh` | dom0 restart, CPU caps, steal time |
| `tests/run-limits.sh` | the root task refuses what it cannot afford and recovers |
| `tests/run-monitor.sh` | serial monitor (`list`, `restart-dom0`) |
| `tests/run-uefitest.sh` | UEFI/GRUB boot image with NVMe and virtio-net, as on GCE |
| `tests/run-debian.sh`, `tests/run-workload.sh` | Debian + systemd, fio verify, parallel build, fsck (`tests/mkdebian.sh` first) |
| `tests/run-bench.sh` | micro-benchmarks |
| `tests/run-fuzz.sh` | hostile-guest virtqueue fuzzing of l4h-vmm |
