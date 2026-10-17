// SPDX-License-Identifier: GPL-2.0
/*
 * Process address spaces of the l4h guest: an mm gets an seL4 VSpace
 * lazily, when one of its threads first runs (see kernel/vspace.c).
 */
#include <linux/mm.h>
#include <linux/sched/signal.h>
#include <asm/mmu_context.h>
#include <l4h_vspace.h>

int init_new_context(struct task_struct *task, struct mm_struct *mm)
{
	l4h_mm_init(mm);
	return 0;
}

void destroy_context(struct mm_struct *mm)
{
	l4h_mm_release(mm);
}
