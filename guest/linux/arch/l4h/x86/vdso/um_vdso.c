// SPDX-License-Identifier: GPL-2.0
/*
 * vDSO time functions for l4h x86_64: the generic implementation over the
 * time page the kernel keeps current (update_vsyscall()) and the TSC, so
 * clock_gettime, gettimeofday, time and clock_getres do not trap.
 *
 * Copyright 2006 Andi Kleen, SUSE Labs.
 * Copyright 2019 ARM Limited
 */
#include <linux/time.h>
#include <linux/kernel.h>
#include <linux/types.h>
#include <vdso/gettime.h>

#include "lib/vdso/gettimeofday.c"

int __vdso_gettimeofday(struct __kernel_old_timeval *tv, struct timezone *tz)
{
	return __cvdso_gettimeofday(tv, tz);
}

int gettimeofday(struct __kernel_old_timeval *, struct timezone *)
	__attribute__((weak, alias("__vdso_gettimeofday")));

__kernel_old_time_t __vdso_time(__kernel_old_time_t *t)
{
	return __cvdso_time(t);
}

__kernel_old_time_t time(__kernel_old_time_t *t) __attribute__((weak, alias("__vdso_time")));

int __vdso_clock_gettime(clockid_t clock, struct __kernel_timespec *ts)
{
	return __cvdso_clock_gettime(clock, ts);
}

int clock_gettime(clockid_t, struct __kernel_timespec *)
	__attribute__((weak, alias("__vdso_clock_gettime")));

int __vdso_clock_getres(clockid_t clock, struct __kernel_timespec *res)
{
	return __cvdso_clock_getres(clock, res);
}

int clock_getres(clockid_t, struct __kernel_timespec *)
	__attribute__((weak, alias("__vdso_clock_getres")));
