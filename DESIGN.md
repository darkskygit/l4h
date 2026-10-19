# l4h: Design and implementation

l4h is a hypervisor built on seL4 that runs Linux VMs without hardware
virtualization. It targets public-cloud instances where VT-x and SVM are
unavailable or nested virtualization is disabled. Guests are paravirtualized:
the guest Linux kernel is ported to run as an ordinary seL4 user-level program,
and guest user processes run natively in their own seL4 address spaces. l4h
never emulates an instruction.

Status: **v0.1 implemented on x86_64**, including SMP Linux guests,
block and network I/O, dom0 control, CPU caps and balancing, and GCE N2
bring-up. AArch64 runs the host and native Linux guests on QEMU `virt`,
including SMP and PCI block/network drivers, with virtualization disabled (§11). The original
review decisions are recorded in §14. Future work is identified below;
§15 records the implemented guest-owned memory design.

---

## 1. Requirements

| # | Requirement | How the design meets it |
|---|-------------|-------------------------|
| R1 | No hardware virtualization. Paravirtualize, as Xen PV does. Avoid instruction emulation. | The guest kernel is a new Linux architecture, `ARCH=l4h`, that runs in ring 3 as an seL4 thread and calls seL4 directly. Guest user code runs unmodified. Its `syscall` instructions and exceptions reach the guest kernel as seL4 fault IPC. |
| R2 | x86_64 first, but friendly to aarch64. | Each layer is split into ISA-independent and ISA-specific parts (§11). seL4, rust-sel4 and sDDF already support aarch64. Nothing in the design relies on an x86-only mechanism such as segmentation, rings 1/2 or Xen-style page-table validation. |
| R3 | virtio-blk and virtio-net. Block devices are passed through. Network is multiplexed by virtual switches. | The guest runs stock Linux `virtio_blk`/`virtio_net` drivers over a new paravirtual transport, `virtio-l4h`, with no trapped MMIO (§8). All device buffers use a host-owned I/O pool; the guest copies block data between its RAM and the pool, and the device DMAs into the pool (§15.1). For networking, the hypervisor provides **only L2 virtual switches**. A VM can have several vNICs, each on a switch, and a physical NIC is just an uplink port on a switch. Routing and NAT, when wanted, are done by a VM. |
| R6 | A runtime management API, available to "dom0", the first guest started at boot. | `l4h-root` exposes a control API through a `l4hctl` paravirtual device that only dom0 receives (§9.1). dom0 has no device-driver duties. |
| R7 | Targets: QEMU with virtio devices first, then GCP. | QEMU uses virtio-net-pci, virtio-blk-pci, virtio-scsi-pci and nvme. GCE N2 is tested with virtio-net and NVMe local SSDs; gen-3+ types (C3, N4, C4) need a future gVNIC driver (§10). |
| R4 | SMP guests. | Each guest vCPU is an seL4 kernel-context thread with its own scheduling context. IPIs are seL4 notifications (§6). |
| R5 | Many guests per host, e.g. 30 guests on 4 CPUs. | seL4 MCS scheduling contexts give each vCPU a budget. Idle guests consume no CPU because the guest kernel is tickless and idle means blocked in `seL4_Recv`. A host balancer moves vCPUs between cores (§7). Each guest makes its kernel objects from its own RAM (§15). |

Non-goals for v1: running unmodified guest *kernels*, live migration, memory
overcommit or ballooning, 32-bit guest userspace, PCI passthrough into guests,
Windows guests, and any L3 networking (routing, NAT, IP filtering) inside the
hypervisor.

---

## 2. Prior art and why this shape

| Approach | Verdict |
|----------|---------|
| **Xen PV ABI on seL4.** Reuse upstream `CONFIG_XEN_PV`. | Rejected. Xen PV gives the guest control of real hardware page tables, which the hypervisor validates through `mmu_update`. It also uses a hypervisor-assisted syscall bounce, `iret` hypercalls, event channels, grant tables and xenstore. Implementing this would mean writing Xen inside the seL4 kernel. Xen PV exists only on x86, which fails R2, and upstream treats it as legacy. |
| **lguest-style `pv_ops` subarch of x86.** | Rejected. It is x86-only and needs the guest kernel in a privileged ring. lguest was removed from Linux in 4.14. |
| **UML (User-Mode Linux) on seL4.** | Rejected as a base, but used as a source of ideas. UML is in-tree and gained initial SMP in 6.18. However, its host layer (`arch/um/os-Linux`) assumes a POSIX host with libc, signals and mmap of a physmem file, and it is effectively x86-only. Its separate-address-space kernel model is the same as ours, so we reuse its approaches to `uaccess` and `pt_regs`. |
| **L4Linux** (TU Dresden / Kernkonzept, Linux on Fiasco.OC). | **The closest model.** L4Linux is a maintained out-of-tree `arch/l4` with x86-64 and arm64 subarches. The guest kernel is a user-level task, and each guest process is a microkernel task whose syscalls and faults arrive as IPC. It relies on Fiasco's *vCPU mode*, which seL4 lacks. §5 shows how seL4 threads, priorities and the kernel extensions replace it. |
| **Wombat / OK Linux** (NICTA, L4/seL4 era). | A historical precedent for Linux para-virtualized on L4-family kernels. |
| **gVisor systrap.** | Different goal: it reimplements Linux rather than running it. It confirms that trapping syscalls from unmodified binaries without VT-x is practical in production. |
| **seL4 Microkit / LionsOS / sDDF / libvmm.** | **Use sDDF as a design reference.** l4h's Rust drivers and shared queues support dynamic guest attachment; they do not link sDDF's C drivers. The libvmm hardware-virtualization VMM and Microkit's static system model do not fit dynamic cloud guests, so we do not adopt them. |

The result: **a new Linux architecture (`arch/l4h`), modelled on L4Linux and
UML, running on an MCS SMP seL4 kernel with three kernel extensions. Host
components and device drivers are written in Rust on rust-sel4.**

---

## 3. System overview

```
 ┌───────────────────── seL4 user level ───────────────────────────────────────┐
 │                                                                             │
 │  Guest A (untrusted)                  dom0 (guest #0)        Host services  │
 │  ┌──────────────────────────────┐     same as any guest,     (trusted)      │
 │  │ guest procs: one seL4 VSpace │     plus an l4hctl device  ┌───────────┐  │
 │  │ + one TCB per Linux thread   │     ──── control API ────► │ l4h-root  │  │
 │  │ (trap-syscall flag)          │                            │ resources │  │
 │  │   │ fault IPC  ▲ reply       │                            │ lifecycle │  │
 │  │   ▼            │             │                            │ ctl API   │  │
 │  │ guest kernel: ARCH=l4h vmlinux                            │ balancer  │  │
 │  │ one VSpace, one kernel TCB per vCPU                       └───────────┘  │
 │  │ virtio_blk / virtio_net over virtio-l4h                                  │
 │  └───────┬──────────────────────┘                                          │
 │          │ shared vrings + notifications                                   │
 │  ┌───────▼──────────┐                                                       │
 │  │ l4h-vmm (A)      │ one per guest, maps its I/O pool, never guest RAM     │
 │  │ virtio backends  │                                                       │
 │  └──┬────────────┬──┘                                                       │
 │     │ blk queue  │ net port rings                                           │
 │  ┌──▼───────┐  ┌─▼─────────────┐                                            │
 │  │ l4h-blkd │  │ l4h-netd      │  switches and uplink NIC drivers           │
 │  └──┬───────┘  └──────┬────────┘                                            │
 │  ┌──▼───────────┐  ┌──▼───────────────┐                                     │
 │  │ NVMe /       │  │ virtio-net PCI  │                                     │
 │  │ virtio-blk   │  │ uplink NIC(s)   │                                     │
 │  └──────────────┘  └──────────────────┘                                     │
 └─────────────────────────────────────────────────────────────────────────────┘
   seL4 microkernel (MCS, SMP, x86_64), plus l4h extensions E1–E3 (§4)
   Cloud VM hardware (QEMU/KVM, GCE …) with no VT-x exposed
```

Each `l4h-blkd` drives one attached disk. `l4h-netd` combines all switches
and uplink NIC drivers in one component. The drivers are Rust; gVNIC is
future work.

Trust: the seL4 kernel, `l4h-root`, `l4h-blkd` and `l4h-netd` form
the host TCB. dom0 is trusted for control-plane decisions (§9.1) but cannot
read other guests' memory. Each `l4h-vmm` is trusted only by its own
guest: it maps that guest's I/O pool and host-owned device, event and
queue pages, and never its RAM (§15), so a compromised VMM can harm only
that guest. Guest kernels are untrusted. Capabilities
confine them, and the hypervisor never validates guest page tables.

---

## 4. seL4 kernel configuration and extensions

Configuration:

- `KernelIsMCS=ON`: scheduling contexts with budget and period are required for
  multi-tenant fairness (§7).
- `KernelMaxNumNodes`: up to 64 cores.
- `KernelFastpath=ON`.
- `KernelFSGSBase=inst`.
- `KernelXSave=XSAVEOPT`, with `KernelXSaveFeatureSet=7` covering x87,
  SSE and AVX. The guest hides AVX-512, AMX and PKU, which seL4 does not
  enable.
- `KernelSkimWindow=OFF`, required by patch 0004's preservation of user
  RSP across syscalls. l4h targets CPUs unaffected by Meltdown (see below).

Release builds keep debug syscalls off.

**Revised (security audit, v1).** Development builds keep
`KernelDebugBuild` on, and debug syscalls need no capability: one of them,
`seL4_DebugHalt`, stops the machine. Patch 0008 therefore reserves debug
syscalls to threads with an MCP of at least `KernelDebugSyscallMinMCP`
(101 for l4h). Guest threads are confined to MCP 100, so in a guest they
are unknown syscalls in every build. `L4H_RELEASE=1 tools/build-kernel.sh`
turns the debug build off. `KernelMaxNumNodes` is 64. XSAVE covers x87,
SSE and AVX; the guest hides AVX-512, AMX and PKU, which seL4 does not
enable, from its CPU feature set.

The kernel is pinned to the seL4 16.0.0 release (`6e7c3b7`, the `16.0.0`
tag). We carry a **small, upstreamable patch set**:

### E1. `seL4_TCBFlag_trapSyscalls` (required)

On x86_64, seL4 takes the syscall number in **`rdx`**, and Linux uses `rdx`
for argument 3. On aarch64, seL4 uses `x7` and Linux uses `x8`. If a guest
process issues a Linux syscall whose 3rd argument happens to equal a valid
seL4 syscall number, seL4 would execute an seL4 syscall instead of raising a
fault.

E1 adds a new bit for the existing `seL4_TCB_SetFlags` (which today only has
`fpuDisabled`). When the bit is set, *every* syscall instruction from that
thread raises `seL4_Fault_UnknownSyscall`, which already carries all GPRs,
FaultIP, SP and FLAGS. The check goes in `c_handle_syscall` and the fastpath
entry on each arch, before the `SYSCALL_MIN..MAX` range test. The patch is
roughly 30 lines per arch, and the semantics do not depend on the ISA.

### E2. TCB FPU state access (required)

Linux needs the FPU and XSAVE state of a user thread for signal frames,
`sigreturn`, `ptrace`, core dumps and `fork`; a child must inherit its
parent's MXCSR/x87 control. seL4 keeps this state in the TCB and has no
invocation that reads or writes it.

E2 adds `seL4_TCB_ReadFPU(tcb, frame_cap, offset)` and
`seL4_TCB_WriteFPU(tcb, frame_cap, offset)`. They copy the arch FPU save area
(`XSAVE` format on x86, `V0–V31+FPSR/FPCR` on arm64) to or from a frame the
caller owns. The kernel validates the size and alignment and rejects an
invalid XSAVE header on write.

