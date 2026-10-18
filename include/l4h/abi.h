/* SPDX-License-Identifier: BSD-2-Clause */
/*
 * l4h guest <-> host ABI.
 *
 * This header is the single source of truth for the contract between
 * l4h-root / l4h-vmm and a paravirtualised guest kernel (ARCH=l4h). It is
 * included by the guest kernel and mirrored by host/l4h-abi (Rust).
 * Everything is fixed-width and little-endian; layouts must not depend on
 * the ISA.  See DESIGN.md §5.
 */
#ifndef L4H_ABI_H
#define L4H_ABI_H

#ifdef __KERNEL__
#include <linux/types.h>
typedef __u8 l4h_u8;
typedef __u16 l4h_u16;
typedef __u32 l4h_u32;
typedef __u64 l4h_u64;
typedef __s64 l4h_s64;
#else
#include <stdint.h>
typedef uint8_t l4h_u8;
typedef uint16_t l4h_u16;
typedef uint32_t l4h_u32;
typedef uint64_t l4h_u64;
typedef int64_t l4h_s64;
#endif

#define L4H_ABI_VERSION 1

/* ------------------------------------------------------------------------
 * Guest CSpace layout
 *
 * The guest's root CNode has 2^12 slots and a guard that makes it resolve
 * the top 46 bits of a CPtr (guard 34 bits of zero + 12 bits radix).  The
 * remaining 18 bits index second-level CNodes: host-created ones have
 * radix 18, guest-created ones must have radix + guard == 18.
 */
#define L4H_ROOT_CNODE_BITS     12
#define L4H_L2_INDEX_BITS       18
#define L4H_ROOT_GUARD_BITS     (64 - L4H_ROOT_CNODE_BITS - L4H_L2_INDEX_BITS)

/* In CNode operations with root = L4H_CNODE_CPTR, a slot of the root CNode
 * is addressed as (index = slot number, depth = L4H_ROOT_DEPTH), and a slot
 * of a second-level CNode as (index = L4H_CPTR2(root, idx), depth = 64). */
#define L4H_ROOT_DEPTH          (L4H_ROOT_GUARD_BITS + L4H_ROOT_CNODE_BITS)

/* CPtr of slot `idx` of the second-level CNode in root slot `root`. */
#define L4H_CPTR2(root, idx)    ((((l4h_u64)(root)) << L4H_L2_INDEX_BITS) | (l4h_u64)(idx))
/* CPtr of a capability held directly in root slot `root`. */
#define L4H_CPTR(root)          L4H_CPTR2(root, 0)

/* Root slot 1 holds a small "meta" CNode (radix 12, guard 6) whose slot 0
 * is a capability to the guest root CNode itself (with the 34-bit guard),
 * so that the root CNode can be named by a CPtr: L4H_CNODE_CPTR.  (A CNode
 * capability held directly in a root slot cannot be invoked, because seL4
 * keeps resolving through it.) */
#define L4H_SLOT_META           1
#define L4H_CNODE_CPTR          L4H_CPTR2(L4H_SLOT_META, 0)
/* Meta CNode slots [1, 4096) are free for the guest: any CNode capability
 * that must be named by a CPtr (e.g. the CSpace of a user thread) has to be
 * kept here, at L4H_CPTR2(L4H_SLOT_META, i). */
#define L4H_META_DYN_BASE       1
#define L4H_SLOT_VSPACE         2   /* guest-kernel VSpace root */
#define L4H_SLOT_HYP_EP         3   /* Call: hypercalls served by l4h-vmm */
#define L4H_SLOT_ASID_POOL      4   /* reserved */

#define L4H_SLOT_UNTYPED_BASE   16  /* Untyped quota, bootinfo.nr_untyped caps */
#define L4H_MAX_UNTYPED         48

