// SPDX-License-Identifier: GPL-2.0-only
/* kemu enters through seL4's syscall fault, like native l4h processes. */
#include <linux/kemu.h>
#include <linux/kemu_abi.h>
#include <linux/sched.h>
#include <asm/syscall.h>
#include <asm/unistd.h>

void kemu_svc(struct pt_regs *regs);
void kemu_op(struct pt_regs *regs, unsigned long op);

bool kemu_do_syscall(struct pt_regs *regs, int nr)
{
	if ((unsigned int)nr == KEMU_OP_SVC) {
		kemu_svc(regs);
		syscall_trace_leave(regs);
	} else {
		PT_REGS_SYSCALL_NR(regs) = -1;
		kemu_op(regs, (unsigned int)nr);
	}
	return false;
}

/* Use the native table, including its sys_ni_syscall entries for disabled
 * features, so the guest ABI adapters need no duplicate config guards. */
#define KEMU_SYSCALL(nr, sym) \
	asmlinkage long __x64_##sym(const struct pt_regs *regs); \
	asmlinkage long __x64_##sym(const struct pt_regs *regs) \
	{ \
		return sys_call_table[nr](PT_REGS_DI(regs), PT_REGS_SI(regs), \
			PT_REGS_DX(regs), PT_REGS_R10(regs), PT_REGS_R8(regs), \
			PT_REGS_R9(regs)); \
	}
#include "kemu-syscalls.h"

/* The generic 64-bit ABI uses this name for x86_64's fadvise64. */
KEMU_SYSCALL(__NR_fadvise64, sys_fadvise64_64)