Fallback if upstream objects: user-mode trampolines in the guest vDSO run
`xsave`/`xrstor` on the thread's behalf. This needs no kernel change, but it
complicates `sigreturn` and `ptrace`.

### E3. Kernel deadline timers (required; this is the primary timer)

GCE guests are not guaranteed to have an HPET, and the PIT is too coarse to
multiplex. The MCS kernel already programs the per-core LAPIC TSC-deadline
timer, or LAPIC countdown where TSC-deadline is unavailable (the generic
timer on arm64), for budget enforcement, so the cheapest,
most portable option is to let user level share it.

E3 adds a deadline timer to each TCB:

- `seL4_TCB_SetTimer(tcb, deadline, badge)` arms or re-arms it; a zero
  deadline disarms it. The absolute deadline uses the kernel timer's ticks
  (TSC on x86, generic timer counter on aarch64).
- At the deadline the kernel signals the badge on the TCB's bound
  notification, if any, and disarms the timer.
- TCBs with armed timers sit in a per-core list ordered by deadline. The kernel programs
  the hardware with the minimum of the next budget event and the head of that
  list.
- A timer fires on the core it was last armed on. Migrated vCPUs re-arm on
  their new core.

The guest clockevent invokes `seL4_TCB_SetTimer` on its vCPU's kTCB, so
there is **no user-level timer server** and no IPC hop. No separate Timer
object is allocated or retyped. The per-core queue uses an O(n) insert.

### Not required

Unlike Fiasco vCPU mode, we need no new interrupt-injection primitive (§5.4).
We need no IOMMU, since no device is passed into a guest.

### Found during implementation

- **Patch 0006 (performance): no timer reprogram for a later deadline.**
  - `setNextInterrupt()` wrote IA32_TSC_DEADLINE on every
    scheduling-context change. Each write is a VM exit when seL4 runs
    virtualised.
  - The kernel now keeps a still-pending earlier deadline. The interrupt
    then fires early, causing at most one spurious timer interrupt and a
    reprogram.
  - Most switches only move the deadline later, so most writes
    disappear: trapped syscalls got 16× faster under nested KVM.
- **0008, debug syscalls need a minimum MCP** (see above).
- **0009, `seL4_X86_ExecuteNever`.** seL4 on x86 had no way to map a
  readable user page without execute permission. The new VM attribute sets
  XD on 4 KiB, 2 MiB and 1 GiB mappings, and boot code enables EFER.NXE when
  the CPU has it. The guest maps pages without `PROT_EXEC` with it, so
  `mmap`/`mprotect` execute permissions are enforced.
- **0010, SMP remote calls.** `handleIPI()` read a remote call's operation
  and arguments before checking that the call was pending for this core.
  A late hardware IPI for a call already handled from the lock's wait loop
  could then run the previous call's operation in the next round. With
  Linux guests unmapping memory on 4 cores this skipped TLB invalidations
  (user memory corruption) or replayed an invalidation on a reused VSpace
  root (triple fault). The flag is now checked first. This is an upstream
  bug.
- **0012, stale timer interrupts (x86, MCS).** The local APIC latches a
  timer interrupt that fires while the kernel runs with interrupts off,
  for example while a core waits for the kernel lock. It is delivered on
  the way back to user level, after the kernel has armed the next
  deadline, and seL4 handled it as a new timer event, which means another
  wait for the lock.
  - When vCPUs of the machine running seL4 are descheduled, or a core
    holds the lock for long, a lock wait can outlast the 5 ms timeslice
    that the previous entry armed. The next timer interrupt is then
    latched during the wait, and so on.
  - Every core then loops through the lock on stale interrupts and user
    threads never run again, even after the host load has gone. In CI
    (4 vCPUs on a 4-vCPU runner) this stopped the whole machine during
    the 4-vCPU self-test. Locally 4 vCPUs on 2 host CPUs hit it on every
    run.
  - A timer interrupt that finds the timer still armed (the TSC-deadline
    MSR is cleared when it fires) is now acknowledged and dropped before
    the lock is taken. `ksArmedDeadline` from 0006 is checked first, so
    interrupts that are due skip the MSR read. This is an upstream bug.
    On aarch64 the timer interrupt is level-triggered, so setting a new
    deadline withdraws it.
  - seL4 still needs its vCPUs to run together: the kernel lock is a FIFO
    (CLH) lock, and remote calls wait for every target core. With 4 vCPUs
    on 2 host CPUs and no pause-loop exiting (nested virtualisation),
    guests make progress but very slowly.
- **0013, MCS reconfiguration of a remote thread.**
  `SchedControl_ConfigureFlags` on an SC whose runnable thread is on
  another core postponed the thread to the release queue when its new
  refill was not ready, then enqueued it in that core's ready queue too:
  a failed assertion in debug kernels, corrupt queues otherwise. l4h-root
  does this whenever it changes the budget of a capped vCPU that used it
  up. Only a schedulable thread is enqueued now; for a postponed one the
  remote core is still told to reprogram its timer. This is an upstream
  bug.
- **0014, x86 timer calibration (MCS).** Every core measured the TSC and
  APIC timer rates again at boot and overwrote the shared values, each
  from a single PIT wraparound. A virtualised PIT can end that wait
  early: on GCE about one boot in three had a core measure the TSC at
  1 MHz or the APIC timer at ~490 kHz.
  - A bad last core gave every core a wrong clock. A ratio of 0, which
    means TSC deadline mode, stopped the timer on N2, which has no TSC
    deadline mode: the whole machine stalled.
  - A bad boot core left the root task with an initial budget below the
    minimum, failing `refill_sufficient` in `switchToThread`.
  - The rates are now measured once, on the boot core, as the median of
    5 wraparounds each, and the APIC ratio is at least 1. This is an
    upstream bug.


- **0004, x86_64 user RSP across syscalls.** seL4's fast `syscall` entry
  never saved the user RSP, and the `sysretq` path cleared it. Only the
  libsel4 convention of keeping RSP in RBX made this work. Trapped Linux
  threads returned with RSP = 0. The patch saves RSP on entry, through a
  per-node scratch slot, and restores it on `sysretq`. It requires
  `KernelSkimWindow=OFF`, the Meltdown mitigation. The CPUs l4h targets,
  such as GCP C3/N4/C4 Sapphire Rapids and later and recent AMD parts, are
  not affected by Meltdown.
- **0005, SMP IPI barrier.** `ipi_wait()` reset its arrival counter and
  released waiters with two unordered plain stores, and GCC emits them in
  the wrong order. A following remote call (TLB shootdown, remote TCB stall)
  could then lose arrivals, leaving every core spinning in `ipi_wait()`.
  This is an upstream bug that SMP l4h guests hit within seconds.
- E1 (`trapSyscalls`) is our fix for seL4 issue #477, which is still open
  upstream. Without it, Linux syscalls whose syscall-number register
  happens to hold a valid seL4 syscall number are executed as seL4
  syscalls. Several of those (NBSend\*, Yield, SetTLSBase, the debug calls)
  do not fault without capabilities. `tests/testguest` sweeps all seL4
  syscall numbers.

**Decision:** l4h carries E1–E3 and the fixes above as patches in
`kernel/patches/`, and proposes them upstream through the seL4 RFC process. They are designed as independent
commits, and each has its own sel4test cases.

**Verification note:** the MCS SMP x86_64 configuration is not covered by the
seL4 proofs today, with or without E1–E3. l4h still benefits from seL4's
design and its capability isolation, but it does not claim a verified
configuration on x86. aarch64 is the better long-term path to proof coverage.

---

## 5. The guest kernel: `ARCH=l4h`

**Implementation note.** `arch/l4h` is derived from `arch/um` (UML) and
`arch/x86/um`. UML already provides most of what is needed:
- the separate-address-space kernel;
- x86 signal frames, ptrace and the syscall table;
- software page tables with accessed/dirty emulation and a batched
  "TLB sync" to the host.

The UML host layer (`os-Linux`) is replaced by `os-sel4`, which is built
against libsel4. The Kconfig keeps `CONFIG_UML` defined, so generic code
treats l4h like UML (no hardware AVX paths, and so on). The tree is
self-contained. Patches outside `arch/l4h` handle kbuild's UML-style UAPI
headers and restricted DMA pools without a device tree (§15.1).

### 5.1 Layout in the Linux tree

```
arch/l4h/
  kernel/   boot, setup, irq, time, smp, process (switch_to), signal glue, ptrace glue
  kernel/skas/   software page tables, uaccess and process/syscall glue
  os-sel4/  freestanding boot, event and user-thread operations against libsel4
  drivers/  virtio_l4h transport, hvc console, l4hctl
  include/  kernel, shared and UAPI headers
  x86/      x86_64 signal frames, TLS, syscall table, vDSO and register glue
  arm64/    native AArch64 ABI, atomics, context switching and module relocations
  configs/defconfig
guest/linux/patches/              changes outside arch/l4h
```

`tools/build-linux.sh` builds with `ARCH=l4h`, using `SUBARCH=x86_64`
by default or `SUBARCH=aarch64` when `L4H_ARCH=aarch64`. Each subarchitecture
uses its native UAPI headers, syscall table and signal-frame layout. AArch64
exports FP/ASIMD HWCAPs, preserves TPIDR_EL0 and all 31 general registers,
and supplies a versioned vDSO with the signal-return trampoline. Its first
syscall argument is saved separately from the x0 return value, including
across restarted syscalls. Executable mappings receive instruction-cache
maintenance through seL4. Both ISAs use the common memory, scheduling and
virtio-l4h code. The Linux base is 6.18; patches outside arch/l4h are in
`guest/linux/patches`.

### 5.2 Address spaces

- **Guest-kernel VSpace**, one per guest:
  - the kernel image;
  - a **direct map of frame-backed guest RAM**: guest-physical address (GPA) *g* maps
    to `PAGE_OFFSET + g`, using 4 KiB frames (see §5.3);
  - vmalloc/module area;
  - per-vCPU event pages;
  - virtio-l4h device pages.

  All vCPU kernel TCBs of the guest share it.
- **Guest-process VSpaces** cache Linux `mm_struct` mappings in a fixed
  pool of host-provided VSpace roots (§5.3). The guest makes their page
  tables and mapping slots from its own RAM. They hold only user mappings,
  plus vDSO and vvar.
- The two kinds are disjoint, and that disjointness is what isolates the guest
  kernel from guest user code. Every seL4 mapping is user-accessible, so the
  kernel's memory must not be mapped into a process VSpace at all. Kernel and
  user VSpaces can use the same numeric addresses independently. The
  x86_64 guest kernel is linked at `0x60000000`; host-provided special
  pages start at `L4H_SPECIAL_BASE`. Separate VSpaces enforce isolation.
- **uaccess** (`copy_{to,from}_user`, `get_user` and the rest) walks the
  guest's Linux page tables in software and goes through the direct map, as
  UML and L4Linux do. A missing page calls `handle_mm_fault` inline. The cost
  is a few hundred ns per page-crossing access, which is acceptable.
  - Each page is looked up under `mmap_lock` (VMA present) and the PTE
    lock, checked like a hardware access (present, readable but not
    `PROT_NONE`, or writable) and pinned with a page reference for the
    copy, so an unmap on another vCPU cannot free and reuse it meanwhile.
  - In atomic context (futex operations, `*_nofault`) `mmap_lock` is only
    tried and nothing faults: callers fault the page in and retry.

### 5.3 Memory management: Linux page tables mirrored into seL4

- The guest kernel keeps ordinary Linux 4-level software page tables. The PTE
  format belongs to `arch/l4h`, and A/D bits are maintained in software.
