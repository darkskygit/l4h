/* SPDX-License-Identifier: GPL-2.0 */
#ifndef __ARCH_L4H_MMU_H
#define __ARCH_L4H_MMU_H

#include <linux/types.h>
#include <linux/spinlock_types.h>
#include <mm_id.h>

/* seL4 side of an mm: which process VSpace it currently owns. */
struct l4h_mm {
	raw_spinlock_t lock;
	int vs;			/* VSpace pool index, -1 if none */
	u64 gen;		/* changes whenever vs is (re)assigned */
	atomic_t running;	/* threads that may be executing user code */
	/* A thread moved to another vCPU since the VSpace was last flushed:
	 * the old vCPU's core may be left in its TLB bitmap (vspace.c). */
	bool moved;
};

typedef struct mm_context {
	struct mm_id id;
	struct l4h_mm l4h;
	/*
	 * Held by um_tlb_sync() from taking the range until it has been
	 * applied, so that a flush waits for a sync already under way on
	 * another vCPU, and two syncs never race on the same PTE.
	 */
	raw_spinlock_t sync_walk_lock;
	raw_spinlock_t sync_lock;	/* protects the range below */
	/* Address range in need of a TLB sync */
	unsigned long sync_tlb_range_from;
	unsigned long sync_tlb_range_to;
} mm_context_t;

#define INIT_MM_CONTEXT(mm)						\
	.context = {							\
		.sync_walk_lock = __RAW_SPIN_LOCK_UNLOCKED(mm.context.sync_walk_lock), \
		.sync_lock = __RAW_SPIN_LOCK_UNLOCKED(mm.context.sync_lock), \
		.l4h = {						\
			.lock = __RAW_SPIN_LOCK_UNLOCKED(mm.context.l4h.lock), \
			.vs = -1,					\
		},							\
	},

#endif
