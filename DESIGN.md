# l4h: Design Proposal

l4h is a hypervisor built on seL4 that runs Linux VMs without hardware
virtualization. It targets public-cloud instances where VT-x and SVM are
unavailable or nested virtualization is disabled. Guests are paravirtualized:
the guest Linux kernel is ported to run as an ordinary seL4 user-level program,
and guest user processes run natively in their own seL4 address spaces. l4h
never emulates an instruction.

Status: **design accepted, with the review decisions recorded in §14**.
Nothing here is implemented yet.

---

## 1. Requirements

| # | Requirement | How the design meets it |
|---|-------------|-------------------------|
| R1 | No hardware virtualization. Paravirtualize, as Xen PV does. Avoid instruction emulation. | The guest kernel is a new Linux architecture, `ARCH=l4h`, that runs in ring 3 as an seL4 thread and calls seL4 directly. Guest user code runs unmodified. Its `syscall` instructions and exceptions reach the guest kernel as seL4 fault IPC. |
| R2 | x86_64 first, but friendly to aarch64. | Each layer is split into ISA-independent and ISA-specific parts (§11). seL4, rust-sel4 and sDDF already support aarch64. Nothing in the design relies on an x86-only mechanism such as segmentation, rings 1/2 or Xen-style page-table validation. |
| R3 | virtio-blk and virtio-net. Block devices are passed through. Network is multiplexed by virtual switches. | The guest runs stock Linux `virtio_blk`/`virtio_net` drivers over a new paravirtual transport, `virtio-l4h`, with no trapped MMIO (§8). Block requests are forwarded zero-copy to a physical disk, partition or namespace assigned to that guest. For networking, the hypervisor provides **only L2 virtual switches**. A VM can have several vNICs, each on a switch, and a physical NIC is just an uplink port on a switch. Routing and NAT, when wanted, are done by a VM. |
| R6 | A runtime management API, available to "dom0", the first guest started at boot. | `l4h-root` exposes a control API through a `l4hctl` paravirtual device that only dom0 receives (§9.1). dom0 has no device-driver duties. |
| R7 | Targets: QEMU with virtio devices first, then GCP. | QEMU uses virtio-net-pci, virtio-blk-pci and nvme. GCP uses gen-3+ machine types (C3, N4, C4) with NVMe disks and gVNIC (§10). |
| R4 | SMP guests. | Each guest vCPU is an seL4 kernel-context thread with its own scheduling context. IPIs are seL4 notifications (§6). |
| R5 | Many guests per host, e.g. 30 guests on 4 CPUs. | seL4 MCS scheduling contexts give each vCPU a budget. Idle guests consume no CPU because the guest kernel is tickless and idle means blocked in `seL4_Recv`. A host balancer moves vCPUs between cores (§7). Each guest's kernel objects are charged to an Untyped quota assigned to that guest. |

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
| **L4Linux** (TU Dresden / Kernkonzept, Linux on Fiasco.OC). | **The closest model.** L4Linux is a maintained out-of-tree `arch/l4` with x86-64 and arm64 subarches. The guest kernel is a user-level task, and each guest process is a microkernel task whose syscalls and faults arrive as IPC. It relies on Fiasco's *vCPU mode*, which seL4 lacks. §5 shows how seL4 threads, priorities and two small kernel extensions replace it. |
| **Wombat / OK Linux** (NICTA, L4/seL4 era). | A historical precedent for Linux para-virtualized on L4-family kernels. |
| **gVisor systrap.** | Different goal: it reimplements Linux rather than running it. It confirms that trapping syscalls from unmodified binaries without VT-x is practical in production. |
| **seL4 Microkit / LionsOS / sDDF / libvmm.** | **Reuse the drivers and queue protocols.** sDDF has drivers for x86 virtio-pci net and blk, NVMe and an HPET/TSC timer, plus an `extern` OS shim, so it runs outside Microkit. The libvmm hardware-virtualization VMM and Microkit's static system model do not fit dynamic cloud guests, so we do not adopt them. |

The result: **a new Linux architecture (`arch/l4h`), modelled on L4Linux and
UML, running on an MCS SMP seL4 kernel with two small kernel extensions. Host
components are written in Rust on rust-sel4, with sDDF C drivers.**

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
 │  │ l4h-vmm (A)      │ one per guest, maps guest A's RAM only                │
 │  │ virtio backends  │                                                       │
 │  └──┬────────────┬──┘                                                       │
 │     │ sDDF blk   │ sDDF net queues                                          │
 │  ┌──▼───────┐  ┌─▼─────────────┐                                            │
 │  │ blk-mux  │  │ L2 vswitch(es)│  physical NIC = one uplink port            │
 │  └──┬───────┘  └──────┬────────┘                                            │
 │  ┌──▼───────────┐  ┌──▼───────────────┐                                     │
 │  │ sDDF NVMe /  │  │ sDDF virtio-net /│                                     │
 │  │ virtio-blk   │  │ l4h gVNIC driver │                                     │
 │  └──────────────┘  └──────────────────┘                                     │
 └─────────────────────────────────────────────────────────────────────────────┘
   seL4 microkernel (MCS, SMP, x86_64), plus l4h extensions E1–E3 (§4)
   Cloud VM hardware (QEMU/KVM, GCE …) with no VT-x exposed