- The seL4 VSpace of each mm is a **cache** of those page tables, filled
  lazily:
  1. A user access to an unmapped address raises an seL4 VMFault, which
     reaches the vCPU kernel TCB.
  2. The kernel runs `handle_mm_fault`.
  3. It copies the frame cap into a slot of that mm's CNode and calls
     `seL4_X86_Page_Map` with the rights from the PTE.
  4. It replies to the fault.

  Optionally, the kernel pre-maps from `set_pte_at` for pages it has just
  faulted in.
- **Software dirty and accessed bits.** Shared writable file pages are mapped
  read-only until the first write, and the write fault sets `pte_dirty`. A
  page counts as young when it is mapped, and aging is done by unmapping. This
  is the same approach Linux uses on architectures without hardware A/D
  bits.
- **Cap-slot tracking.** Every seL4 mapping needs its own frame-cap copy. PTE
  pages are allocated at order 1, and the second 4 KiB holds a parallel
  512-entry array of CNode slot indices, so unmapping is O(1).
- **Bulk operations.** Exit, `exec` teardown, `fork` COW downgrade and large
  `munmap`/`mprotect` (above a threshold) do not unmap page by page. They
  delete the mm's seL4 VSpace and CNode together, which takes a few
  preemptible kernel operations, and let faults repopulate it. In `fork`, both
  parent and child refault their working sets lazily, so fork followed by exec
  stays cheap.
- **TLB coherence** across cores is seL4's job: unmap shoots down the ASID's
  TLB entries. With several vCPUs, `flush_tlb_*` syncs synchronously, so a
  page is never freed while another vCPU can still reach it. The fault-path
  mapping runs under the PTE lock.

  *Revised (v1):* every vCPU also syncs `current->mm` before returning to
  user, and a sync takes the whole pending range of the mm at once. A
  flush may therefore find that its range has already been taken by a
  sync on another vCPU that has not yet applied it. Syncs of an mm are
  serialised by a per-mm lock, held from taking the range until it is
  applied, so that such a flush waits for that sync instead of returning
  while the old mappings are still in the VSpace. Without the lock, the
  freed pages were reused while another thread could still write to them
  through the old mapping. This showed up as `smp_mm_stress` data errors
  on oversubscribed hosts. The lock also keeps two syncs from racing on
  one PTE, where one could map a value that the other had already
  replaced and marked synced.
- **VSpace pool.** x86_64 seL4 has only 4096 ASIDs in total (8 pools of
  512). Each guest therefore gets a fixed pool of pre-assigned VSpace roots,
  and the guest kernel recycles them least recently used first. An mm that
  loses its VSpace simply refaults: every successful page fault installs
  the PTE directly.
- **CSpace naming.** A CNode capability held directly in a root slot cannot
  be named by a 64-bit CPtr, because seL4 keeps resolving through it.
  Capabilities that must be named, such as the root CNode itself and user
  threads' CSpaces, live in a "meta" CNode in root slot 1 (see `abi.h`).
- **Guest RAM.** `l4h-root` allocates 2 MiB blocks from anywhere in free
  host memory. The guest receives frame caps for the boot blocks and
  Untyped copies for the rest, which it retypes into frames or its own
  kernel objects (§15). There is no separate object quota or DMA RAM
  chunk table: devices see only the host-owned I/O pool. Running out of
  RAM for objects is a guest-local OOM. Destroy revokes the original
  block Untypeds to reclaim everything the guest made.
- Huge pages are not in v1. A later version can back hugetlb/THP regions with
  2 MiB frame caps from dedicated chunks.

### 5.4 Execution model: vCPUs, tasks, syscalls and interrupts

Each guest **vCPU** has:

- a **kernel-context TCB** (`kTCB`) at priority `P_K`, running in the
  guest-kernel VSpace, with its own scheduling context `SC_K`;
- a fault/IPC **endpoint** `EP_v` and a bound **notification** `N_v`;
- a user scheduling context `SC_U`, bound to whichever guest task runs on that
  vCPU.

Each **guest task that has a user context** (each Linux thread) has a
**user-context TCB** (`uTCB`) at priority `P_U < P_K`:

- its VSpace is the mm's VSpace;
- its CSpace is empty;
- `trapSyscalls` (E1) is set;
- its fault handler is a badged copy of `EP_v` for the vCPU it runs on, and
  the badge is the task ID.

uTCBs are created lazily from object blocks in the guest's own RAM. Keeping one TCB per thread,
rather than one per vCPU, means seL4 holds each thread's register and FPU
context, so a context switch never copies FPU state.

```
 user thread T runs on core c ───syscall──► seL4 ──UnknownSyscall fault IPC──► kTCB(v)
   (uTCB_T, SC_U, prio P_U)                                                     │
                                                                  Linux syscall │
   ◄──────────── reply (new regs) ◄── seL4_ReplyRecv(EP_v) ◄────────────────────┘
```

**Return to user, switching from task A to task B:**

```
seL4_TCB_SetSchedParams(uTCB_A, …, SC=null)
seL4_TCB_SetSchedParams(uTCB_B, …, SC=SC_U, fault_ep=EP_v[badge B])
seL4_ReplyRecv(B's reply object, …)   (or seL4_TCB_Resume if B was preempted)
```

The second call also migrates B to v's core, because in MCS the core follows
the scheduling context.

**Reply objects.** MCS needs a reply object for every task blocked in a fault.
The guest kernel keeps a pool: it calls `Recv` with a free reply object,
attaches that object to the faulting task, and refills the pool.

**Interrupts and preemption:**

- **While a user task runs.** kTCB is blocked in `Recv` on `EP_v`. A signal on
  `N_v` wakes it, and because `P_K > P_U` it preempts the uTCB at once. The
  preempted uTCB is not in a fault state. The kernel either leaves it alone,
  so it resumes when kTCB blocks again, or unbinds `SC_U` to deschedule it and
  saves nothing, because the registers stay in the TCB.
- **While kTCB itself runs guest-kernel code.** Notifications cannot
  interrupt a running thread asynchronously. Interrupt sources therefore
  **set a bit in the vCPU's event page** (a shared frame, as with Xen's
  `evtchn_pending`) and then signal `N_v`. The guest checks the event word, a
  cheap memory load, in `local_irq_enable`, `preempt_enable` and
  `cond_resched`, and before blocking. The E3 timer only signals `N_v`, so
  the same check also compares `rdtsc` against the vCPU's armed deadline.
  The default guest configuration uses `CONFIG_PREEMPT_VOLUNTARY`.
- **Future backstop.** The proposed upcall injection for a kTCB that runs
  too long without polling is not implemented. It would require suspending
  the kTCB and changing its registers to enter an interrupt handler, with
  a safe way to construct the interrupt frame under the guest-owned
  memory model. Current interrupt delivery relies on guest polling.
- **Idle.** kTCB calls `seL4_Recv(EP_v)`. Idle consumes no CPU, which is what
  makes 30 mostly-idle guests on 4 cores practical.

**Exceptions.** A UserException fault carries the vector and error code. We
map them to signals, for example #GP to SIGSEGV, #UD to SIGILL and #DE to
SIGFPE. `int3` is used for SIGTRAP and ptrace breakpoints. `int 0x80` is
rejected because there is no 32-bit compat. The legacy vsyscall page is not
supported; we use `vsyscall=none`, which is the modern default.

**TLS and FS/GS.** `arch_prctl(ARCH_SET_FS)` maps to `seL4_TCB_SetTLSBase` or
WriteRegisters `fs_base`. With `KernelFSGSBase=inst`, userspace
`wrfsbase` works natively, and we advertise HWCAP2_FSGSBASE.

**Time.** The clocksource is the TSC, read with `rdtsc` in the kernel and in
the vDSO. The vvar page is mapped into every process, and `clock_gettime`
never traps. `l4h-root` passes the TSC frequency, taken from seL4 bootinfo.
The clockevent is a per-vCPU one-shot deadline armed with
`seL4_TCB_SetTimer(kTCB, deadline, L4H_BADGE_TIMER)`, which signals `N_v`.

*Revised (v1):* the wall clock at boot comes from `L4H_HC_GET_TIME`:
`l4h-root` reads the CMOS RTC once at boot, and each VMM adds the TSC time
since. Guests may correct it with NTP. Without an RTC it
is 0, the epoch.

**Signals.** The kernel builds the native x86 `rt_sigframe` in user memory
through the direct map. It fetches FPU and XSAVE state with E2, then rewrites
the uTCB's registers with `WriteRegisters`. `rt_sigreturn` reverses this.

**ptrace and coredump** use ReadRegisters/WriteRegisters and E2.
`PTRACE_ARCH_PRCTL`, and the fs/gs base in `PEEKUSER`/`POKEUSER` and
`GETREGS`/`SETREGS`, act on the tracee: its fs/gs base is read back from
its TCB (user code may have changed it with `wrfsbase`), and a new value
is kept in its saved registers, marked dirty, until it resumes.

### 5.5 Calls from the guest to the host

The guest kernel makes plain seL4 invocations on capabilities it holds:

- Map and Unmap;
- TCB, SchedContext Bind and Unbind;
- Signal;
- Call to its VMM endpoint for device configuration, power-off and reboot.

**Direct seL4 invocations rely on capability checks.** The CSpace that
`l4h-root` hands the guest contains:

- the guest's own frames and its Untyped;
- its vCPU SCs, which it can bind but not configure, since it holds no
  SchedControl;
- its own endpoints;
- an MCP no higher than `P_K`.

Host hypercalls and device requests still validate their arguments,
including pool addresses, device indices and lifecycle state.

---

## 6. SMP guests

- vCPU *v* consists of `kTCB_v`, `SC_K,v`, `SC_U,v`, `EP_v` and `N_v`. The
  boot vCPU starts at the ELF entry, and secondaries start at
  `l4h_secondary_entry` when `l4h-root` resumes them at the guest's request.
  CPU hotplug is just suspending and resuming those TCBs.
- **IPIs.** The sender sets a reason bit in the target's per-CPU IPI word and
  the pending bit in the target's event page, then calls `seL4_Signal(N_target)`.
  seL4 delivers the signal across cores with a hardware IPI when needed.
- **TLB shootdown for guest mms** is seL4's job (§5.3), so
  `flush_tlb_mm_range` only unmaps or invalidates the mirror and never IPIs
  other vCPUs. This is a large saving over a real VM.
- **Lock-holder preemption.** The guest currently uses native qspinlocks
  and qrwlocks. A notification-based paravirtual spinlock slow path is
  future work; a waiter currently spins if the lock holder is descheduled.
  Keeping a guest's vCPUs on distinct cores where possible reduces this
  cost, but oversubscription can still make it expensive.
- `steal_time` comes from `seL4_SchedContext_Consumed` deltas, which the
  balancer publishes in the event page. Guest CFS can then account for steal.

---

## 7. Host scheduling and many guests per core

- **Priorities**, global on each core:

  | Band | Components |
  |------|------------|
  | highest | l4h-netd (switches and NIC drivers), l4h-blkd (disk drivers) |
  | | l4h-vmm instances |
  | | guest `P_K` |
  | lowest | guest `P_U` |

  Every band has MCS budgets, so no component can starve the bands below it.
- **Per-vCPU budgets.**
  - `SC_K` and `SC_U` each get period T (default 5 ms) and a budget derived
    from the guest's CPU cap. Under a cap they split one budget (see below);
    CPU weights are not implemented.
  - Sums across a core may exceed 100%, which is overcommit. Inside the guest
    band, threads with budget left are scheduled round-robin.
  - Caps limit each vCPU's combined kernel and user CPU time, with the
    reconfiguration allowance described below.