/* Per-vCPU objects: root slots L4H_SLOT_VCPU(v) + L4H_VCPU_* */
#define L4H_SLOT_VCPU_BASE      64
#define L4H_VCPU_STRIDE         8
#define L4H_MAX_VCPUS           64
#define L4H_SLOT_VCPU(v)        (L4H_SLOT_VCPU_BASE + (v) * L4H_VCPU_STRIDE)
#define L4H_VCPU_KTCB           0   /* kernel-context TCB */
#define L4H_VCPU_EP             1   /* fault/IPC endpoint (receive side) */
#define L4H_VCPU_NTFN           2   /* notification bound to the kTCB */
#define L4H_VCPU_SC_U           4   /* scheduling context for user threads */
#define L4H_VCPU_IPI            5   /* N_v, badged L4H_BADGE_IPI (send only) */
#define L4H_VCPU_IPCBUF         6   /* frame: kTCB IPC buffer (+FPU scratch) */
#define L4H_VCPU_EVENT          7   /* frame: event page */

/* Guest RAM: root slot L4H_SLOT_RAM_CNODE(n) is a radix-18 CNode holding
 * the 4 KiB frame caps for GPA [n GiB, n+1 GiB). */
#define L4H_SLOT_RAM_CNODE_BASE 576
#define L4H_MAX_RAM_CNODES      64
#define L4H_RAM_CNODE_FRAMES    (1u << L4H_L2_INDEX_BITS)
#define L4H_FRAME_CPTR(gpa)                                             \
	L4H_CPTR2(L4H_SLOT_RAM_CNODE_BASE + ((gpa) >> 30),                  \
		  ((gpa) >> 12) & (L4H_RAM_CNODE_FRAMES - 1))

/* A CNode with bootinfo.nr_vspaces pre-assigned (ASID-bearing) VSpace
 * roots that the guest uses for its processes. */
#define L4H_SLOT_VSPACE_POOL    640
/* A CNode with device capabilities (indices from l4h_dev_desc). */
#define L4H_SLOT_DEV_CNODE      641

/* Root slots [L4H_SLOT_DYN_BASE, 4096) are free for the guest. */
#define L4H_SLOT_DYN_BASE       1024

/* ------------------------------------------------------------------------
 * Guest-kernel VSpace layout
 *
 * Guest RAM is mapped at bootinfo.ram_base_va: GPA g <-> VA ram_base_va + g.
 * The guest kernel ELF is linked so that its first loadable segment starts
 * at ram_base_va (GPA 0).  Host-provided pages live in the special region.
 */
#define L4H_SPECIAL_BASE        0x00007ff000000000ull
#define L4H_BOOTINFO_VA         L4H_SPECIAL_BASE
#define L4H_BOOTINFO_SIZE       0x4000ull
#define L4H_VCPU_AREA_VA(v)     (L4H_SPECIAL_BASE + 0x100000ull + (l4h_u64)(v) * 0x4000ull)
#define L4H_VCPU_IPCBUF_VA(v)   (L4H_VCPU_AREA_VA(v) + 0x0000)
#define L4H_VCPU_EVENT_VA(v)    (L4H_VCPU_AREA_VA(v) + 0x1000)
/* FPU transfers (seL4_TCB_ReadFPU/WriteFPU) use this offset in IPCBUF. */
#define L4H_FPU_SCRATCH_OFFSET  2048
#define L4H_DEV_AREA_VA(d)      (L4H_SPECIAL_BASE + 0x10000000ull + (l4h_u64)(d) * 0x10000ull)
/* First VA above the special region that the guest may never use. */
#define L4H_SPECIAL_END         (L4H_SPECIAL_BASE + 0x40000000ull)

/* ------------------------------------------------------------------------
 * Scheduling
 */
#define L4H_PRIO_KERNEL         100 /* kTCB priority and MCP */
#define L4H_PRIO_USER           99  /* priority of guest user threads */

/* ------------------------------------------------------------------------
 * Notification badges on N_v (bound to the vCPU's kTCB).  Endpoint badges
 * on EP_v (user-thread faults) always have L4H_EP_BADGE_TASK set, so a
 * badge received by seL4_Recv(EP_v) is unambiguous.
 */
#define L4H_BADGE_TIMER         (1ull << 0)  /* seL4_TCB_SetTimer expiry */
#define L4H_BADGE_IPI           (1ull << 1)  /* another vCPU kicked us */
#define L4H_BADGE_EVENT         (1ull << 2)  /* event page has pending bits */
#define L4H_EP_BADGE_TASK       (1ull << 62)

/* ------------------------------------------------------------------------
 * Event page (one per vCPU, shared with l4h-vmm).  Senders set bits with an
 * atomic OR and then signal N_v with L4H_BADGE_EVENT; the guest clears them
 * with an atomic exchange.
 */
