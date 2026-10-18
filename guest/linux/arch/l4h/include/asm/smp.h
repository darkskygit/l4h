/* SPDX-License-Identifier: GPL-2.0 */
#ifndef __L4H_SMP_H
#define __L4H_SMP_H

#ifdef CONFIG_SMP

#include <linux/cpumask.h>

/* vCPU number from the vCPU's TLS block (struct l4h_tls.cpu, %fs:16). */
#ifdef CONFIG_UML_ARM64
#define raw_smp_processor_id() ({ \
	long __cpu; \
	asm volatile("mrs %0, tpidr_el0\n\tldr %0, [%0, #48]" : "=r"(__cpu)); \
	(int)__cpu; \
})
#else
#define raw_smp_processor_id() ({				\
	long __cpu;						\
	asm volatile("movq %%fs:16, %0" : "=r"(__cpu));		\
	(int)__cpu;						\
})

#endif

#define hard_smp_processor_id()	raw_smp_processor_id()

void arch_smp_send_reschedule(int cpu);
void arch_send_call_function_single_ipi(int cpu);
void arch_send_call_function_ipi_mask(const struct cpumask *mask);

#else

#define hard_smp_processor_id()	0

#endif

#endif
