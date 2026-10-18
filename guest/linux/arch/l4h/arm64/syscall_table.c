// SPDX-License-Identifier: GPL-2.0
#include <linux/linkage.h>
#include <linux/cache.h>
#include <asm/syscall.h>
#include <asm/unistd.h>
/* arm64 uses the common 64-bit syscall ABI, with no compat ABI. */
#undef __SYSCALL
#define __SYSCALL(nr, sym)                                       \
	extern asmlinkage long sym(unsigned long, unsigned long, \
				   unsigned long, unsigned long, \
				   unsigned long, unsigned long);
#define __SYSCALL_WITH_COMPAT(nr, native, compat) __SYSCALL(nr, native)
#include <asm/syscall_table_64.h>
#undef __SYSCALL
#define __SYSCALL(nr, sym) [nr] = sym,
const sys_call_ptr_t sys_call_table[__NR_syscalls] ____cacheline_aligned = {
	[0 ... __NR_syscalls - 1] = (sys_call_ptr_t)sys_ni_syscall,
#include <asm/syscall_table_64.h>
};
int syscall_table_size = sizeof(sys_call_table);