#define L4H_NR_EVENTS           512
struct l4h_event_page {
	l4h_u64 pending[L4H_NR_EVENTS / 64];
	l4h_u64 steal_ns;      /* written by the host: time the vCPU was runnable
	                        * but not running, cumulative, monotonic */
	l4h_u64 idle_ticks;    /* written by the guest: time the vCPU blocked
	                        * idle, in time-counter ticks, cumulative (the
	                        * host derives steal_ns from it) */
	l4h_u64 reserved[502];
};

/* ------------------------------------------------------------------------
 * Boot info, mapped read-only at L4H_BOOTINFO_VA; the boot vCPU starts
 * with rdi (x0 on arm64) = L4H_BOOTINFO_VA.
 */
#define L4H_BOOTINFO_MAGIC      0x31544f4f4248344cull /* "L4HBOOT1" */
#define L4H_MAX_DEVICES         64
#define L4H_CMDLINE_SIZE        2048

enum l4h_dev_type {
	L4H_DEV_NONE = 0,
	L4H_DEV_VIRTIO = 1,     /* virtio-l4h transport, see l4h_virtio_regs */
	L4H_DEV_CONSOLE = 2,    /* hvc console ring */
	L4H_DEV_CTL = 3,        /* dom0 control channel (l4hctl) */
};

struct l4h_dev_desc {
	l4h_u32 type;           /* enum l4h_dev_type */
	l4h_u32 virtio_id;      /* VIRTIO_ID_* for L4H_DEV_VIRTIO */
	l4h_u64 page_va;        /* L4H_DEV_AREA_VA(index) */
	l4h_u64 page_size;      /* bytes mapped at page_va */
	l4h_u32 event;          /* event bit raised on the target vCPU */
	l4h_u32 kick_slot;      /* index in L4H_SLOT_DEV_CNODE: notification to kick the backend */
	l4h_u64 reserved[2];
};

struct l4h_untyped_desc {
	l4h_u64 size_bits;
};

struct l4h_bootinfo {
	l4h_u64 magic;
	l4h_u64 abi_version;
	l4h_u64 guest_id;
	l4h_u64 ram_base_va;
	l4h_u64 ram_size;
	l4h_u64 kernel_end_gpa;   /* end of the loaded kernel image, page aligned */
	l4h_u64 initrd_gpa;
	l4h_u64 initrd_size;
	l4h_u64 nr_vcpus;
	l4h_u64 tsc_hz;
	l4h_u64 nr_untyped;
	l4h_u64 nr_vspaces;
	l4h_u64 nr_devices;
	l4h_u64 flags;            /* L4H_BOOT_* */
	l4h_u64 reserved[18];
	struct l4h_untyped_desc untyped[L4H_MAX_UNTYPED];
	struct l4h_dev_desc devices[L4H_MAX_DEVICES];
	char cmdline[L4H_CMDLINE_SIZE];
};

#define L4H_BOOT_DOM0           (1ull << 0)

/* ------------------------------------------------------------------------
 * virtio-l4h: the shared device page of an L4H_DEV_VIRTIO device.
 *
 * The guest reads registers directly.  Fields with side effects (status,
 * driver features, queue setup, config writes) are written to the page and
 * then committed with L4H_HC_DEV_WRITE(device index, L4H_VIRTIO_OP_*).
 * Queue notifications: the guest atomically sets bit q in `kick` and then
 * signals the device's kick notification (L4H_SLOT_DEV_CNODE[kick_slot]).
 * Interrupts: the host atomically sets bits in `isr` and raises the
 * device's event line on the vCPU in `irq_vcpu`.
 */
#define L4H_VIRTIO_MAGIC        0x7634686cu /* "lh4v" */
#define L4H_VIRTIO_MAX_QUEUES   16
#define L4H_VIRTIO_CONFIG_OFF   2048
#define L4H_VIRTIO_CONFIG_SIZE  2048

#define L4H_VIRTIO_ISR_VRING    1u
#define L4H_VIRTIO_ISR_CONFIG   2u