- **Placement and balancing.** seL4 does no load balancing, so `l4h-root`
  runs a balancer thread at low frequency (about 100 ms):
  - It reads `Consumed` for every SC.
  - It moves vCPUs between cores by re-running `SchedControl_Configure` on
    another core's SchedControl cap. `SC_K` and `SC_U` of a vCPU always move
    together.
  - It keeps each guest's vCPUs on distinct cores where possible, which limits
    lock-holder preemption.
  - SMT sibling placement policy is future work.
- **Fairness caveat.** Because `P_K > P_U` for every guest, a guest's kernel
  work can briefly pre-empt another guest's user work, bounded by `SC_K`
  budgets. If measurements show this matters, the balancer can lower `P_K` for
  vCPUs that burn kernel time, or we can trial `seL4_SchedContext_YieldTo`
  from interrupt senders.
- **Memory sizing for 30 guests.** Guest RAM is reserved in full at creation.
  Guest-created CNode slots, page tables and TCBs come from that RAM. Host
  overhead is the I/O pool plus about 6 MiB of objects per guest (§15.2).
  Ballooning (virtio-balloon to revoke frames) and free-page reporting come
  later.

**Revised during implementation (v1).**

- **Placement and balancing.** New guests are placed round-robin. Every
  100 ms `l4h-root` reads `SchedContext_Consumed` for both SCs of every
  vCPU.
  - A vCPU is *busy* if it got at least half of its fair share among the
    vCPUs active on its core. Measuring this way still counts a starved
    vCPU on a crowded core.
  - The balancer evens out the number of busy vCPUs per core. It moves one
    vCPU per period from the core with the most to the core with the
    fewest, and only when that strictly lowers the maximum.
  - It prefers guests with no vCPU on the target core. A moved vCPU stays
    put for 1 s.
  - The interval is measured, not assumed: root work such as building a
    guest can delay a period.
- **CPU caps** (`vm_set_sched`, `l4hctl sched`) give a vCPU one budget,
  a percentage of the 5 ms period, split between its SC_K and SC_U, so
  that the vCPU as a whole stays within the cap. Weights are not
  implemented.
  - Each SC needs at least 200 µs, well above seL4's minimum budget, so
    caps below 8% get a longer period: 1% is 400 µs every 40 ms.
    AArch64 capped vCPUs use at least a 40 ms period to amortize emulated
    GIC timer delivery: 25% gets a shared 10 ms budget every 40 ms.
  - The split adapts. When, in a balancer period, either SC used at
    least 90% of its part, the budget is split in proportion to what each
    used (each keeps at least 200 µs). A user-bound vCPU capped at 25%
    therefore ends up with most of the budget in SC_U, and a syscall-bound
    one the reverse.
    - *Revised:* the first rule gave a saturated SC "what the other used
      plus a quarter". A vCPU alternates between its kernel and user
      threads, so when one SC runs out the other stalls too, and its use
      understates its demand. The split flipped every period (2300/200 µs,
      then about 400/2100 µs). A fork/exec-heavy guest capped at 50% used
      about 20% of its budget: 200 fork+exec took 16 s instead of about
      1 s. The usage ratio survives that throttling.
    - **Remaining cost.** Splitting one budget between two strictly
      alternating SCs still loses capacity, because whichever SC runs out
      first in a period stalls the vCPU while the other's budget goes
      unused. In `tests/run-capperf.sh` (release seL4, nested KVM, noisy),
      200 fork+exec at a 50% cap took 3.9 s with the proportional split.
      With both SCs given the whole budget, an experiment that breaks the
      cap, they took 1.8 s, against 0.4–0.7 s uncapped. So the split still
      costs about 2×, and something else, not yet found, costs about 2×
      more.
  - seL4 keeps the sliding-window limit of a reconfigured SC whose
    thread is runnable; an SC that is idle or unbound starts afresh with
    its new budget. Since the split changes at most once per balancer
    period, that is at most one period's budget extra per change
    (5% of the cap at a 5 ms period, 40% at a 40 ms period).
    Samples covering more CPU time than elapsed (consumption reported
    late when the root task ran late) do not move the split.
  - seL4 refuses to bind an SC whose budget is exhausted to a blocked
    thread. The guest therefore waits for the refill, on 100 µs timers,
    when it moves a capped SC_U to another process: not with
    `seL4_Yield`, which charges SC_K all of its remaining budget.
  - Under a cap, `P_K > P_U` no longer means that a user context runs
    only once kTCB blocks: when SC_K is out of budget and SC_U is not,
    the context bound to SC_U runs. The guest therefore writes a
    preempted context's registers before it binds SC_U to it, never
    after (`tests/run-captest.sh`).
  - Their SCs hold 16 refills (512-byte objects). With seL4's minimum of
    two, a budget used in many short pieces, as a vCPU that is preempted
    and blocks often uses it, keeps being merged into later refills.
- **Steal time.**
  - The guest accumulates its idle time in the event page (`idle_ticks`).
  - Each balancer period, `l4h-root` computes, per vCPU,
    `steal = wall − consumed − idle`, counting from the vCPU's first run.
    It publishes the result monotonically as `steal_ns`.
  - The guest returns it as Linux's paravirt steal clock
    (`CONFIG_PARAVIRT_TIME_ACCOUNTING`), so it shows as `st` in /proc/stat
    and top. A guest capped at 25% sees about 75% steal
    (`tests/run-m6test.sh`). `l4hctl top` shows each VM's CPU use over the
  last period.
- **Memory.** Free memory is a set of power-of-two Untyped blocks,
  managed buddy-style. An allocation splits the smallest block that fits;
  free halves merge lazily (§15.2). Guest RAM is allocated as independent
  2 MiB blocks. There is no separate object quota: a guest costs its RAM,
  its I/O pool (4 MiB by default) and about 6 MiB of host objects, so a
  256 MiB guest costs about 266 MiB.

- **Limits.** `l4h-root` checks a request's memory, ASIDs and root CNode
  slots before starting it, and refuses with ENOMEM instead of exhausting
  itself.
  - x86_64 seL4 has 4096 ASIDs, and a guest holds 66 of them: its
    VSpace, its pool of 64 process VSpaces, and its VMM. That allows about
    61 guests per host; `MAX_GUESTS` is 64.
  - `tests/run-limits.sh` fills the host and checks that it recovers.
  - Every size and address in a control request is range-checked before
    any arithmetic on it (no overflow), and a request that fails half way
    (e.g. no contiguous block large enough for the I/O pool) gives back what it
    took. Free root CNode slots are coalesced ranges, so the slot ranges
    of vNIC ports and disk DMA buffers are reused after a guest is
    destroyed, and splitting a memory block no longer keeps a slot for
    the parent.
  - Interrupt vectors are split: NICs 0–3, the serial monitor 8, disks
    16–79 (at most 64 disks are used).
- **Dashboard.** With a linear framebuffer from the boot loader (GRUB on
  UEFI; seL4 patch 0011 makes the multiboot2 header ask for one),
  `l4h-root` draws a read-only status screen every second: host, VMs,
  disks, switches. Changed character cells only; 8x8 public-domain font.
- **Serial monitor.** `l4h-root` reads COM1 (I/O ports and IOAPIC IRQ 4)
  and runs `help`, `list`, `info`, `destroy <vm>` and `restart-dom0`. The
  kernel keeps using the port for output (`tests/run-monitor.sh`).
- **dom0 restart.** If dom0 reboots or crashes, `l4h-root` destroys it and
  boots it again from the boot image. Other guests keep running, and the
  new dom0 sees them through the control API (`tests/run-m6test.sh`).
- **Density gate.** `tests/run-density.sh` runs 30 guests of 256 MiB on 4
  cores (16 GiB QEMU) with 15 of them spinning. All 30 boot in about 45 s,
  every guest answers on its console within 0.5 s, worst case, and
  destroying them returns all memory.

---

## 8. Devices

### 8.1 Transport: `virtio-l4h` (guest side, about 600 LOC)

Linux virtio already allows custom transports; `virtio_mmio`, `virtio_ccw`
and `vdpa` are existing examples. `virtio-l4h` provides `virtio_config_ops`
for each device, with no trapped accesses:

- **Device page.** A shared 4 KiB frame laid out like the virtio-mmio register
  file plus the device config space. Reads are plain loads.
- **Synchronous register writes** (status, feature select, queue setup,
  `QueueReady`, reset) go through `seL4_Call(vmm_ep, op, args)`. They are
  rare.
- **Queue kick.** The guest sets a bit in the device page, then calls
  `seL4_Signal(vmm_notif)`. This is asynchronous, and the guest honours
  `VRING_AVAIL_F_NO_NOTIFY` and event-idx suppression, so most kicks are
  avoided.
- **Device interrupt.** The VMM sets an event bit in the target vCPU's event
  page, then signals `N_v`. The `l4h-irq` irqchip maps event bits to Linux
  IRQs. Interrupt affinity is set through the same `seL4_Call`.
- Discovery happens through the boot-info device table, not PCI or DT
  probing. Vrings and every buffer live in the guest's host-owned I/O
  pool and are addressed by GPA (§15.1). `VIRTIO_F_ACCESS_PLATFORM` is
  required.

### 8.2 Block, shared controllers and exclusive raw windows

A physical virtio-blk, virtio-scsi or NVMe PCI controller has one persistent blkd.
`BLK_ATTACH` attaches a finite, 4 KiB aligned, non-overlapping LBA window
to a guest before boot. Every window appears as a separate virtio-blk
volume starting at sector zero. There is no image format, host filesystem,
thin provisioning or copy-on-write.

- Guest `virtio_blk` → l4h-vmm → per-client shared queue → blkd → device.
- Each client has its own immutable pool/window/permission configuration,
  channel and completion notification. The controller supports 64 clients.
- Descriptor buffers must lie in that client's host-owned I/O pool. The
  driver translates pool GPAs to physical addresses; the device never sees
  guest RAM. Guest swiotlb performs the remaining payload copy.
- l4h-root and blkd both enforce window exclusivity. Request validation uses
  checked arithmetic and verifies block alignment, direction, permissions,
  operation and pool bounds. Hardware request tags are allocated by blkd,
  so identical IDs from different clients cannot collide.
- Submission runs round-robin, with bounded work per client and reserved
  completion capacity for outstanding requests.
- Destroy stops the client's VMM, drains that client's submitted work, then
  releases its mapping and pool. It does not reset a shared controller.
  On a five-second timeout, root suspends the driver and quarantines the
  controller. Its DMA memory remains allocated, and every affected guest
  pool is pinned when destroyed. No new attachment or provisioning write
  is accepted until host reboot.
- dom0 can seed or explicitly zero UNATTACHED ranges using `BLK_WRITE` and
  its ioctl staging buffer (up to 1 MiB per call). A zero-length call flushes
  the controller. Provisioning uses a private 1 MiB DMA scratch area and
  never permits writes overlapping a client, including read-only clients.
- A virtio-scsi controller, modern or legacy (0.9.5 in I/O BAR 0, as on
  GCE), is one disk: blkd scans LUN 0 of each target and drives the first
  direct-access unit, as NVMe drives namespace 1. Requests become
  READ/WRITE(10 or 16) and SYNCHRONIZE CACHE commands, split when they
  exceed the device's `seg_max` or `max_sectors`. Write protection and the
  write cache come from MODE SENSE; unit attentions and busy statuses are
  retried. Indirect descriptors are used when offered; otherwise each
  command slot owns a fixed run of ring descriptors.
- `DISK_INFO` additionally returns capacity and attachment count;
  `BLK_INFO` enumerates `(vm, start, sectors)` with a cursor. The legacy
  owner field identifies the first attachment, or -1 when none exist.