```

Trust: the seL4 kernel, `l4h-root`, the drivers, blk-mux and vswitch form
the host TCB. dom0 is trusted for control-plane decisions (§9.1) but cannot
read other guests' memory. Each `l4h-vmm` is trusted only by its own
guest: it maps that guest's RAM and nothing else, so a compromised VMM can harm
only that guest. Guest kernels are untrusted. Capabilities confine them, and
the hypervisor never validates guest page tables.

---

## 4. seL4 kernel configuration and extensions

Configuration:

- `KernelIsMCS=ON`: scheduling contexts with budget and period are required for
  multi-tenant fairness (§7).
- `KernelMaxNumNodes`: up to 64 cores.
- `KernelFastpath=ON`.
- `KernelFSGSBase=inst`.
- `KernelXSave` with `KernelXSaveFeatureSet` covering x87, SSE and AVX (and
  AVX-512 when present), so that guest userspace sees the same ISA as a normal
  Linux VM.
- Spectre/Meltdown mitigations on: `KernelSkimWindow`, IBRS options.

Release builds keep debug syscalls off.

The kernel is pinned to a recent seL4 master; at review time that was
16.0.0-dev at `357c600`. We carry a **small, upstreamable patch set**:

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
timer (the generic timer on arm64) for budget enforcement, so the cheapest,
most portable option is to let user level share it.

E3 adds a small `Timer` kernel object, retyped from Untyped and bound to a
notification:

- `seL4_Timer_Set(timer, deadline_tsc)` arms or re-arms it, and
  `seL4_Timer_Clear` disarms it.
- At the deadline the kernel signals the bound notification.
- Armed timers sit in a per-core list ordered by deadline. The kernel programs
  the hardware with the minimum of the next budget event and the head of that
  list.
- A timer fires on the core it was last armed on. Migrated vCPUs re-arm on
  their new core.

Each vCPU gets one Timer object. The guest clockevent calls `Timer_Set`
directly, so there is **no user-level timer server** and no IPC hop. We expect
fewer than about 200 armed timers per host, so an O(n) insert is fine. The
patch is roughly 200 lines.

### Not required

Unlike Fiasco vCPU mode, we need no new interrupt-injection primitive (§5.4).
We need no IOMMU, since no device is passed into a guest.

### Found during implementation

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
self-contained, and the only change outside `arch/l4h` is a one-line kbuild
patch.

### 5.1 Layout in the Linux tree

```
arch/l4h/                         ISA-independent
  kernel/   boot, setup, irq, time, smp, process (switch_to), signal glue, ptrace glue
  mm/       software page tables -> seL4 VSpace mirroring, uaccess, fault path
  drivers/  virtio_l4h transport, hvc console, l4h irqchip, clockevent
  include/asm/sel4/   vendored libsel4 headers (BSD-2) and generated invocations
arch/l4h/x86_64/                  ISA-specific
  pt_regs <-> seL4 fault message, syscall entry/return, sigframe (x86 rt_sigframe),
  vDSO build (reuses arch/x86/entry/vdso sources), TLS (fs_base), ELF/hwcaps, cpufeature