struct l4h_virtq_regs {
	l4h_u32 num_max;        /* RO: maximum queue size, 0 if absent */
	l4h_u32 num;            /* RW: queue size chosen by the driver */
	l4h_u32 ready;          /* RW: committed with L4H_VIRTIO_OP_QUEUE */
	l4h_u32 reserved;
	l4h_u64 desc;           /* RW: guest-physical addresses */
	l4h_u64 driver;
	l4h_u64 device;
};

struct l4h_virtio_regs {
	l4h_u32 magic;
	l4h_u32 version;        /* 1 */
	l4h_u32 device_id;      /* VIRTIO_ID_* */
	l4h_u32 vendor_id;
	l4h_u64 device_features;
	l4h_u64 driver_features;
	l4h_u32 status;
	l4h_u32 config_generation;
	l4h_u32 num_queues;
	l4h_u32 isr;
	l4h_u64 kick;
	l4h_u32 irq_vcpu;
	l4h_u32 reserved0;
	l4h_u64 reserved[3];
	struct l4h_virtq_regs queues[L4H_VIRTIO_MAX_QUEUES];
};

enum l4h_virtio_op {
	L4H_VIRTIO_OP_STATUS = 1,       /* status written (0 = reset) */
	L4H_VIRTIO_OP_FEATURES = 2,     /* driver_features written */
	L4H_VIRTIO_OP_QUEUE = 3,        /* MR2 = queue index: ready/num/addresses */
	L4H_VIRTIO_OP_CONFIG = 4,       /* MR2 = offset, MR3 = length written */
	L4H_VIRTIO_OP_IRQ_VCPU = 5,     /* irq_vcpu written */
};

/* ------------------------------------------------------------------------
 * Console device (L4H_DEV_CONSOLE): input ring written by the host.
 * Output goes through L4H_HC_CONSOLE_WRITE.
 */
#define L4H_CONSOLE_IN_SIZE     4032
struct l4h_console_page {
	l4h_u32 in_head;        /* written by the host */
	l4h_u32 in_tail;        /* written by the guest */
	/* Set by the host, which then raises the device's event line: the
	 * guest should shut down cleanly (L4H_POWER_OFF + 1) or reboot
	 * (L4H_POWER_REBOOT + 1).  0 = no request. */
	l4h_u32 power_request;
	l4h_u32 reserved[13];
	l4h_u8 in[L4H_CONSOLE_IN_SIZE];
};

/* ------------------------------------------------------------------------
 * Control device (L4H_DEV_CTL, dom0 only): a request at offset 0 of the
 * device page and the response at L4H_CTL_RESP_OFF, exchanged with
 * L4H_HC_CTL.  Bulk data (images) is passed by guest-physical address of a
 * physically contiguous dom0 buffer.  See DESIGN.md §9.1.
 */
#define L4H_CTL_VERSION         1
#define L4H_CTL_RESP_OFF        2048
#define L4H_CTL_DATA_MAX        1984
#define L4H_CTL_LOAD_MAX        (4u << 20)
#define L4H_CTL_NAME_MAX        32

struct l4h_ctl_req {
	l4h_u32 op;             /* enum l4h_ctl_op */
	l4h_u32 version;        /* L4H_CTL_VERSION */
	l4h_u64 args[6];
	l4h_u8 data[L4H_CTL_DATA_MAX];
};

struct l4h_ctl_resp {
	l4h_s64 status;         /* 0 or -errno */
	l4h_u64 vals[6];
	l4h_u8 data[L4H_CTL_DATA_MAX];
};