`tests/run-sharedblk.sh` checks concurrent clients, overlap and bounds
rejection, provisioning exclusion, destroy during I/O, read-only windows,
flush/readback and repeated reuse. Run it with virtio-blk, virtio-scsi and
NVMe (512 and 4096 byte logical blocks). `tests/blk-windows.rs` exercises the production
validator on malformed requests and window/pool boundaries.

### 8.3 Network: L2 virtual switches only

**Principle.** The hypervisor implements Ethernet switching. Routing,
NAT, DHCP and firewalling belong in a VM.

- **Path.** Guest `virtio_net` → `l4h-vmm` → shared net port rings →
  `l4h-netd` → destination port or uplink NIC. netd combines all switches
  and the virtio-net PCI drivers in one Rust component. It supports modern
  and legacy virtio-net; gVNIC is future work (§10).
- **Multiple vNICs.** Each vNIC is on one switch and appears as a separate
  `ethN`. It shares the guest's limit of 16 devices with the console,
  disks and, in dom0, control device. There are 48 guest ports in all.
- **Switches and uplinks.** The API creates switches dynamically. Each is
  an L2 broadcast domain with its own learned MAC table. A physical NIC
  can be the uplink of one switch; a switch without an uplink is internal.
  A single netd serves all switches. VLANs, jumbo MTUs, static MAC entries
  and port rate limits are future work.
- **Ports and copies.** A vNIC has TX and RX rings of 2 KiB frame slots
  shared by its VMM and netd. Root adds/removes ports through
  `NETD_ADD_PORT`/`NETD_REMOVE_PORT`; port memory belongs to the guest's
  arena. The guest first bounces buffers through its host-owned I/O pool.
  The VMM copies between that pool and port slots; netd copies slot to
  slot, or slot to NIC DMA buffer. Making the I/O pool itself a netd port
  region could remove the VMM copy in future.
- **Offloads.** Checksum and TSO metadata pass through in the virtio-net
  header. netd validates flags, checksum offsets and GSO type, including
  that a GSO frame is TCP over IPv4/IPv6. Incoherent headers are dropped.
  A frame for a port that cannot take its offload is segmented and
  checksummed in software. Guest vNICs have one RX/TX queue pair.
- **Learning and security.** MAC entries age after 300 s, with at most
  1024 per switch and 8192 in all. A full table drops expired entries and
  then stops learning; unknown unicast, broadcast and multicast are
  flooded within the switch. netd handles at most one ring's worth of
  frames per kick and ignores rings whose indices claim more. MAC lock
  is on by default. Isolated ports exchange no frames with each other,
  but can reach normal ports and the uplink (`--nic ...,isolated`).
- **Router VMs.** On GCE, the router's uplink vNIC uses the instance's MAC
  address. dom0's first vNIC takes the uplink NIC's MAC by default. The
  router's other vNICs join internal switches and Linux provides NAT or
  routing. QEMU guests can also sit directly on the uplink switch.
- **Tests.** `tests/run-nettest.sh` covers DHCP and ping over the QEMU
  uplink, internal switches, multiple vNICs, isolation and MAC lock.
  `tests/run-router.sh` uses a Debian router with nftables masquerade to
  connect an internal guest to the outside.

### 8.4 Console and other devices

- **Console.** `hvc_l4h` sends output to the VMM using hypercalls. The VMM
  appends it to a host-owned per-guest ring and writes to the serial port;
  root reads the ring for `console_read`. Console input and power requests
  use the shared console device page.
- **Control.** `l4hctl`, dom0 only (§9.1).
- **Future devices.** virtio-rng, virtio-vsock and virtio-balloon.

---

## 9. Host components

All host components are Rust: no_std, using the rust-sel4 crates `sel4` and
`sel4-root-task`.

Device drivers are also written in Rust, following sDDF's design
(shared queues and block requests that add no host data copy,
virtualiser/switch components) rather than linking sDDF's C drivers.
sDDF's components assume a static, Microkit-shaped system, while l4h
attaches and detaches guests at runtime. A single language and runtime for
the host keeps that dynamic plumbing simple.

| Component | Instances | Role |
|-----------|-----------|------|
| `l4h-root` | 1 | Allocates guest RAM blocks, I/O pools, host objects and vCPU scheduling contexts. Builds guest CSpaces, kernel VSpaces and boot info, loads dom0's embedded images and copies runtime image uploads. Runs lifecycle, control, balancing, monitor and dashboard; authorizes disk and NIC attachments. |
| `l4h-vmm` | 1 per guest | Serves virtio backends, console and hypercalls using the guest's I/O pool and host-owned shared pages. Never maps guest RAM. Forwards control requests from dom0 to root. |
| `l4h-blkd` | 1 per attached disk | Validates pool addresses and LBA windows and drives a virtio-blk, virtio-scsi or NVMe PCI disk. |
| `l4h-netd` | 1 | Runs all L2 switches, dynamic guest ports and virtio-net PCI uplink drivers. |

The shared crates are `l4h-abi` (guest ABI), `l4h-host` (host component
protocols), `l4h-rt` (component runtime) and `l4h-virtio` (PCI transport).
Serial output uses seL4's printing syscall; root drives COM1 input for the
monitor. There is no separate serial virtualiser or gVNIC driver.

**Guest boot protocol.** The guest boots without firmware or a bootloader.
Root parses dom0's embedded ELF. For runtime guests, the dom0 `l4hctl`
client parses the ELF and root copies uploaded segments and initramfs
bytes into the new guest's boot frames. Root constructs the kernel VSpace
and `struct l4h_bootinfo`, then starts vCPU0 at the entry point with `rdi`
(`x0` on aarch64) pointing at boot info. It holds:

- the guest ABI version (currently 3);
- RAM size, direct-map base, block count and boot-block count;
- the I/O pool GPA and size;
- the vCPU count and pre-assigned VSpace count;
- the device table;
- the command line;
- the time-counter frequency and flags.

Capability slots and special mappings, including event-page addresses,
are defined by `include/l4h/abi.h`, the single source of truth. The guest
includes it directly. `host/l4h-abi/src/lib.rs` is a handwritten Rust
mirror with layout assertions; bindings are not generated with bindgen.
The control protocol has its own version, currently 1.

### 9.1 Control plane: dom0 and the runtime API

**Starting dom0.** `tools/build-host.sh` builds the services and root.
`host/l4h-root/build.rs` embeds their ELFs, dom0's kernel and initramfs,
and its command line. `L4H_DOM0_*` environment variables set its memory,
vCPUs, I/O pool, disks and NICs; rebuilding root applies changes. There
is no runtime TOML manifest. The user manual lists the variables.

dom0 is guest #0 and receives a host-owned `l4hctl` device page. Its VMM
has a control forwarding capability to root. Other guests' VMMs do not.
dom0 runs no host hardware drivers; those stay in blkd and netd.

**API surface.** Requests and responses are synchronous, fixed-layout,
little-endian structs defined in `include/l4h/abi.h` and mirrored in
`l4h-abi`. The control version is 1.

| Area | Implemented operations |
|------|------------------------|
| Host | `host_info`: cores, free/total bytes, disk, VM and NIC counts |
| Guests | `vm_create`, `vm_load`, `vm_start`, `vm_shutdown`, `vm_destroy`, `vm_list`, `vm_info` (cores for the first six vCPUs), `vm_stats` (CPU use and cap), `vm_set_sched` |
| Block | `blk_attach`: disk, read-only flag and LBA window; `disk_info`: PCI address, owner, failed flag and kind |
| Network | `switch_create`, `switch_attach_uplink`, `nic_create` (MAC, MAC lock, isolation), `switch_info` (ports and counters), `nic_info` (PCI address, switch, MAC) |
| Console | `console_read` (offset, bytes and lost-byte count), `console_write` |

Devices are attached before start and removed on destroy. Pause/resume,
hotplug, switch deletion, I/O limits and asynchronous event queues remain
future work. A Rust userspace library and CLI live in `tools/l4hctl`;
external orchestration can run in dom0.

**Transport and lifecycle.**

- The `l4hctl` device is a single shared page rather than virtqueues:
  dom0 writes a fixed-layout request (`struct l4h_ctl_req` in `abi.h`) and
  issues `L4H_HC_CTL`. dom0's `l4h-vmm` forwards the call to `l4h-root`,
  which executes it synchronously and writes the response into the same
  page. Image loads are bounded at 1 MiB per call.
- `/dev/l4hctl` takes one ioctl that carries a request, its response and,
  for `VM_LOAD`, a user buffer. The driver stages it in the control
  device's data area, 1 MiB of host-owned frames mapped into dom0, and
  sets the request's source offset to 0 and length to the ioctl buffer
  length: `l4h-root` never reads dom0's RAM.
- `l4hctl` parses the guest kernel ELF in dom0 userspace and loads it with
  raw `VM_LOAD(vm, gpa, bytes)` writes. `l4h-root` copies page by page from
  the data area to the new guest's frames and never parses guest images.
  The lifecycle is `vm_create` → (`blk_attach` / `nic_create` / `vm_load`)* → `vm_start`
  (writes the boot info and spawns the guest's `l4h-vmm`) → `vm_destroy`.
- `vm_shutdown` (`l4hctl shutdown [--reboot]`) is the ACPI-power-button
  equivalent. `l4h-root` sets `power_request` in the guest's console page
  and raises the console's event line on vCPU 0. The guest runs
  `orderly_poweroff()` or `orderly_reboot()`, which signals init.
  `vm_pause` is not implemented: suspending the vCPU kernel thread does
  not stop a user thread that holds SC_U.
- Console output goes into a per-guest ring that `l4h-vmm` writes and
  `l4h-root` reads for `console_read`. It is also copied to the serial port
  with a `[vmN]` line prefix. Console input is a ring in the guest's
  console device page, which hvc polls.
- **Reclaim.** Each guest holds RAM block Untypeds, its I/O pool, a
  host-object arena and its blkds' arenas. Root CNode slots allocated
  on the guest's behalf are tracked. `vm_destroy` first quiesces the
  attached disks, then revokes those Untypeds, returns them to the free
  pool and deletes the tracked slots. ASIDs are released by the kernel
  when VSpaces die. Repeated create/destroy leaves free memory unchanged
  (`tests/run-ctltest.sh`).
- **Disk detach.** A disk window is detached only when its guest is destroyed; the shared controller remains running.
  The device may hold commands that address the guest's I/O pool and
  blkd's DMA memory (queues, PRP lists, request headers), so `l4h-root`:
  1. suspends blkd, so that nothing more is submitted;
  2. resets the device itself, through a mapping of its registers: NVMe
     clears CC.EN and waits up to CAP.TO for CSTS.RDY to clear, which
     aborts every command and deletes the queues; virtio writes 0 to
     `device_status` (an I/O port on a legacy device) and waits until it
     reads back 0. Bus mastering stays
     on meanwhile, so DMA the device finishes while resetting lands in
     memory that still belongs to the guest;
  3. clears bus mastering, memory decoding and MSI-X, and deletes the IRQ
     handler;
  4. only then reclaims blkd's arena and the guest's.

  Clearing bus mastering alone is not a drain: a device can complete a
  command it already started, and it keeps its queue addresses, which it
  would use again when the next attach turns bus mastering back on. If a
  device does not confirm the reset, it is marked failed and never enabled
  again (`blk_attach` returns EIO, `l4hctl disks` says so). The guest's I/O pool
  and blkd's DMA memory are then kept out of the free pool; guest RAM
  and other host objects are reclaimed (§15.1). Disks are also reset at boot,
  with bus mastering off, before anything enables them, since firmware
  may leave a controller running. There is no IOMMU (§8.2), so nothing
  else confines a device.
  `tests/run-ctltest.sh` destroys guests in the middle of throttled reads,
  on virtio-blk and NVMe, and checks that the next guest, which reuses the
  memory, sees no disk data appear in it. Before the reset was added, the
  NVMe case wrote 1 MiB of disk data into the next guest, whose init then
  crashed.

**Policy and safety.**

- `l4h-root` enforces resource accounting (memory, cores, disk ownership: a
  backing is attached to at most one VM; v1 does not share read-only
  backings either) and rejects
  invalid requests. dom0 cannot breach host isolation, but it is fully trusted
  to decide *what* runs. In particular, dom0 can attach any disk to itself.
- If dom0 crashes, the other guests keep running. `l4h-root` restarts dom0
  from its embedded images and settings, and the new dom0 recovers state
  with `vm_list` and `switch_info`. The authoritative state lives in
  `l4h-root`.
- Debugging fallback: a minimal serial monitor in `l4h-root` (`list`,
  `destroy`, `restart-dom0`) stays available on COM1, which is the GCE serial
  console.

---

## 10. Boot and deployment

- **Firmware.** UEFI → GRUB (EFI) → multiboot2. QEMU uses OVMF for this
  path, or plain `-kernel` multiboot for development. The boot files are
  the seL4 kernel and `l4h-root`, the latter loaded as a module and
  embedding the service ELFs, dom0 images and settings. Images for other
  guests come through the dom0 API.

- **Platform discovery** is done by seL4 using ACPI: MADT, cores and IOAPIC.
  `l4h-root` handles PCI enumeration over MCFG/ECAM, or port I/O config, and
  MSI/MSI-X allocation through `seL4_IRQControl_GetMSI`.

### 10.1 Targets, in order

1. **QEMU (development and CI).** `-cpu host,-vmx` with KVM used only as a
   fast CPU. Devices: virtio-net-pci, virtio-blk-pci, nvme and a 16550 UART.
   l4h drives these with its Rust blkd/netd drivers and serial monitor.
2. **GCP.**
   - **Current target:** N2 with virtio-net and NVMe local SSDs. N2 boot
     disks use virtio-scsi, which blkd drives as a legacy device (tested
     under QEMU with `disable-modern=on`, not yet on GCE).
   - **Future target:** third-generation and later types (C3, C3D, N4,
     C4), which require gVNIC. A gVNIC driver, potentially ported from
     Linux `gve`, is not implemented. NVMe already has a Rust driver.
   - **Boot:** a custom image with GRUB EFI and seL4. Secure Boot is off
     (Shielded VM vTPM and integrity monitoring are optional). The serial
     console is enabled for seL4 and monitor output.
   - **Timer:** E3 shares seL4's LAPIC timer (TSC-deadline where available,
     APIC countdown on N2), so there is no HPET dependency. Patch 0014
     stabilizes TSC/APIC calibration on N2 (§4).
   - **Networking:** virtio-net is the uplink of a switch, and a router VM
     (often dom0) owns the instance MAC (§8.3). A future gVNIC driver would
     use the same topology.