arch/l4h/arm64/                   (future) same items for AArch64
```

As UML does with `SUBARCH`, we build with `make ARCH=l4h SUBARCH=x86_64`. The
**userspace ABI is exactly the native x86_64 Linux ABI**: the same uapi
headers, `syscall_64.tbl`, signal frame layout and vDSO. Unmodified distro
userlands therefore run. We maintain the code as a patch series against the
current Linux LTS (6.18 at the time of writing), rebased once per LTS.
Changes outside `arch/` should be close to zero; UML and L4Linux need almost
none.

### 5.2 Address spaces

- **Guest-kernel VSpace**, one per guest:
  - the kernel image;
  - a **direct map of all guest RAM**: guest-physical address (GPA) *g* maps
    to `PAGE_OFFSET + g`, using 4 KiB frames (see §5.3);
  - vmalloc/module area;
  - per-vCPU event pages;
  - virtio-l4h device pages.

  All vCPU kernel TCBs of the guest share it.
- **Guest-process VSpaces**, one seL4 VSpace per Linux `mm_struct`, created by
  the guest kernel from its own Untyped quota. They hold only user mappings,
  plus vDSO and vvar.
- The two kinds are disjoint, and that disjointness is what isolates the guest
  kernel from guest user code. Every seL4 mapping is user-accessible, so the
  kernel's memory must not be mapped into a process VSpace at all. Kernel and
  user VA ranges do not overlap numerically either: the kernel sits near the
  top of the 47-bit range (`seL4_UserTop = 0x7ffffffff000`), and `TASK_SIZE`
  is slightly smaller than native, for example `0x7f0000000000`. Generic code
  that compares against `TASK_SIZE` therefore still works.
- **uaccess** (`copy_{to,from}_user`, `get_user` and the rest) walks the
  guest's Linux page tables in software and goes through the direct map, as
  UML and L4Linux do. A missing page calls `handle_mm_fault` inline. The cost
  is a few hundred ns per page-crossing access, which is acceptable.

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
- **VSpace pool.** x86_64 seL4 has only 4096 ASIDs in total (8 pools of
  512). Each guest therefore gets a fixed pool of pre-assigned VSpace roots,
  and the guest kernel recycles them least recently used first. An mm that
  loses its VSpace simply refaults: every successful page fault installs
  the PTE directly.
- **CSpace naming.** A CNode capability held directly in a root slot cannot
  be named by a 64-bit CPtr, because seL4 keeps resolving through it.
  Capabilities that must be named, such as the root CNode itself and user
  threads' CSpaces, live in a "meta" CNode in root slot 1 (see `abi.h`).
- **Guest RAM.** `l4h-root` allocates guest RAM as 4 KiB frame caps from
  physically contiguous Untyped chunks (at most 8 chunks, 2 MiB aligned).
  GPA→HPA translation is therefore a tiny table, which zero-copy block DMA
  relies on (§8.2). Each guest kernel receives:
  - its RAM frame caps;
  - a CNode;
  - an **Untyped quota** for its own page tables, CNodes, TCBs, reply objects
    and endpoints.

  Running out of quota is a guest-local OOM. Tearing down a guest is a single
  revoke of its Untyped.
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

uTCBs are created lazily from the guest's quota. Keeping one TCB per thread,
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
  The guest kernel is built with `CONFIG_PREEMPT` or `PREEMPT_LAZY`.
- **Phase-2 backstop.** If the event page shows an unacknowledged event older
  than N ms while kTCB is running, the guest's `l4h-vmm` performs an *upcall
  injection*: Suspend, ReadRegisters, push an interrupt frame, WriteRegisters
  with IP set to `l4h_irq_entry`, and Resume. This is the pre-vCPU-mode
  L4Linux technique, and it should rarely fire.
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
The clockevent is a per-vCPU one-shot deadline on that vCPU's E3 Timer
object, bound to `N_v`.

**Signals.** The kernel builds the native x86 `rt_sigframe` in user memory
through the direct map. It fetches FPU and XSAVE state with E2, then rewrites
the uTCB's registers with `WriteRegisters`. `rt_sigreturn` reverses this.

**ptrace and coredump** use ReadRegisters/WriteRegisters and E2.

### 5.5 Calls from the guest to the host

The guest kernel makes plain seL4 invocations on capabilities it holds:

- Map and Unmap;
- TCB, SchedContext Bind and Unbind;
- Signal;
- Call to its VMM endpoint for device configuration, power-off and reboot.

**No hypercall needs validation.** Isolation follows from the CSpace that
`l4h-root` hands the guest:

- the guest's own frames and its Untyped;
- its vCPU SCs, which it can bind but not configure, since it holds no
  SchedControl;
- its own endpoints;
- an MCP no higher than `P_K`.

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
- **Lock-holder preemption.** vCPUs are preempted by the host, so spinlocks use
  a paravirt slow path. `arch/l4h` owns its spinlock implementation, using
  qspinlock with spinning bounded at around 2^10 iterations. After that the
  waiter blocks on its notification, and the holder kicks the next waiter on
  unlock. This is the same idea as KVM/Xen `pv_qspinlock`, but expressed with
  seL4 notifications.
- `steal_time` comes from `seL4_SchedContext_Consumed` deltas, which the
  balancer publishes in the event page. Guest CFS can then account for steal.

---

## 7. Host scheduling and many guests per core

- **Priorities**, global on each core:

  | Band | Components |
  |------|------------|
  | highest | NIC/disk drivers |
  | | vswitch, blk-mux |
  | | l4h-vmm instances |
  | | guest `P_K` |
  | lowest | guest `P_U` |

  Every band has MCS budgets, so no component can starve the bands below it.
- **Per-vCPU budgets.**
  - `SC_K` and `SC_U` each get period T (default 5 ms) and a budget derived
    from the guest's CPU *weight* and *cap*, as configured.
  - Sums across a core may exceed 100%, which is overcommit. Inside the guest
    band, threads with budget left are scheduled round-robin.
  - Caps are hard: a guest never exceeds its configured vCPU count times its
    cap.
- **Placement and balancing.** seL4 does no load balancing, so `l4h-root`
  runs a balancer thread at low frequency (about 100 ms):
  - It reads `Consumed` for every SC.
  - It moves vCPUs between cores by re-running `SchedControl_Configure` on
    another core's SchedControl cap. `SC_K` and `SC_U` of a vCPU always move
    together.
  - It keeps each guest's vCPUs on distinct cores where possible, which limits
    lock-holder preemption.
  - It optionally keeps a guest on one SMT sibling set, to reduce side
    channels.
- **Fairness caveat.** Because `P_K > P_U` for every guest, a guest's kernel
  work can briefly pre-empt another guest's user work, bounded by `SC_K`
  budgets. If measurements show this matters, the balancer can lower `P_K` for
  vCPUs that burn kernel time, or we can trial `seL4_SchedContext_YieldTo`
  from interrupt senders.
- **Memory sizing for 30 guests.** Guest RAM is reserved in full at creation.
  Per-guest seL4 kernel-object overhead is about 1–3% of RAM: CNode slots
  (32 B per mapping), PTs and TCBs, all charged to that guest's quota.
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
- **CPU caps** (`vm_set_sched`, `l4hctl sched`) set the budget of both SCs
  to a percentage of the 5 ms period. Weights and steal-time reporting
  are not implemented yet. `l4hctl top` shows each VM's CPU use over the
  last period.
- **Memory.** Free memory is a set of untouched power-of-two Untyped
  blocks, managed buddy-style without coalescing. An allocation splits
  the smallest block that fits, so guest RAM chunks never lose memory to
  alignment. Reclaimed guest memory returns as blocks. A 256 MiB guest
  costs about 317 MiB:
  - its RAM;
  - its seL4 object quota, by default 16 MiB plus RAM/16, used for its
    processes' mapping slots and page tables;
  - about 30 MiB of host objects, including its `l4h-vmm`.
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
  probing. Vrings live in guest RAM and are addressed by GPA.
  `VIRTIO_F_ACCESS_PLATFORM` is not negotiated in v1.

### 8.2 Block, passthrough (zero-copy)

"Passthrough" here means that **a guest's virtio-blk disk is a raw physical
backing**: a whole disk, an NVMe namespace or an LBA range or partition,
assigned to exactly one guest. There is no image format, no host filesystem
and no copy.

- **Path.** Guest `virtio_blk` → `l4h-vmm` (parses descriptor chains) → sDDF
  blk queue → `blk-mux` → sDDF NVMe or virtio-blk driver → device.
- **Zero-copy DMA.**
  - The VMM puts the **GPA** of each guest data segment into sDDF requests.
    The guest's RAM is registered with `blk-mux` as that client's data region.
  - `blk-mux` bounds-checks each segment against the client's RAM extents and
    LBA window, then translates GPA→HPA with the chunk table (§5.3).
  - The device then DMAs directly into guest memory.

  This is the existing sDDF `io_or_offset` scheme, extended from one region to
  a chunk table.
- **No IOMMU.** Cloud VMs rarely expose one. Isolation therefore depends on the
  trusted `blk-mux` and driver validating addresses; the untrusted guest never
  touches device registers. If an IOMMU is present, seL4 VT-d IOSpaces can
  confine the device itself, as defence in depth.
- **blk-mux, written in Rust.** It replaces sDDF's static `blk/components/virt.c`
  with dynamic client attach and detach. It also adds per-guest LBA windows,
  read-only flags, flush and FUA passthrough, discard/write-zeroes, and
  per-guest IOPS and bandwidth limits using token buckets.
- Multi-queue: one virtqueue per guest vCPU (`VIRTIO_BLK_F_MQ`), and one sDDF
  queue pair per guest.
- **Revised during implementation (v1).**
  - One `blkd` per attached disk, with no blk-mux: a disk, or a window of
    one, belongs to exactly one guest.
  - blkd has two drivers behind one trait:
    - virtio-blk PCI, which uses an indirect descriptor table per request;
    - NVMe, with one I/O queue pair and namespace 1. Its 4 KiB pages and
      PRP lists are sized to the controller's MDTS. A request is split
      into several commands when its pieces cannot share one PRP list.
  - The common part checks every request:
    - each segment's GPA lies in the client's RAM;
    - the data direction matches the operation;
    - the request is aligned to the logical block size, which is passed to
      the guest as virtio `blk_size`;
    - it stays within the LBA window (`start`/`sectors`).
    Writes are refused on read-only attachments.
  - There is still a single queue: no `VIRTIO_BLK_F_MQ`, discard or write
    zeroes yet.
  - Tested with QEMU NVMe at 512-byte and 4 KiB LBAs
    (`BLKTEST_DEV=nvme tests/run-blktest.sh`), and with a read-only window
    in `tests/run-ctltest.sh`.

### 8.3 Network: L2 virtual switches only

**Principle.** The hypervisor implements only Ethernet switching. Any L3
function, such as routing, NAT, DHCP or firewalling, belongs in a VM.

- **Path.** Guest `virtio_net` → `l4h-vmm` (net backend) → sDDF net queues →
  `vswitch` → (uplink) NIC driver. The NIC driver is the sDDF virtio-net-pci
  driver on QEMU and the l4h gVNIC driver on GCP (§10).
- **Multiple vNICs per VM.** A guest may have up to 16 virtio-net devices.
  Each one is a port on exactly one switch, has its own MAC and queue pairs,
  and appears to the guest as a separate `ethN`.
- **Switches.** The control API (§9.1) can create any number of switches.
  Each is an isolated L2 broadcast domain. One vswitch component serves all
  of them, with per-switch tables, and can be sharded per core later.
- **Uplinks.** A physical NIC is attached as an ordinary *uplink port* on one
  switch, so frames are switched to and from it like any other port. A NIC
  can be attached to at most one switch, and a switch can have no uplink,
  which makes it internal-only.
- **vswitch, written in Rust.** It reuses the sDDF net queue format but
  replaces sDDF's static `vswitch.c`, which has 63 clients and fixed
  allow-lists, with dynamic ports. It provides:
  - MAC learning with ageing, plus optional static MAC entries;
  - broadcast, multicast and unknown-unicast flooding, with per-port rate
    limits;
  - optional 802.1Q access and trunk ports;
  - per-port MTU, including jumbo frames where the uplink allows.
  - **L2 port security:** a port can be locked to its configured source MAC.
    It is on by default for guest ports. Nothing above L2 is inspected.
- **Routing is done by a VM.** On GCP the VPC delivers only frames for the
  instance's own MAC and IP addresses (or alias IP ranges). The usual topology
  is therefore:
  - The uplink switch holds the gVNIC and one vNIC of a *router VM*. That VM
    may be dom0 or a dedicated guest, and the control API configures its vNIC
    to use the gVNIC's MAC.
  - Internal switches connect the other guests to the router VM's other
    vNICs.
  - The router VM does NAT, or routes GCP alias IP ranges to the guests,
    using standard Linux tooling.

  On QEMU or bare metal, guests can sit directly on the uplink switch instead.
- **Copies.** v1 has two copies on each path:
  - tx: guest buffer → vswitch-owned buffer, done by the VMM;
  - rx: vswitch buffer → guest buffer, done by the VMM.

  The vswitch only moves descriptors. Guest-to-guest traffic costs one copy
  into a shared buffer and one copy out.
- **Later optimization.** A per-guest *I/O window* inside guest RAM would act
  as the sDDF client data region. The guest would use a restricted-DMA pool,
  forcing swiotlb for `virtio_net` with `ACCESS_PLATFORM`, so the vswitch
  copies window to window and the VMM copies nothing.
- **Revised during implementation (v1).**
  - One component, `netd`, holds every switch and also drives the uplink
    NICs (virtio-net PCI on QEMU). Uplinks are ports of a switch like any
    other, so a separate driver component would only add a hop.
  - A guest vNIC is a netd *port*: a shared region with TX and RX rings of
    2 KiB frame slots between the guest's `l4h-vmm` and netd. `l4h-root`
    adds and removes ports with calls to netd (`NETD_ADD_PORT`,
    `NETD_REMOVE_PORT`); port memory belongs to the guest's arena.
  - Copies: the VMM copies guest buffers to and from slots, and netd copies
    slot to slot, or slot to NIC DMA buffer. That is one copy more than the
    plan above, and it keeps netd's memory independent of every guest.
  - Offloads are not negotiated yet: guests compute checksums.
  - MAC learning has 300 s ageing. Unknown unicast, broadcast and multicast
    are flooded within the switch. Locked ports (the default) drop frames
    whose source MAC is not the port's MAC.
  - `tests/run-nettest.sh` covers DHCP and ping over the QEMU user-net
    uplink, guest-to-guest pings on internal switches, a guest with two
    vNICs, switch isolation and MAC lock.
- Offloads: checksum offload and TSO/GSO are passed through as metadata
  (virtio-net header). Software segmentation and checksumming happen at the
  uplink port when the NIC lacks the feature; this is still L2 framing work,
  not routing. mrg_rxbuf and multiqueue are on.

### 8.4 Console and other devices

- **Console**: an `hvc_l4h` driver, a shared ring to the VMM, which feeds the
  sDDF serial virtualiser (pc99 UART) and per-guest log buffers.
- virtio-rng is backed by RDRAND/RDSEED in the VMM.
- `l4hctl`, dom0 only (§9.1).
- Later: virtio-vsock, virtio-balloon.

---

## 9. Host components

All host components are Rust: no_std, using the rust-sel4 crates `sel4` and
`sel4-root-task`.

**Revised during implementation.** Device drivers are also written in
Rust, following sDDF's design (shared queues, zero-copy block requests,
virtualiser/switch components) rather than linking sDDF's C drivers.
sDDF's components assume a static, Microkit-shaped system, while l4h
attaches and detaches guests at runtime. A single language and runtime for
the host keeps that dynamic plumbing simple.

| Component | Instances | Role |
|-----------|-----------|------|
| `l4h-root` | 1 | Root task. Takes all Untyped and device memory from bootinfo, creates the services, and runs the guest lifecycle: create, start, stop and destroy. Allocates RAM chunks, quotas, SCs and Timer objects, runs the balancer, and is the authority for blk and net attachments. It implements the control API (§9.1). |
| `l4h-vmm` | 1 per guest | Boots the guest: loads the ELF and initramfs, then builds the boot-info page and the kernel VSpace with the direct map. Hosts the guest's virtio backends and console, handles power events, and runs the upcall-injection backstop. dom0's VMM also hosts the `l4hctl` backend. |
| `blk-mux` | 1 | Multiplexes physical disks for blk clients (§8.2). |
| `vswitch` | 1 (n switches) | §8.3. |
| drivers | 1 per device | sDDF NVMe, virtio-blk-pci, virtio-net-pci and pc99 serial, plus the sDDF PCI library. Also the l4h gVNIC driver, ported from Linux `gve`, which is dual MIT/GPL licensed. |

**Guest boot protocol.** The guest boots with no BIOS and no bootloader.
`l4h-vmm` maps the `vmlinux` ELF and an initramfs, then starts vCPU0 at the
entry point with `rdi` pointing at an `l4h_boot_info` page. The page holds:

- an ABI version;
- the RAM extents;
- the vCPU count;
- the CSpace layout, meaning cap slot indices for frames, Untyped, the vCPU
  objects and device endpoints;
- the device table;
- the command line;
- the TSC frequency;
- the event-page addresses.

The ABI header (`l4h_abi.h`) is the single source of truth. Rust bindings are
generated from it with bindgen, and the guest includes it directly.

### 9.1 Control plane: dom0 and the runtime API

**Starting dom0.** At boot, `l4h-root` reads a small *host manifest*, a boot
module in TOML. The manifest describes host-level settings and **dom0 only**:

- dom0's kernel, initramfs and cmdline (also boot modules);
- its vCPUs and memory;
- its disks, for example a partition of the boot disk plus any disks to
  manage;
- its vNICs, for example one on a management switch with the uplink.

dom0 is guest #0. It is a normal l4h Linux guest with one extra device,
**`l4hctl`**, which carries a badged capability to the control endpoint of
`l4h-root`. No other guest ever receives that capability. dom0 runs no host
device drivers: disks and NICs stay with the host components. This makes it
much smaller in role than Xen's dom0.

**API surface.** Requests and responses are fixed-layout, little-endian,
versioned structs defined in `l4h-abi`.

| Area | Operations |
|------|------------|
| Host | `host_info` (cores, memory free/total, physical disks and namespaces, physical NICs, ABI version), `host_stats` |
| Guests | `vm_create` (vCPUs, memory, CPU weight/cap, kernel cmdline), `vm_load_image` (kernel/initramfs bytes, see below), `vm_start`, `vm_shutdown` (ACPI-like request to the guest), `vm_destroy` (hard kill and reclaim), `vm_pause`/`vm_resume`, `vm_list`, `vm_info`/`vm_stats` (CPU consumed, steal, I/O counters), `vm_set_sched` |
| Block | `blk_attach(vm, backing, ro)`, where the backing is a whole disk, an NVMe namespace or an LBA range. `blk_detach`, with hotplug through the virtio-l4h device table. `blk_set_limits` (IOPS/bandwidth). |
| Network | `switch_create`/`switch_destroy`, `switch_attach_uplink(switch, nic)`, `nic_create(vm, switch, mac, queues, mac_lock, vlan)`, `nic_destroy`, `switch_info` (ports, MAC table, counters) |
| Console | `console_open(vm)` returns a stream of guest console bytes in and out, multiplexed over the l4hctl device |
| Events | an asynchronous event queue for guest exits, crashes and console data |

**Transport.**

- `l4hctl` is a virtio-l4h device of a private type, with a request virtqueue
  and an event virtqueue.
- Image upload is chunked: `vm_load_image` names a buffer in dom0's RAM, and
  `l4h-vmm`/`l4h-root` copies it into the new guest's RAM. dom0 can therefore
  fetch images from anywhere, such as a disk, the network or GCS.
- In dom0 the driver exposes `/dev/l4hctl` and passes messages through
  unchanged.
- A Rust userspace tool and library, `l4hctl`, provides a CLI. Cloud
  orchestration (a REST or gRPC agent) can be built on it later, in dom0
  userspace and outside the hypervisor.

**Revised during implementation (v1).**

- The `l4hctl` device is a single shared page rather than virtqueues:
  dom0 writes a fixed-layout request (`struct l4h_ctl_req` in `abi.h`) and
  issues `L4H_HC_CTL`. dom0's `l4h-vmm` forwards the call to `l4h-root`,
  which executes it synchronously and writes the response into the same
  page. Requests are short except image loads, which are bounded at 4 MiB
  per call.
- `/dev/l4hctl` takes one ioctl that carries a request, its response and,
  for `VM_LOAD`, a user buffer. The driver stages the buffer in physically
  contiguous memory and passes its GPA.
- `l4hctl` parses the guest kernel ELF in dom0 userspace and loads it with
  raw `VM_LOAD(vm, gpa, bytes)` writes. `l4h-root` copies page by page from
  dom0's frames to the new guest's frames and never parses guest images.
  The lifecycle is `vm_create` → `blk_attach`* → `vm_load`* → `vm_start`
  (writes the boot info and spawns the guest's `l4h-vmm`) → `vm_destroy`.
- Console output goes into a per-guest ring that `l4h-vmm` writes and
  `l4h-root` reads for `console_read`. It is also copied to the serial port
  with a `[vmN]` line prefix. Console input is a ring in the guest's
  console device page, which hvc polls.
- **Reclaim.** Each guest's objects come from its arena: arena Untypeds,
  RAM chunks, quota and the arenas of its blkds. Root CNode slots allocated
  on the guest's behalf are tracked. `vm_destroy` stops the attached devices'
  DMA (clears PCI bus mastering), revokes those Untypeds, returns them to
  the free pool and deletes the tracked slots. ASIDs are released by the
  kernel when VSpaces die. Repeated create/destroy leaves free memory
  unchanged (`tests/run-ctltest.sh`).

**Policy and safety.**

- `l4h-root` enforces resource accounting (memory, cores, disk ownership: a
  backing is attached to at most one VM unless it is read-only) and rejects
  invalid requests. dom0 cannot breach host isolation, but it is fully trusted
  to decide *what* runs. In particular, dom0 can attach any disk to itself.
- If dom0 crashes, the other guests keep running. `l4h-root` restarts dom0
  according to the manifest's restart policy, and the new dom0 recovers state
  with `vm_list` and `switch_info`. The authoritative state lives in
  `l4h-root`.
- Debugging fallback: a minimal serial monitor in `l4h-root` (`list`,
  `destroy`, `restart-dom0`) stays available on COM1, which is the GCE serial
  console.

---

## 10. Boot and deployment

- **Firmware.** UEFI → GRUB (EFI) → multiboot2, which seL4 supports. On QEMU
  this is OVMF, or plain `-kernel` multiboot for quick development loops. The
  modules are:
  - the seL4 kernel;
  - `l4h-root`, which embeds the service ELFs as a CPIO archive;
  - the host manifest;
  - dom0's kernel and initramfs.

  Images for the other guests come through the dom0 API.
- **Platform discovery** is done by seL4 using ACPI: MADT, cores and IOAPIC.
  `l4h-root` handles PCI enumeration over MCFG/ECAM, or port I/O config, and
  MSI/MSI-X allocation through `seL4_IRQControl_GetMSI`.

### 10.1 Targets, in order

1. **QEMU (development and CI).** `-cpu host,-vmx` with KVM used only as a
   fast CPU. Devices: virtio-net-pci, virtio-blk-pci, nvme and a 16550 UART.
   All the drivers already exist in sDDF.
2. **GCP.**
   - **Machine types:** third-generation and later (C3, C3D, N4, C4).
     These support only gVNIC and expose Persistent Disk / Hyperdisk as NVMe.
   - **Drivers:** we need one new driver, gVNIC, ported from Linux `gve`.
     NVMe is reused from sDDF.
   - **Early bring-up:** a second-generation type (N2), where virtio-net is
     still offered and the boot disk is attached with the NVMe interface, so
     it runs with only existing drivers.
   - **Boot:** a custom image with GRUB EFI and seL4. Secure Boot is off
     (Shielded VM vTPM and integrity monitoring are optional). The serial
     console is enabled for seL4 and monitor output.
   - **Timer:** E3 uses the LAPIC TSC-deadline timer, which KVM-based GCE
     exposes, so there is no HPET dependency. Invariant TSC frequency comes
     from CPUID leaf 0x15/0x40000010 or calibration, as seL4 does today.
   - **Networking:** the gVNIC sits on an uplink switch, and a router VM
     (often dom0) owns the instance MAC (§8.3). Multi-NIC instances give one
     uplink switch per gVNIC.
   - **To verify at first GCE boot:** ACPI tables, MSI-X, gVNIC queue format
     (GQI vs DQO; C3 and later use DQO), and whether x2APIC is present.
3. Later targets: AWS (ENA and NVMe) and Azure (MANA and NVMe), each mainly
   adding one NIC driver.

---

## 11. Portability to aarch64

| Concern | x86_64 | aarch64 |
|---------|--------|---------|
| seL4 support | MCS, SMP | MCS, SMP, and the best proof coverage |
| syscall trap (E1) | `syscall`, number in `rdx` | `svc`, number in `x7`; the same flag applies |
| FPU access (E2) | XSAVE area | V-regs + FPSR/FPCR |
| Guest VA | 47-bit | 48-bit TTBR0 |
| TLS | `fs_base` | `TPIDR_EL0` (`seL4_TCB_SetTLSBase`) |
| Timer (E3) | LAPIC TSC-deadline | ARM generic timer |
| vDSO clock | `rdtsc` | `cntvct_el0` |
| Drivers | sDDF virtio-pci, NVMe | sDDF virtio-mmio/pci, NVMe; ENA on Graviton |
| Boot | multiboot2 | seL4 elfloader / UEFI |

Rules that keep the port cheap:

1. ISA code lives only in `arch/l4h/<isa>/` and in `host/*/src/arch/<isa>.rs`.
2. Nothing in the protocols depends on the ISA: boot info, event pages,
   virtio-l4h, sDDF queues. Fields are fixed-width and little-endian.
3. No use of x86-only seL4 features such as I/O ports outside drivers, EPT or
   `GetStatusBits`.
4. Guest RAM uses 4 KiB frames, which every ISA supports.
5. CI builds both ISAs from day one, even before arm64 boots.

---

## 12. Performance expectations, to be measured

| Operation | Native KVM guest | l4h, estimate | Notes |
|-----------|------------------|---------------|-------|
| null syscall | ~50 ns | ~0.4–1 µs | Fault IPC slowpath plus ReplyRecv. An optional seL4 patch can fastpath UnknownSyscall replies. Comparable to Xen PV x86_64 or gVisor systrap. |
| page fault (anon) | ~1 µs | ~2–4 µs | Fault IPC, `handle_mm_fault` and `Page_Map`. |
| context switch | ~1–2 µs | ~2–3 µs | Two `SetSchedParams` and a reply. No FPU copying. |
| `clock_gettime` | vDSO | vDSO (rdtsc) | No trap. |
| fork+exec | baseline | 1.5–3× | Lazy refault. |
| disk I/O | | near native | Zero-copy; the main overhead is two queue hops. |
| network | | ~2 copies per direction | Needs the I/O-window optimization to compete. |

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
  host/              Cargo workspace: l4h-abi, l4h-root, l4h-vmm, vswitch, blk-mux, gvnic, sddf-shim
  tools/l4hctl/      dom0 userspace CLI and library for the control API
  third_party/sddf/  submodule (drivers)
  tools/             build.sh, mkimage (multiboot2 ISO / GCE image), run-qemu, guest rootfs builder (busybox)
  tests/             host-level integration tests driven by QEMU + expect-style console scripting
```

**Test environment.** This machine has KVM, so QEMU uses it only as a fast
CPU to mimic a cloud VM. The guest CPU is `-cpu host,-vmx -smp 4`, with
`virtio-blk-pci`, `nvme` and `virtio-net-pci` devices, and UEFI via OVMF or
plain multiboot. seL4 never uses VT-x. The dev box has 32 GiB of RAM, which
is reported as less because of host ballooning. The 30-guest test therefore
uses a 16 GiB QEMU machine and guests of 256–512 MiB.

**Milestones:**

1. **M0, platform.**
   - Toolchains and the seL4 MCS SMP build.
   - A Rust `l4h-root` hello world boots in QEMU on 4 cores.
   - E1, E2 and E3 implemented, each with sel4test-style unit tests.
2. **M1, single-vCPU guest (dom0 only).**
   - The manifest-driven dom0 boots `arch/l4h` to an initramfs shell over the
     hvc console.
   - Covers syscalls, page faults, signals, fork/exec, mmap/mprotect and
     futex, plus the E3 timer and the vDSO.
   - Gate: busybox, then a static test suite (LTP subset).
3. **M2, SMP guests.**
   - vCPU bring-up, IPIs and PV spinlocks.
   - Gate: `stress-ng` and LTP on 4 vCPUs; `make -j4` of a kernel inside the
     guest.
4. **M3, virtio-l4h and blk passthrough.**
   - blk-mux and the sDDF virtio-blk and NVMe drivers.
   - Gate: fio and fsck, with data integrity across guest reboots.
5. **M4, control API.**
   - The `l4hctl` device and `/dev/l4hctl`, the dom0 CLI, and the
     `vm_*` and `blk_*` operations.
   - Gate: dom0 creates, starts and destroys guests at runtime, and console
     attach works.
6. **M5, networking.**
   - vswitch with multiple switches, multiple vNICs per VM, and uplink ports.
   - The sDDF virtio-net driver and the netback in the VMM.
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
   - The upcall-injection backstop, the I/O window, and fuzzing of the VMM,
     vswitch and control-API parsers.
   - aarch64 bring-up on QEMU virt.

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
