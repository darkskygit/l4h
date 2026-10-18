#ifndef L4H_ARM64_SYSCALL_H
#define L4H_ARM64_SYSCALL_H
/* SPDX-License-Identifier: GPL-2.0 */
#include <asm/syscall-generic.h>
#include <uapi/linux/audit.h>
typedef asmlinkage long (*sys_call_ptr_t)(unsigned long, unsigned long,
					  unsigned long, unsigned long,
					  unsigned long, unsigned long);
extern const sys_call_ptr_t sys_call_table[];
static inline int syscall_get_arch(struct task_struct *task)
{
	return AUDIT_ARCH_AARCH64;
}

#endif
