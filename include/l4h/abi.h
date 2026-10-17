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
#else
#include <stdint.h>
typedef uint8_t l4h_u8;
typedef uint16_t l4h_u16;
typedef uint32_t l4h_u32;
typedef uint64_t l4h_u64;
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
	l4h_u64 steal_ns;      /* written by the host balancer */
	l4h_u64 reserved[503];
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
 * Hypercalls: seL4_Call(L4H_SLOT_HYP_EP) with the label below, arguments
 * in message registers, result in MR0.
 */
enum l4h_hypercall {
	L4H_HC_CONSOLE_WRITE = 1,   /* MR0 = len (<= 8*L4H_HC_CONSOLE_MAX_WORDS), MR1.. = bytes */
	L4H_HC_POWER = 2,           /* MR0 = L4H_POWER_* */
	L4H_HC_DEV_WRITE = 3,       /* MR0 = device index, MR1 = register, MR2 = value */
	L4H_HC_DEV_READ = 4,        /* MR0 = device index, MR1 = register; returns MR0 */
	L4H_HC_SET_IRQ_AFFINITY = 5,/* MR0 = device index, MR1 = vCPU */
	L4H_HC_GET_TIME = 6,        /* returns MR0 = wall clock ns since epoch */
};
#define L4H_HC_CONSOLE_MAX_WORDS 32

enum l4h_power {
	L4H_POWER_OFF = 0,
	L4H_POWER_REBOOT = 1,
	L4H_POWER_CRASH = 2,
};

#endif /* L4H_ABI_H */
