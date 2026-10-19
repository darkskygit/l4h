/* SPDX-License-Identifier: GPL-2.0 */
#ifndef __ASM_L4H_VDSO_CLOCKSOURCE_H
#define __ASM_L4H_VDSO_CLOCKSOURCE_H

/* The l4h clocksource is the architectural counter, readable in user mode
 * (TSC; CNTVCT_EL0 on aarch64): kernel/time.c. */
#define VDSO_ARCH_CLOCKMODES	\
	VDSO_CLOCKMODE_L4H

#endif
