/* SPDX-License-Identifier: GPL-2.0 */
#ifndef __ASM_L4H_VDSO_PROCESSOR_H
#define __ASM_L4H_VDSO_PROCESSOR_H

/* The kernel has its own cpu_relax() (asm/processor.h); this one is for
 * the vDSO. */
#if !defined(__ASSEMBLER__) && defined(BUILD_VDSO)

static __always_inline void cpu_relax(void)
{
#ifdef CONFIG_UML_ARM64
	asm volatile("yield" ::: "memory");
#else
	asm volatile("pause" ::: "memory");
#endif
}

#endif

#endif