3. Later targets: AWS (ENA and NVMe) and Azure (MANA and NVMe), each mainly
   adding one NIC driver.

**Status (v1).**

- **Image.** `tools/mkimage.sh` builds a GPT disk image. Its EFI system
  partition holds:
  - a standalone GRUB EFI binary;
  - the seL4 kernel, loaded with `multiboot2` and
    `console_port=0x3f8 debug_port=0x3f8`, because UEFI firmware need not
    fill in the BIOS data area that seL4 otherwise reads the port from;
  - `l4h-root`, which carries dom0's images, as `module2`.
- **Local test.** `tests/run-uefitest.sh` boots that image the way GCE
  would, under OVMF on a q35 machine: NVMe boot disk, virtio-net NIC, KVM
  without VMX. dom0 sees the boot disk as host disk 0 and gets DHCP
  through the virtio-net uplink.
- **Deployment.** `tools/gce-image.sh` packages the image as
  `disk.raw.tar.gz` and prints the `gcloud` import and instance commands
  (N2, `nic-type=VIRTIO_NET`, `UEFI_COMPATIBLE`).
- **Tested on GCE** (`tests/gce-test.sh`, n2-standard-4): UEFI boot,
  DHCP and internet access through the NIC, a 4-vCPU guest self-test, a
  local SSD (NVMe, 4 KiB blocks) passed through, 12 small guests.
  - GCE's virtio-net is a legacy (0.9.5) device: registers in I/O BAR 0,
    no virtio capabilities. netd drives both kinds; root gives it the I/O
    port capability.
  - dom0's first vNIC takes the uplink NIC's MAC address, since the VPC
    only delivers frames for it.
  - N2 boot disks are virtio-scsi. Before blkd supported it, only the
    firmware and GRUB read them.
  - gVNIC, which C3 and later need, is not implemented.

---

## 11. Portability to aarch64

| Concern | x86_64 | aarch64 |
|---------|--------|---------|
| seL4 support | MCS, SMP | MCS, SMP, and the best proof coverage |
| syscall trap (E1) | `syscall`, number in `rdx` | `svc`, number in `x7`; the same flag applies, and the fault carries Linux's x8 (patch 0007) |
| FPU access (E2) | XSAVE area | V-regs + FPSR/FPCR |
| Guest VA | 47-bit | 48-bit TTBR0 |
| TLS | `fs_base` | `TPIDR_EL0` (`seL4_TCB_SetTLSBase`) |
| Timer (E3) | LAPIC TSC-deadline or countdown | ARM generic timer |
| vDSO clock | `rdtsc` | `cntvct_el0` |
| Drivers | Rust virtio-net/blk/scsi PCI, NVMe | Same drivers; ECAM, BAR assignment and GICv2m MSI-X |
| Boot | multiboot2 / UEFI | rust-sel4 loader, entry at EL1 or EL2 |

Rules that keep the port cheap:

1. ISA code lives only in `arch/l4h/<isa>/` and in `host/*/src/arch/<isa>.rs`.
2. Nothing in the protocols depends on the ISA: boot info, event pages,
   virtio-l4h, host block and net queues. Fields are fixed-width and little-endian.
3. No use of x86-only seL4 features such as I/O ports outside drivers, EPT or
   `GetStatusBits`.
4. Guest RAM uses 4 KiB frames, which every ISA supports.
5. CI builds both ISAs from day one, even before arm64 boots.

**Status (v1).**

- **Kernel.** seL4 aarch64 (QEMU `virt`, MCS, SMP, EL1, no hypervisor
  support) builds with all l4h patches, from `kernel/configs/l4h-aarch64.cmake`
  via `tools/build-kernel-aarch64.sh`.
  - E1 (trapSyscalls), E2 (FPU access), E3 (timers), patch 0005 (IPI
    barrier) and patch 0006 (lazy timer) are ISA-neutral.
  - Patch 0007 makes aarch64 report **x8**, the Linux syscall number,
    in the UnknownSyscall fault of trapping threads. seL4's own number is
    in x7, and x8 is not otherwise part of the fault message.
  - FaultIP points at the `svc` instruction, so the guest advances it by
    4 (x86_64: by 2).
  - Patch 0015 uses inner-shareable broadcast TLB maintenance at EL1.
    Patch 0016 reads the architectural counter frequency at boot for MCS
    budget and consumed-time conversions; the same image handles TCG's
    62.5 MHz counter and Apple M4 HVF's 24 MHz counter.
- **Boot.** rust-sel4's kernel loader only entered EL2 kernels.
  `third_party/patches/rust-sel4/0001` adds the EL1 path: it drops from EL2
  with the EL1 translation regime set up and no traps.
  `tools/build-loader-aarch64.sh` applies the patch and builds the loader.
  Patch 0002 also supports direct EL1 entry, initializes the EL1 translation
  regime and selects QEMU's HVC PSCI conduit when EL2 is unavailable.
  The run script disables virtualization by default; its optional
  `L4H_QEMU_VIRTUALIZATION=on` tests the EL2 path.
- **Host.** All components build for AArch64. QEMU `virt` uses GICv2,
  `highmem-ecam=off,highmem-mmio=off`, a 16 MiB ECAM at 0x3f000000 and
  the 32-bit PCI memory aperture. The root assigns memory BARs before starting devices. GICv2m
  supplies 64 MSI interrupts (SPIs 80–143), divided between NICs and up to
  48 disks. MSI address/data are passed explicitly to the common drivers.
  The PL031 supplies wall time and the PL011 receives serial monitor input.
- **Linux.** The native AArch64 ABI, signals and FPSIMD context records,
  ptrace register sets, TLS, SMP, atomics, setjmp/longjmp, syscall restart,
  instruction-cache maintenance and module relocations are implemented.
  Modules use the large code model, like the guest kernel.
- **Tests.** `run-testguest-aarch64.sh` covers the ABI with an attached
  PCI disk. `L4H_ARCH=aarch64` selects the Linux integration tests, including
  1/4-vCPU self-tests, modules, virtio-blk/NVMe, VM lifecycle, networking and
  the serial monitor. CI has a QEMU/TCG job for these paths. The assembled
  image also runs directly on Apple Silicon macOS using QEMU/HVF with
  `kernel-irqchip=off`; 1/4-vCPU self-tests pass on an Apple M4.
  `tools/run-image-aarch64.sh` selects HVF on macOS.
- **Platform scope.** This configuration targets QEMU `virt` with GICv2m.
  The existing UEFI/GCE images target x86_64; other ARM boards require their
  own platform description and interrupt routing.

---

## 12. Performance expectations, to be measured

| Operation | Native KVM guest | l4h, estimate | Notes |
|-----------|------------------|---------------|-------|
| null syscall | ~50 ns | ~0.4–1 µs | Fault IPC slowpath plus ReplyRecv. An optional seL4 patch can fastpath UnknownSyscall replies. Comparable to Xen PV x86_64 or gVisor systrap. |
| page fault (anon) | ~1 µs | ~2–4 µs | Fault IPC, `handle_mm_fault` and `Page_Map`. |
| context switch | ~1–2 µs | ~2–3 µs | Two `SetSchedParams` and a reply. No FPU copying. |
| `clock_gettime` | vDSO | vDSO (rdtsc) | No trap. |
| fork+exec | baseline | 1.5–3× | Lazy refault. |
| disk I/O | | near native | One copy by the guest kernel through its I/O pool, plus two host queue hops. |
| network | | copies through pool and port slots | The guest bounces through its I/O pool; VMM and netd copy between pool, port and NIC buffers (§8.3). |

**Distribution check.** `tests/run-debian.sh` boots Debian 13 with
systemd as dom0 from a passed-through virtio-blk disk, on 2 vCPUs:
- systemd reaches "running" with no failed units;
- DHCP and DNS work over the user-mode uplink;
- `apt-get update` and `apt-get install` succeed;
- `ping` works.
The image comes from `tests/mkdebian.sh`.
`tests/run-workload.sh` covers the M2 and M3 gates on that image, with 4
vCPUs:
- fio random writes with crc32c verification, direct I/O, libaio,
  queue depth 16: 13.4k IOPS, all verified;
- busybox built with `make -j4` in 124 s, and the result runs;
- a clean `e2fsck -f` of the disk on the host;
- a checksum intact after a second boot.

**Measured before the I/O pool and RAM blocks (`tests/run-bench.sh`).**
These figures describe the earlier memory design; §15.2 records the
measurements after those changes. The test machine is unusual: the
dev box is itself a Hyper-V VM, so QEMU/KVM runs *nested*. Every VM exit
of seL4 (timer MSR writes, IPIs, EOIs, HLT wakeups) costs tens of
microseconds, against roughly one on a single-level cloud hypervisor.
Figures dominated by exits are therefore pessimistic here.

