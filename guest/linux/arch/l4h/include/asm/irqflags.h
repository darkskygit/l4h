/* SPDX-License-Identifier: GPL-2.0 */
#ifndef __UM_IRQFLAGS_H
#define __UM_IRQFLAGS_H

/*
 * Interrupt masking is per vCPU (struct l4h_vcpu.irqs_enabled).  The vCPU
 * structure is found through the vCPU's TLS block: %fs:8 (see
 * os-sel4/l4h_os.h, struct l4h_tls).
 */
#include <linux/compiler.h>
#include <l4h_sys.h>

int um_set_signals(int enable);
void block_signals(void);
void unblock_signals(void);

static __always_inline struct l4h_vcpu *__l4h_vcpu(void)
{
	struct l4h_vcpu *v;

	asm volatile("movq %%fs:8, %0" : "=r"(v));
	return v;
}

#define arch_local_save_flags arch_local_save_flags
static inline unsigned long arch_local_save_flags(void)
{
	return READ_ONCE(__l4h_vcpu()->irqs_enabled);
}

#define arch_local_irq_restore arch_local_irq_restore
static inline void arch_local_irq_restore(unsigned long flags)
{
	um_set_signals(flags);
}

#define arch_local_irq_enable arch_local_irq_enable
static inline void arch_local_irq_enable(void)
{
	unblock_signals();
}

#define arch_local_irq_disable arch_local_irq_disable
static inline void arch_local_irq_disable(void)
{
	WRITE_ONCE(__l4h_vcpu()->irqs_enabled, 0);
	barrier();
}

#define ARCH_IRQ_DISABLED	0

#include <asm-generic/irqflags.h>

#endif
