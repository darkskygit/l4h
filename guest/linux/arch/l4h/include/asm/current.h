/* SPDX-License-Identifier: GPL-2.0 */
#ifndef __ASM_CURRENT_H
#define __ASM_CURRENT_H

#include <linux/compiler.h>
#include <linux/threads.h>

#ifndef __ASSEMBLER__

struct task_struct;
extern struct task_struct *cpu_tasks[NR_CPUS];

/* The current task of each vCPU is kept in the vCPU's TLS block
 * (struct l4h_tls.current at %fs:24, see os-sel4/l4h_os.h). */
static __always_inline struct task_struct *get_current(void)
{
	struct task_struct *t;

	asm volatile("movq %%fs:24, %0" : "=r"(t));
	return t;
}

static __always_inline void l4h_set_current(struct task_struct *t)
{
	asm volatile("movq %0, %%fs:24" : : "r"(t) : "memory");
}

#define current get_current()

#endif /* __ASSEMBLER__ */

#endif /* __ASM_CURRENT_H */