| Operation | l4h, nested KVM | Notes |
|-----------|-----------------|-------|
| getpid | 1.3–1.5 µs | Was 20 µs before kernel patch 0006. MCS reprogrammed the TSC deadline, a VM exit, on every scheduling-context switch. It now keeps an earlier armed deadline. |
| page fault (first touch) | 5–6.5 µs | |
| pipe round trip | 10 µs (same vCPU) | 37 µs between 2 vCPUs, which needs cross-core IPIs. |
| fork+exit+wait | 340–450 µs | |
| disk, 1 MiB direct | 830 MB/s write, 1.2–1.9 GB/s read | Zero-copy, close to the QEMU backend's own speed. |
| disk, 4 KiB read latency | ~200 µs | Two cross-core wakeups: guest → VMM → blkd and back. |
| guest↔guest ping | 1.4 ms | Four component hops per direction, with cross-core wakeups. |
| guest↔guest TCP | ~30 MB/s | Limited per frame (~140 µs, i.e. wakeups), not by copying. Checksum offload and TSO pass through the switch. |

Next steps for I/O latency:
- place each guest's VMM, blkd and netd work on the guest's core, or use
  passive servers on the caller's scheduling context;
- poll briefly before blocking, which trades CPU for wakeups;
- measure on GCE, where exits are about 20× cheaper.

The design prioritizes I/O and idle density, the cloud workload, over
syscall-heavy microbenchmarks. Benchmarks: lmbench lat_syscall/lat_ctx/
lat_pagefault, fio, iperf3, kernel compile, and a 30-guest idle-density test.

---

## 13. Repository layout and plan

```
l4h/
  DESIGN.md
  kernel/            seL4 (git submodule, pinned) + patches/ (E1–E3) + configs
  guest/linux/       arch/l4h sources, patches/ for anything outside arch/, defconfig (Linux 6.18 LTS)
  host/              Rust workspace: l4h-abi, l4h-host, l4h-rt, l4h-root, l4h-vmm, l4h-blkd, l4h-netd, l4h-virtio
  tools/l4hctl/      dom0 userspace CLI and library for the control API
  third_party/sddf/  submodule (design reference; no linked C drivers)
  tools/             build-kernel.sh, build-linux.sh, build-host.sh, mkimage.sh (UEFI disk), run-qemu.sh, mkinitramfs.sh
  tests/             host-level integration tests driven by QEMU + expect-style console scripting
```

**Test environment.** This machine has KVM, so QEMU uses it only as a fast
CPU to mimic a cloud VM. The guest CPU is `-cpu host,-vmx -smp 4`, with
`virtio-blk-pci`, `nvme` and `virtio-net-pci` devices, and UEFI via OVMF or
plain multiboot. seL4 never uses VT-x. The dev box has 32 GiB of RAM, which
is reported as less because of host ballooning. The 30-guest test therefore
uses a 16 GiB QEMU machine and guests of 256–512 MiB.

**Original milestones and gates.** These record the development plan,
not a claim that every proposed gate or feature has been completed.
M0–M6 have working v1 implementations, with the tests and limitations
described above. M7 has reached N2; gVNIC remains. M8 includes AArch64 Linux/PCI support and virtqueue fuzzing; upcall
injection and paravirtual spinlocks remain future work.

1. **M0, platform.**
   - Toolchains and the seL4 MCS SMP build.
   - A Rust `l4h-root` hello world boots in QEMU on 4 cores.
   - E1, E2 and E3 implemented, each with sel4test-style unit tests.
2. **M1, single-vCPU guest (dom0 only).**
   - dom0 with embedded images and build-time settings boots `arch/l4h`
     to an initramfs shell over the hvc console.
   - Covers syscalls, page faults, signals, fork/exec, mmap/mprotect and
     futex, plus the E3 timer and the vDSO.
   - Gate: busybox, then a static test suite (LTP subset).
3. **M2, SMP guests.**
   - vCPU bring-up and IPIs; PV spinlocks remain future work.
   - Gate: `stress-ng` and LTP on 4 vCPUs; `make -j4` of a kernel inside the
     guest.
4. **M3, virtio-l4h and blk passthrough.**
   - One Rust blkd per attached disk, with virtio-blk and NVMe drivers.
   - Gate: fio and fsck, with data integrity across guest reboots.
5. **M4, control API.**
   - The `l4hctl` device and `/dev/l4hctl`, the dom0 CLI, and the
     `vm_*` and `blk_*` operations.
   - Gate: dom0 creates, starts and destroys guests at runtime, and console
     attach works.
6. **M5, networking.**
   - vswitch with multiple switches, multiple vNICs per VM, and uplink ports.
   - The Rust netd virtio-net driver and the net backend in the VMM.
   - `switch_*` and `nic_*` operations.
   - Gate: guest↔guest iperf3 and ping, a router VM doing NAT to the outside,
     and MAC-lock tests.
7. **M6, density.**
   - Budgets, the balancer, steal time and dom0 restart.
   - Gate: 30 guests on 4 cores stay responsive, and destroy/recreate loops
     leak no Untyped.
8. **M7, GCP.**
   - Bring-up on N2 with virtio-net and NVMe (existing drivers).
   - Then the gVNIC driver (DQO) and C3/N4.
   - A GCE image builder.
   - Gate: the M3–M6 tests pass on GCE.
9. **M8, hardening and portability.**
   - The upcall-injection backstop (future), the host-owned I/O pool
     (implemented, §15.1), and fuzzing of the VMM, switch and control-API
     parsers (virtqueue and pool fuzzing implemented).
   - AArch64 Linux and PCI on QEMU virt (implemented, §11).

---

## 14. Review decisions

Decided on 2026-10-17:

1. **seL4 patches:** carry E1–E3 and propose them upstream.
2. **Targets:** QEMU with virtio devices first, then GCP. AWS and Azure come
   later.
3. **Networking:** the hypervisor provides only L2 virtual switches, with
   multiple vNICs per VM. Routing and NAT are done by a VM.
4. **Control plane:** a runtime API, available only to dom0, the first guest
   started at boot (§9.1).
5. **Guest kernel:** Linux 6.18 LTS with out-of-tree `arch/l4h` patches.

Still open, but not blocking M0–M3:

- Memory: is fixed reservation, with no overcommit, acceptable for v1?
  This is the current assumption.
- The API wire format beyond v1 fixed structs, if external orchestration
  needs a richer schema. That would be added in dom0 userspace.

---

## 15. Guest-owned memory

Implemented from the design "guest-owned memory (2 MiB Untyped RAM,
host-owned I/O pool)", revision 2, in its three phases. Goal: a guest's
paging structures and other kernel objects come from its own RAM, so that
its host overhead is a small fixed amount instead of growing with its RAM.
The precondition is that no device and no host component ever touches
memory the guest can retype.

### 15.1 Host-owned I/O pool and restricted DMA (phase 1)

- **The pool.** At create time `l4h-root` allocates the guest's I/O pool as
  one physically contiguous Untyped of `io_pool` bytes (default 4 MiB; a
  power of two from 1 to 256 MiB; `l4hctl create --io-pool`, dom0 with
  `L4H_DOM0_IO_POOL_MB`). It retypes it into frames and maps them into the
  guest kernel's VSpace at `ram_base_va + io_pool_gpa`, where
  `io_pool_gpa` is RAM rounded up to 2 MiB (boot info `io_pool_gpa`,
  `io_pool_size`), and into the guest's `l4h-vmm`. The guest gets these
  mappings only, no capability to the pool at all (stronger than the
  design, which gave it frame-cap copies: it never needs them).
- **Guest kernel.** `arch/l4h` no longer selects `NO_DMA`: it has
  dma-direct (device addresses are GPAs) with `SWIOTLB` and
  `DMA_RESTRICTED_POOL`. The pool is added to memblock and reserved, so it
  has struct pages but never enters the page allocator.
  `guest/linux/patches/0002` adds `swiotlb_dev_init_pool(dev, base, size)`
  and lets `DMA_RESTRICTED_POOL` be built without OF. The pool gets one
  swiotlb area per possible CPU (as far as 128-slot segments allow), and a
  full pool does not warn: it is normal back-pressure.
  - The transport registers one platform device, `virtio-l4h`, binds it to
    the pool and makes it the parent (DMA device) of every virtio device.
    Vrings are coherent allocations from the pool; buffers bounce.
  - It requires `VIRTIO_F_ACCESS_PLATFORM`, and fails a queue the host
    refuses.
- **VMM.** `GuestMem` is a window on the pool only (`IO_POOL_VA`); it
  checks every guest address against `[io_pool_gpa, +io_pool_size)` with
  overflow-safe arithmetic. A driver that does not accept
  `ACCESS_PLATFORM` fails `FEATURES_OK`; a queue outside the pool is
  refused, and `DRIVER_OK` then fails (status `DEVICE_NEEDS_RESET`) until
  the device is reset. Chains with any buffer or indirect table outside
  the pool are returned unused.
- **Request size.** A block request bounces entirely through the pool, and
  one that cannot fit even in an empty pool would be requeued forever. The
  VMM therefore offers `VIRTIO_BLK_F_SIZE_MAX` with `size_max` such that
  `seg_max * size_max` is at most a quarter of the pool (72 KiB segments,
  about 1 MiB per request, with 4 MiB; 16 KiB with 1 MiB).
- **blk-mux.** blkd's client data region is the pool: every segment must
  lie in it and is translated with one offset (`host::blk::Pool`). The
  RAM chunk tables of blkd and the VMM are gone.
- **Hypercalls and other host access.** The device register pages and the
  console ring were already host pages. The only hypercall that passed a
  guest buffer by GPA was the control API's `VM_LOAD`: its source is now
  an offset in the control device's data area, 1 MiB of host frames mapped
  into dom0 (`l4h_dev_desc.data_va`), so `l4h-root` never reads guest RAM.
  `L4H_CTL_LOAD_MAX` is 1 MiB. ABI version 2.
- **Disk detach (F1).** Destroy still suspends blkd and resets the device
  (`blkd::detach`, `reset`) before any memory is reused. Only the pool and
  blkd's DMA memory can be device targets now, so a device that does not
  confirm the reset pins those (`Guest::destroy(pin_pool)`), not the
  guest's RAM.
- **Tests.** `tests/testguest` plays a hostile driver against a virtio-blk
  disk: no `ACCESS_PLATFORM`, rings in RAM or across the pool's end,
  header, data or status in RAM, a buffer past the pool's end; a read
  through the pool still works. `tests/fuzz-virtq` places the pool at
  random GPAs (including 0 and the top of the address space), aims
  descriptors and rings inside, at the edges of and outside it, checks
  queue setup both ways, checks blkd's pool check, and checks that no
  chain outside the pool reaches blkd. `run-blktest.sh` adds 8 parallel
  direct writers and readers (more than the pool holds); it also passes
  with `L4H_DOM0_IO_POOL_MB=1`.

### 15.2 Guest RAM as 2 MiB Untypeds (phase 2)

- **seL4 semantics, settled first** (`tests/testguest`, `untyped_copy_test`
  and `ram_blocks_test`, seL4 16.0.0). An Untyped can be copied only while
  it has no children; the copy is a revocable MDB child of the original, and
  copying sets the original's free index to "full", so the two can never
  hand out the same memory. The guest retypes and revokes through its copy;
  a retype from a copy without children starts at its base and the memory
  is zeroed (reset); a copy with children cannot be copied again; revoking
  the original deletes the copy and everything retyped from it. So the
  design's first scheme works as intended: l4h-root keeps the original of
  every block and gives the guest a copy. The fallback (only cap to a
  2 MiB Untyped under a parent) was not needed.
- **Blocks.** Guest RAM is `ram / 2 MiB` blocks (RAM is rounded up to
  2 MiB), each a 2 MiB Untyped split from any free memory: guest RAM is no
  longer physically contiguous at all, and the chunk table and
  `VMM_MAX_RAM_CHUNKS` are gone. The originals stay in the guest's arena
  (`Arena::owned`), so destroy revokes them as before. Each block costs the
  host one root CNode slot.
