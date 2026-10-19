// SPDX-License-Identifier: GPL-2.0
/*
 * Kernel-mode NEON for the l4h aarch64 guest (see asm/neon.h).
 *
 * As on x86_64 (arch/l4h/x86/fpu.c) nothing has to be saved: user FP/SIMD
 * state lives in each user thread's own seL4 TCB.  kernel_neon_begin()
 * only keeps the section on its vCPU, free of preemption and of nested use
 * from softirqs and interrupt handlers, which the guest runs from
 * local_irq_enable(), local_bh_enable(), preempt_enable() and the like.
 * FPCR keeps its reset value: no kernel code outside these sections
 * touches the FP registers.
 */
#include <linux/bottom_half.h>
#include <linux/export.h>
#include <linux/hardirq.h>
#include <linux/irqflags.h>
#include <linux/percpu.h>
#include <linux/preempt.h>
#include <asm/neon.h>

/* False while this vCPU is inside a kernel NEON section. */
static DEFINE_PER_CPU(bool, kernel_neon_allowed) = true;

bool l4h_neon_usable(void)
{
	if (in_nmi())
		return false;
	/* The vCPU is already in a section that this context interrupted. */
	if (!this_cpu_read(kernel_neon_allowed))
		return false;
	if (!in_hardirq())
		return true;
	/* A hardirq is safe if it did not hit a bottom-half-disabled region. */
	return !softirq_count();
}
EXPORT_SYMBOL(l4h_neon_usable);

void kernel_neon_begin(void)
{
	if (!irqs_disabled())
		local_bh_disable();
	else
		preempt_disable();

	WARN_ON_ONCE(!l4h_neon_usable());
	this_cpu_write(kernel_neon_allowed, false);
}
EXPORT_SYMBOL_GPL(kernel_neon_begin);

void kernel_neon_end(void)
{
	WARN_ON_ONCE(this_cpu_read(kernel_neon_allowed));
	this_cpu_write(kernel_neon_allowed, true);

	if (!irqs_disabled())
		local_bh_enable();
	else
		preempt_enable();
}
EXPORT_SYMBOL_GPL(kernel_neon_end);