enum l4h_ctl_op {
	/* -> vals: cores, free bytes, total bytes, nr disks, nr vms, nr NICs */
	L4H_CTL_HOST_INFO = 1,
	/* args: ram bytes, vcpus, ram_base_va, seL4 object quota bytes (0 =
	 * default); data: name -> vals[0] = vm */
	L4H_CTL_VM_CREATE = 2,
	/* args: vm, dst gpa, src gpa (dom0), len (<= L4H_CTL_LOAD_MAX) */
	L4H_CTL_VM_LOAD = 3,
	/* args: vm, entry, kernel_end gpa, initrd gpa, initrd size; data: cmdline */
	L4H_CTL_VM_START = 4,
	/* args: vm */
	L4H_CTL_VM_DESTROY = 5,
	/* args: first vm -> data: struct l4h_ctl_vm[], vals[0] = count */
	L4H_CTL_VM_LIST = 6,
	/* args: vm, disk index, read-only, first sector, sectors (0 = to the
	 * end of the disk); sectors are 512 bytes */
	L4H_CTL_BLK_ATTACH = 7,
	/* args: vm, offset -> vals[0] = next offset, vals[1] = len,
	 * vals[2] = bytes lost; data: console output */
	L4H_CTL_CONSOLE_READ = 8,
	/* args: vm, len; data: console input */
	L4H_CTL_CONSOLE_WRITE = 9,
	/* args: disk index -> vals: bdf, owner vm (or -1), -, kind (0 virtio-blk,
	 * 1 NVMe) */
	L4H_CTL_DISK_INFO = 10,
	/* -> vals[0] = switch */
	L4H_CTL_SWITCH_CREATE = 11,
	/* args: switch, host NIC index */
	L4H_CTL_SWITCH_ATTACH_UPLINK = 12,
	/* args: vm, switch, MAC (low 48 bits, 0 = generate), flags
	 * (L4H_NIC_*) -> vals[0] = MAC */
	L4H_CTL_NIC_CREATE = 13,
	/* args: switch -> vals[0] = ports, vals[1] = uplink NIC (or -1);
	 * data: struct l4h_ctl_port[] */
	L4H_CTL_SWITCH_INFO = 14,
	/* args: NIC index -> vals: bdf, switch (or -1), MAC */
	L4H_CTL_NIC_INFO = 15,
	/* args: vm, CPU cap per vCPU in percent of a core (0 = none) */
	L4H_CTL_VM_SET_SCHED = 16,
	/* args: vm -> vals[v] = host core of vCPU v, for v < min(6, vCPUs) (the
	 * vCPU count is in L4H_CTL_VM_LIST) */
	L4H_CTL_VM_INFO = 17,
	/* args: vm, L4H_POWER_OFF or L4H_POWER_REBOOT: ask the guest to shut
	 * down or reboot cleanly */
	L4H_CTL_VM_SHUTDOWN = 19,
	/* args: vm -> vals[0] = CPU used in the last 100 ms (permille of a core,
	 * all vCPUs), vals[1] = CPU cap (percent per vCPU, 0 = none) */
	L4H_CTL_VM_STATS = 18,
};

#define L4H_NIC_NO_MAC_LOCK     (1ull << 0) /* allow any source MAC */

struct l4h_ctl_port {
	l4h_u32 vm;
	l4h_u32 port;
	l4h_u64 mac;
	l4h_u64 rx_frames;
	l4h_u64 tx_frames;
	l4h_u64 drops;
};

enum l4h_vm_state {
	L4H_VM_CREATED = 1,
	L4H_VM_RUNNING = 2,
	L4H_VM_STOPPED = 3,
};

struct l4h_ctl_vm {
	l4h_u32 id;
	l4h_u32 state;          /* enum l4h_vm_state */
	l4h_u32 vcpus;
	l4h_u32 reserved;
	l4h_u64 ram;
	char name[L4H_CTL_NAME_MAX];
};

/* ------------------------------------------------------------------------
 * Hypercalls: seL4_Call(L4H_SLOT_HYP_EP) with the label below, arguments
 * in message registers, result in MR0.
 */
enum l4h_hypercall {
	L4H_HC_CONSOLE_WRITE = 1,   /* MR0 = len (<= 8*L4H_HC_CONSOLE_MAX_WORDS), MR1.. = bytes */
	L4H_HC_POWER = 2,           /* MR0 = L4H_POWER_* */
	L4H_HC_DEV_WRITE = 3,       /* MR0 = device index, MR1 = L4H_VIRTIO_OP_*, MR2.. = args */
	L4H_HC_DEV_READ = 4,        /* MR0 = device index, MR1 = register; returns MR0 */
	L4H_HC_SET_IRQ_AFFINITY = 5,/* MR0 = device index, MR1 = vCPU */
	L4H_HC_GET_TIME = 6,        /* returns MR0 = wall clock ns since epoch */
	L4H_HC_CTL = 7,             /* dom0: execute the request in the control device page */
};
#define L4H_HC_CONSOLE_MAX_WORDS 32

enum l4h_power {
	L4H_POWER_OFF = 0,
	L4H_POWER_REBOOT = 1,
	L4H_POWER_CRASH = 2,
};

#endif /* L4H_ABI_H */
