/* SPDX-License-Identifier: GPL-2.0 */
/*
 * Generic vDSO time functions for l4h (lib/vdso/gettimeofday.c): the
 * counter is read in user mode, and only clocks the time page cannot
 * serve fall back to a syscall.
 */
#ifndef __ASM_L4H_VDSO_GETTIMEOFDAY_H
#define __ASM_L4H_VDSO_GETTIMEOFDAY_H

#ifndef __ASSEMBLER__

#include <uapi/linux/time.h>
#include <asm/unistd.h>

#define VDSO_HAS_TIME 1

#define VDSO_HAS_CLOCK_GETRES 1

#ifdef CONFIG_UML_ARM64
#error "the l4h aarch64 vDSO has no time functions yet"
#endif

static __always_inline
long clock_gettime_fallback(clockid_t _clkid, struct __kernel_timespec *_ts)
{
	long ret;

	asm ("syscall" : "=a" (ret), "=m" (*_ts) :
	     "0" (__NR_clock_gettime), "D" (_clkid), "S" (_ts) :
	     "rcx", "r11");

	return ret;
}

static __always_inline
long gettimeofday_fallback(struct __kernel_old_timeval *_tv,
			   struct timezone *_tz)
{
	long ret;

	asm("syscall" : "=a" (ret) :
	    "0" (__NR_gettimeofday), "D" (_tv), "S" (_tz) : "rcx", "r11", "memory");

	return ret;
}

static __always_inline
long clock_getres_fallback(clockid_t _clkid, struct __kernel_timespec *_ts)
{
	long ret;

	asm ("syscall" : "=a" (ret), "=m" (*_ts) :
	     "0" (__NR_clock_getres), "D" (_clkid), "S" (_ts) :
	     "rcx", "r11");

	return ret;
}

static __always_inline u64 __arch_get_hw_counter(s32 clock_mode,
						 const struct vdso_time_data *vd)
{
	u32 lo, hi;

	if (clock_mode != VDSO_CLOCKMODE_L4H)
		return U64_MAX;
	/* Ordered like the kernel's rdtsc_ordered(): not before earlier loads. */
	asm volatile("lfence; rdtsc" : "=a" (lo), "=d" (hi) :: "memory");
	return ((u64)hi << 32) | lo;
}

#endif /* !__ASSEMBLER__ */

#endif
