/* SPDX-License-Identifier: GPL-2.0 */
#ifndef L4H_ARM64_NEON_H
#define L4H_ARM64_NEON_H

#include <linux/types.h>
#include <asm/cpufeature.h>

/*
 * Kernel-mode FP/ASIMD on l4h aarch64, as on x86_64 (asm/fpu/api.h):
 * every user thread has its own seL4 TCB and seL4 keeps each TCB's
 * V0-V31, FPSR and FPCR, so the registers the guest kernel's vCPU thread
 * sees never hold user state.  The kernel is built with
 * -mgeneral-regs-only, so the only users are kernel_neon_begin/end
 * sections, which must not be preempted, migrated or entered again from
 * softirq or hardirq context.  See arch/l4h/arm64/neon.c.
 */
#define cpu_has_neon()		true

void kernel_neon_begin(void);
void kernel_neon_end(void);
bool l4h_neon_usable(void);

#endif