- **Boot blocks.** `l4h-root` turns a block into host frames (a level-2 RAM
  CNode, 512 frames, mapped in the direct map) when an image is loaded into
  it (`VM_LOAD`, or dom0's embedded image). At `VM_START` the boot blocks
  are the prefix of RAM up to the end of the kernel, the initrd and
  anything loaded, plus `BOOT_SLACK` (one) block for the guest kernel's
  memory until it has mapped the rest; at least one block must be left for
  the guest. Deviation: the design counted two slack blocks as boot blocks
  *and* as the guest's first object pool, which cannot both hold (the
  guest has no Untyped for a boot block); here the slack block is frames
  for early `memblock` allocations, and object blocks come from the
  Untyped blocks.
- **Capabilities (ABI version 3).** Root slot `L4H_SLOT_BLOCKS` is the block
  CNode: slot `b` holds the Untyped copy of every non-boot block. Frames
  are named through two levels: root slot `L4H_SLOT_RAM_L1_BASE + n` is a
  level-1 CNode (radix 9) for GiB `n`, whose slot `j` holds the level-2
  CNode (radix 9) of block `512 n + j`; `L4H_FRAME_CPTR(gpa)` keeps its old
  value. l4h-root installs the level-2 CNodes of boot blocks (it keeps its
  own capability to them, to load images); the guest retypes its own
  into the others. Boot info: `nr_blocks`, `boot_blocks`; the Untyped
  quota (`L4H_SLOT_UNTYPED_BASE`, `bootinfo.untyped[]`) is gone, and
  `VM_CREATE`'s quota argument is ignored (`l4hctl --quota` removed).
- **Guest early boot** (`kernel/caps.c`, `l4h_caps_init`): memblock knows
  only the boot blocks. The guest marks some non-boot blocks as object
  blocks, creates its first capability CNode and the user CSpace from
  them, then for every other block retypes its level-2 RAM CNode (into its
  level-1 slot) and 512 frames, and maps them at `ram_base_va + GPA`
  (read-write, execute-never), adding page tables from object blocks. Each
  block is added to memblock once mapped; object blocks are added and
  reserved, so they have struct pages.
- **Objects.** `l4h_obj_alloc` takes objects from Untyped *chunks* of 16,
  64 or 128 KiB (by object size, as the old quota allocator did) that it
  carves out of object blocks, one open block per class. seL4 reuses an
  Untyped's memory only when all its children are gone; with whole 2 MiB
  blocks as the unit, a few long-lived objects (TCBs, a process's page
  tables) pinned each block, freed page tables were never reused, and
  `smp_mm_stress` exhausted 32 MiB of objects and slowed down sevenfold.
  A chunk resets as soon as its own objects are gone; a block is free
  when all its chunks are, and then forgets them. Slot CNodes (128 KiB)
  come straight from a block, since a chunk needs a slot. In this phase
  the object blocks are fixed at boot, 4 MiB plus RAM/32 out of the
  guest's own RAM (the old default quota, 16 MiB plus RAM/16, left a
  128 MiB guest too little RAM for ctltest's 80 MiB tmpfs); §15.3 makes
  them dynamic. `/proc/meminfo` shows `L4hObjects` (RAM in
  object blocks) and `L4hObjectsUsed` (in chunks with live objects).
  Deviation: the design's per-vCPU open blocks are per class only. All
  allocations take one lock anyway, and per-vCPU blocks would multiply the
  partly used blocks by the number of vCPUs.
- **Host accounting.** The ×2 safety factor is gone: an arena starts with
  an estimate of what its owner needs and grows from the free blocks
  (1 MiB at a time) when it runs out (`Arena::retype`). The VMM's estimate
  went from 8 MiB to what it maps (image, 1 MiB heap, 512 KiB stack).
- **Buddy merging.** With RAM coming back in 2 MiB blocks, the host's free
  memory would end up in 2 MiB pieces, and an I/O pool (4 MiB) could no
  longer be found. `Alloc` now keeps the Untyped of every block it
  splits, and merges two free halves again by deleting them. It merges
  lazily, only when no free block is large enough (`merge_up_to`): the
  next retype of a merged parent makes seL4 clear everything its children
  used, and merging eagerly up to GiB blocks made every guest create clear
  a GiB of memory (create took 470 ms instead of 150).
- AArch64 uses the same host and Linux guest RAM-block conversion and
  reclamation paths. Only its paging-object operations differ.
- **Measured** (`tests/run-vmbench.sh`, nested QEMU/KVM, debug builds;
  host memory beyond a booted guest's RAM, and median latencies):

  | | main | phase 1 | phase 2 |
  |---|---|---|---|
  | overhead, 64 MiB guest | 41 MiB | 43 MiB | 10 MiB |
  | overhead, 128 MiB | 48 MiB | 49 MiB | 10 MiB |
  | overhead, 256 MiB | 62 MiB | 60 MiB | 10 MiB |
  | overhead, 512 MiB | 90 MiB | 82 MiB | 10 MiB |
  | create (call returns), 128 MiB | 389 ms | 316 ms | 128 ms |
  | create to init ready, 128 MiB | 446 ms | 344 ms | 258 ms |
  | create, 512 MiB | 1266 ms | 931 ms | 143 ms |
  | destroy, stopped guest | 345 ms | 39 ms | 119 ms |

  The overhead is the 4 MiB I/O pool plus about 6 MiB of host objects,
  whatever the RAM. Creating a guest no longer retypes its RAM: that
  moved into the guest's boot (create-to-ready minus create, about 130 ms
  for 128 MiB, was 57 ms). Phase 1 made create and destroy faster because
  the VMM no longer maps guest RAM.
- **Throughput** (`tests/run-bench.sh`, 1 vCPU). These runs were taken on
  a shared, heavily loaded development machine (load average about 10 on
  8 CPUs). Repeated runs of the same build differed by up to 3x, so the
  numbers below are only indicative and need repeating on a quiet host:

  | | main | this branch |
  |---|---|---|
  | disk write, 1 MiB direct | 376–1236 MB/s, 51–68 % CPU | 429–512 MB/s, 68–78 % CPU |
  | disk read, 1 MiB direct | 316–1167 MB/s, 88–91 % CPU | 447–598 MB/s, 82–88 % CPU |
  | disk 4K read latency | 69–163 µs | 351–374 µs |
  | guest↔guest TCP | 9–10 MB/s | 15–18 MB/s |

  A slower disk path is expected, because every request is now copied
  through the swiotlb pool. Earlier runs on a quieter machine gave phase 1
  974 MB/s write and 750 MB/s read, against 1148–1210 and 910–1182 on
  main. The 4K latency gap is larger than the copy alone explains, and
  has not been investigated yet.

### 15.3 Converting blocks between frames and objects (phase 3)

- **At boot** the guest makes object blocks only of what onlining needs
  (a 16 KiB RAM CNode and a 4 KiB page table per frame block, and a slot
  CNode), plus the reserve; everything else starts as frames. A
  4-vCPU 256 MiB dom0 boots with 6 object blocks (12 MiB), of which
  2.6 MiB is used.
- **Classes.** Objects come from chunks of four classes, each with its
  own open block: small objects, 4 KiB objects (TCBs), page tables, and
  CNodes. Page tables have a class of their own because they die
  together when a process exits or loses its VSpace; mixed with TCBs,
  they pinned their chunks and blocks did not drain.
- **Free blocks.** A block is free when none of its chunks has live
  objects; it then forgets its chunks. Free blocks are either in the
  **reserve** or **spare**. Allocations (in atomic context, under
  page-table and capability locks) take a spare block, else a reserve
  block, and never wait. A worker (`objmem_balance`, on the unbound
  workqueue, also run synchronously by `l4h_objmem_wait`) keeps the
  reserve at its target and returns spare blocks beyond the watermark to
  frame mode. Both targets default to one block per vCPU, at most 1/32 of
  RAM.
  - Deviation: the design's per-vCPU reserve is one reserve counted per
    vCPU; allocations take a global lock anyway.
- **Frame -> object** (`frames_to_objects`): an order-9 allocation from the
  page allocator (`GFP_KERNEL | __GFP_RETRY_MAYFAIL`, so it reclaims and
  compacts; an order-9 page is a whole aligned block), retrying past boot
  blocks, which have no Untyped. If this fails, the guest scans frame
  blocks with `alloc_contig_range`, isolating the selected range and
  migrating its pages synchronously. Direct compaction only uses
  `MIGRATE_SYNC_LIGHT` and can defer or skip busy and dirty pages; an
  order-9 failure alone does not establish OOM. Revoking the block's
  Untyped copy then deletes its 512 frames with every mapping and copy of
  them; the pages stay allocated, so they are out of the page allocator.
  Boot blocks and blocks already holding objects are excluded from the
  fallback scan.
- **Object -> frame** (`objects_to_frames`): the highest spare block is
  revoked (it has no children), gets its 512 frames back in its RAM CNode
  (which is kept), is mapped again, and its pages go back to the page
  allocator (`__free_pages` for buddy allocations, `free_contig_range`
  for isolated ranges, or `free_reserved_page` for blocks that were
  object blocks since boot). Isolated ranges hold one reference per page
  and must not be freed as a single order-9 allocation.
- **When there is no memory for objects.** Atomic callers get `-ENOMEM`:
  a TLB sync that cannot map a page leaves it unmapped and goes on (its
  unmaps must all happen; before, an error ended the sync early), so the
  page faults again. The fault path and the creation of a user context,
  which may sleep, call `l4h_objmem_wait()`; if no block could be
  converted (no free 2 MiB range), `l4h_objmem_oom()` calls the OOM
  killer, as a failed page-table allocation would on native Linux.
  Creating a user context no longer panics on a failure.
- **Accounting.** `/proc/meminfo` has `L4hObjects` (RAM in object blocks,
  out of the page allocator) and `L4hObjectsUsed`; `/proc/l4h_objmem`
  shows block counts, targets and conversions, plus `contig_attempts`
  and `contig_successes` for the synchronous migration fallback.
- **Test** (`tests/run-objtest.sh`): a fragmentation regression fills most
  free RAM, retains one movable anonymous page per physical 2 MiB block,
  and discards the rest. With direct compaction's fragmentation threshold
  set to 1000 and proactive compaction disabled, a child touches a page
  every 2 MiB of a 16 GiB mapping.
  This must grow object memory through synchronous migration and preserve
  the retained pages' contents. Before the fallback, this reproduced an
  objmem OOM with 162 MiB free in a 256 MiB guest.
  The stress phases use 8 processes that each
  touch a page every 2 MiB of a 1 GiB mapping (4096 page tables, 16 MiB):
  object blocks grow by converting frame blocks, and go back when they
  exit. The same three times while `selftest`'s
  `smp_mm_stress`, `smp_unmap_vis` and `smp_fork_storm` run, five times.
  Then a process maps sparse memory until RAM is exhausted: the OOM
  killer kills it, the guest carries on, and conversions work afterwards.
  With the fallback, a 1-vCPU 256 MiB run made nine synchronous
  conversions in the fragmentation regression; the later exhaustion
  phase reached 6 MiB free before OOM and recovered successfully.
  The full suite also passed with 4 vCPUs, using ten synchronous
  conversions in the fragmentation regression.
- **Fragmentation** (§9 of the design): long-lived objects can still pin
  partly empty chunks, and unmovable Linux pages can prevent a whole
  frame block from being isolated. Compaction of object memory (rebuilding
  VSpace mirrors into fresh blocks) remains future work.
- Phase 3 is shared by x86_64 and AArch64; only the paging-structure
  operations differ. The measurements above are from x86_64.
