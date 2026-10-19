/* SPDX-License-Identifier: GPL-2.0 */
/* 
 * Copyright (C) 2002 - 2007 Jeff Dike (jdike@{addtoit,linux.intel}.com)
 */

#ifndef __UM_MMU_CONTEXT_H
#define __UM_MMU_CONTEXT_H

#include <linux/sched.h>
#include <linux/mm_types.h>
#include <linux/mmap_lock.h>

#include <asm/mm_hooks.h>
#include <asm/mmu.h>

static inline void switch_mm(struct mm_struct *prev, struct mm_struct *next, 
			     struct task_struct *tsk)
{
}

extern void l4h_uctx_mm_gone(struct task_struct *t, struct mm_struct *mm);

/* exec: current stops running user code of `prev`. */
#define activate_mm activate_mm
static inline void activate_mm(struct mm_struct *prev, struct mm_struct *next)
{
	if (prev)
		l4h_uctx_mm_gone(current, prev);
}

/* exit_mm(), or `tsk` is a kernel thread borrowing `mm`. */
#define enter_lazy_tlb enter_lazy_tlb
static inline void enter_lazy_tlb(struct mm_struct *mm, struct task_struct *tsk)
{
	l4h_uctx_mm_gone(tsk, mm);
}

#define init_new_context init_new_context
extern int init_new_context(struct task_struct *task, struct mm_struct *mm);

#define destroy_context destroy_context
extern void destroy_context(struct mm_struct *mm);

#include <asm-generic/mmu_context.h>

#endif
