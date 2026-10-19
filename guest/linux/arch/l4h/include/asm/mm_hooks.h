/* SPDX-License-Identifier: GPL-2.0 */
#ifndef __L4H_MM_HOOKS_H
#define __L4H_MM_HOOKS_H

#include <linux/types.h>

struct mm_struct;
struct vm_area_struct;

static inline int arch_dup_mmap(struct mm_struct *oldmm, struct mm_struct *mm)
{
	return 0;
}

void arch_exit_mmap(struct mm_struct *mm);

static inline bool arch_vma_access_permitted(struct vm_area_struct *vma,
		bool write, bool execute, bool foreign)
{
	return true;
}

#endif
