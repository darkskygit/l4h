/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _ASM_L4H_PARAVIRT_H
#define _ASM_L4H_PARAVIRT_H

/* Steal time from the l4h host (event page steal_ns). */
#ifdef CONFIG_PARAVIRT
struct static_key;
extern struct static_key paravirt_steal_enabled;
extern struct static_key paravirt_steal_rq_enabled;

unsigned long long l4h_steal_ns(int cpu);

static inline u64 paravirt_steal_clock(int cpu)
{
	return l4h_steal_ns(cpu);
}
#endif

#endif
