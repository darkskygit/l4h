// SPDX-License-Identifier: GPL-2.0-only
/*
 * Copyright (C) 2011 Richard Weinberger <richrd@nod.at>
 * Mostly copied from arch/x86/lib/delay.c
 */

#include <linux/export.h>
#include <linux/kernel.h>
#include <linux/delay.h>
#include <asm/param.h>
#include <l4h_sys.h>

/*
 * loops_per_jiffy is in TSC ticks (lpj_fine, kernel/time.c), so wait for
 * the TSC to advance by `loops`, as x86's delay_tsc(): a loop of
 * instructions runs at the core clock, which is not the TSC's.
 */
void __delay(unsigned long loops)
{
	unsigned long long start = l4h_rdtsc();

	while (l4h_rdtsc() - start < loops)
		cpu_relax();
}
EXPORT_SYMBOL(__delay);

inline void __const_udelay(unsigned long xloops)
{
	int d0;

	xloops *= 4;
	asm("mull %%edx"
		: "=d" (xloops), "=&a" (d0)
		: "1" (xloops), "0"
		(loops_per_jiffy * (HZ/4)));

	__delay(++xloops);
}
EXPORT_SYMBOL(__const_udelay);

void __udelay(unsigned long usecs)
{
	__const_udelay(usecs * 0x000010c7); /* 2**32 / 1000000 (rounded up) */
}
EXPORT_SYMBOL(__udelay);

void __ndelay(unsigned long nsecs)
{
	__const_udelay(nsecs * 0x00005); /* 2**32 / 1000000000 (rounded up) */
}
EXPORT_SYMBOL(__ndelay);
