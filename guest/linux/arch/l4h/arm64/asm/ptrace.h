/* SPDX-License-Identifier: GPL-2.0 */
#ifndef L4H_ARM64_ASM_PTRACE_H
#define L4H_ARM64_ASM_PTRACE_H
#include "../../../arm64/include/uapi/asm/ptrace.h"
#include <asm/ptrace-generic.h>
#include <asm/errno.h>
#define user_mode(r) UPT_IS_USER(&(r)->regs)
#define PT_REGS_AX(r) ((r)->regs.gp[0])
#define PT_REGS_BP(r) ((r)->regs.gp[29])
#define PT_REGS_SYSCALL_RET(r) PT_REGS_AX(r)
#define PT_REGS_ORIG_SYSCALL(r) ((r)->regs.gp[8])
#define PT_REGS_SET_SYSCALL_RETURN(r, v) (PT_REGS_AX(r) = (v))
/* Restore x0 as well as x8 before re-executing SVC. */
#define UPT_RESTART_SYSCALL(r)            \
	do {                              \
		(r)->gp[0] = (r)->gp[36]; \
		UPT_IP(r) -= 4;           \
	} while (0)
#define PT_FIX_EXEC_STACK(sp) \
	do {                  \
	} while (0)
#define profile_pc(r) PT_REGS_IP(r)
#define user_stack_pointer(r) PT_REGS_SP(r)
static inline long regs_return_value(struct pt_regs *r)
{
	return PT_REGS_AX(r);
}
struct task_struct;
void arch_switch_to(struct task_struct *to);
#define PTRACE_SYSEMU 31
#define PTRACE_SYSEMU_SINGLESTEP 32
#define PTRACE_GET_THREAD_AREA 25
#define PTRACE_SET_THREAD_AREA 26
struct user_desc;
static inline int ptrace_get_thread_area(struct task_struct *t, int idx,
					 struct user_desc __user *d)
{
	return -EIO;
}
static inline int ptrace_set_thread_area(struct task_struct *t, int idx,
					 struct user_desc __user *d)
{
	return -EIO;
}
#endif
