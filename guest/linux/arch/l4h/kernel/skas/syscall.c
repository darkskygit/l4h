// SPDX-License-Identifier: GPL-2.0
/*
 * Copyright (C) 2002 - 2007 Jeff Dike (jdike@{addtoit,linux.intel}.com)
 */

#include <linux/kernel.h>
#include <linux/init.h>
#include <linux/sched.h>
#include <linux/ptrace.h>
#include <linux/seccomp.h>
#include <linux/kemu.h>
#include <kern_util.h>
#include <sysdep/ptrace.h>
#include <sysdep/ptrace_user.h>
#include <asm/syscall.h>
#include <asm/unistd.h>
#include <asm/delay.h>

static int l4h_strace;
static int __init l4h_strace_setup(char *s)
{
	l4h_strace = 1;
	return 0;
}
early_param("l4h_strace", l4h_strace_setup);

static int __init l4h_debug_user_setup(char *s)
{
	extern int l4h_debug_user;

	l4h_debug_user = 1;
	return 0;
}
early_param("l4h_debug_user", l4h_debug_user_setup);

void handle_syscall(struct uml_pt_regs *r)
{
	struct pt_regs *regs = container_of(r, struct pt_regs, regs);
	int syscall;

	/* Initialize the syscall number and default return value.  arm64
	 * takes the number from w8, so the restart checks see the number
	 * that is dispatched (x86 keeps all of orig_ax, as on x86, and its
	 * restart checks use the low 32 bits). */
	if (IS_ENABLED(CONFIG_UML_ARM64))
		UPT_SYSCALL_NR(r) = (int)PT_SYSCALL_NR(r->gp);
	else
		UPT_SYSCALL_NR(r) = PT_SYSCALL_NR(r->gp);
#ifdef CONFIG_KEMU
	if (task_is_kemu(current)) {
		kemu_do_syscall(regs, UPT_SYSCALL_NR(r));
		return;
	}
#endif
	/* ARM x0 is also the first argument, visible at a ptrace entry stop. */
	if (!IS_ENABLED(CONFIG_UML_ARM64) || UPT_SYSCALL_NR(r) == -1)
		PT_REGS_SET_SYSCALL_RETURN(regs, -ENOSYS);

	if (syscall_trace_enter(regs))
		goto out;

	/* Do the seccomp check after ptrace; failures should be fast. */
	if (secure_computing() == -1)
		goto out;

	syscall = UPT_SYSCALL_NR(r);

	if (syscall >= 0 && syscall < __NR_syscalls) {
		unsigned long ret;

		ret = (*sys_call_table[syscall])(UPT_SYSCALL_ARG1(&regs->regs),
						 UPT_SYSCALL_ARG2(&regs->regs),
						 UPT_SYSCALL_ARG3(&regs->regs),
						 UPT_SYSCALL_ARG4(&regs->regs),
						 UPT_SYSCALL_ARG5(&regs->regs),
						 UPT_SYSCALL_ARG6(&regs->regs));

		PT_REGS_SET_SYSCALL_RETURN(regs, ret);
		if (l4h_strace)
			pr_info("[%d] syscall %d(%lx, %lx, %lx, %lx) = %lx  ip=%lx sp=%lx\n",
				task_pid_nr(current), syscall,
				UPT_SYSCALL_ARG1(&regs->regs), UPT_SYSCALL_ARG2(&regs->regs),
				UPT_SYSCALL_ARG3(&regs->regs), UPT_SYSCALL_ARG4(&regs->regs), ret,
				UPT_IP(&regs->regs), UPT_SP(&regs->regs));

#ifdef CONFIG_UML_ARM64
	} else if (syscall != -1) {
		PT_REGS_SET_SYSCALL_RETURN(regs, -ENOSYS);
#endif
	}

out:
	syscall_trace_leave(regs);
}
