// SPDX-License-Identifier: GPL-2.0
/*
 * Copyright (C) 2000 - 2007 Jeff Dike (jdike@{addtoit,linux.intel}.com)
 */

#include <linux/module.h>
#include <linux/ptrace.h>
#include <linux/sched.h>
#include <linux/ftrace.h>
#include <asm/siginfo.h>
#include <asm/signal.h>
#include <asm/unistd.h>
#include <frame_kern.h>
#include <kern_util.h>
#include <os.h>

EXPORT_SYMBOL(block_signals);
EXPORT_SYMBOL(unblock_signals);

void block_signals_trace(void)
{
	block_signals();
	if (current_thread_info())
		trace_hardirqs_off();
}

void unblock_signals_trace(void)
{
	if (current_thread_info())
		trace_hardirqs_on();
	unblock_signals();
}

void um_trace_signals_on(void)
{
	if (current_thread_info())
		trace_hardirqs_on();
}

void um_trace_signals_off(void)
{
	if (current_thread_info())
		trace_hardirqs_off();
}

/*
 * OK, we're invoking a handler
 */
static void handle_signal(struct ksignal *ksig, struct pt_regs *regs)
{
	sigset_t *oldset = sigmask_to_save();
	int singlestep = 0;
	unsigned long sp;
	int err;

	if (test_thread_flag(TIF_SINGLESTEP) && (current->ptrace & PT_PTRACED))
		singlestep = 1;

	/* Did we come from a system call?  As syscall_get_nr(), only the low
	 * 32 bits count: they are what was dispatched.  aarch64's do_signal()
	 * prepared the restart already, and a tracer at the signal stop may
	 * have set a syscall number since. */
	if (!IS_ENABLED(CONFIG_UML_ARM64) && (int)PT_REGS_SYSCALL_NR(regs) >= 0) {
		/* If so, check system call restarting.. */
		switch (PT_REGS_SYSCALL_RET(regs)) {
		case -ERESTART_RESTARTBLOCK:
		case -ERESTARTNOHAND:
			PT_REGS_SYSCALL_RET(regs) = -EINTR;
			break;

		case -ERESTARTSYS:
			if (!(ksig->ka.sa.sa_flags & SA_RESTART)) {
				PT_REGS_SYSCALL_RET(regs) = -EINTR;
				break;
			}
			fallthrough;
		case -ERESTARTNOINTR:
			PT_REGS_RESTART_SYSCALL(regs);
			PT_REGS_ORIG_SYSCALL(regs) = PT_REGS_SYSCALL_NR(regs);
			break;
		}
	}

	sp = PT_REGS_SP(regs);
	if ((ksig->ka.sa.sa_flags & SA_ONSTACK) && (sas_ss_flags(sp) == 0))
		sp = current->sas_ss_sp + current->sas_ss_size;

#ifdef CONFIG_ARCH_HAS_SC_SIGNALS
	if (!(ksig->ka.sa.sa_flags & SA_SIGINFO))
		err = setup_signal_stack_sc(sp, ksig, regs, oldset);
	else
#endif
		err = setup_signal_stack_si(sp, ksig, regs, oldset);

	signal_setup_done(err, ksig, singlestep);
}

#ifdef CONFIG_UML_ARM64
/*
 * As arm64: a syscall's restart is prepared before get_signal(), so that
 * a tracer at the signal stop sees the PC rewound, x0 restored and no
 * syscall (NT_ARM_SYSTEM_CALL reads -1, and register writes leave the
 * original x0 alone), and it is undone only if the PC is still the
 * restart address.  One signal per call: interrupt_end() calls again.
 */
void do_signal(struct pt_regs *regs)
{
	unsigned long continue_addr = 0, restart_addr = 0;
	bool syscall = PT_REGS_SYSCALL_NR(regs) >= 0;
	struct ksignal ksig;
	long retval = 0;

	if (syscall) {
		continue_addr = PT_REGS_IP(regs);
		restart_addr = continue_addr - 4;
		retval = PT_REGS_SYSCALL_RET(regs);
		PT_REGS_SYSCALL_NR(regs) = -1;
		switch (retval) {
		case -ERESTARTNOHAND:
		case -ERESTARTSYS:
		case -ERESTARTNOINTR:
		case -ERESTART_RESTARTBLOCK:
			PT_REGS_RESTART_SYSCALL(regs);
			break;
		}
	}

	if (get_signal(&ksig)) {
		if (PT_REGS_IP(regs) == restart_addr &&
		    (retval == -ERESTARTNOHAND ||
		     retval == -ERESTART_RESTARTBLOCK ||
		     (retval == -ERESTARTSYS &&
		      !(ksig.ka.sa.sa_flags & SA_RESTART)))) {
			PT_REGS_SYSCALL_RET(regs) = -EINTR;
			PT_REGS_IP(regs) = continue_addr;
		}
		handle_signal(&ksig, regs);
		return;
	}

	if (syscall && PT_REGS_IP(regs) == restart_addr &&
	    retval == -ERESTART_RESTARTBLOCK)
		PT_REGS_ORIG_SYSCALL(regs) = __NR_restart_syscall;
	restore_saved_sigmask();
}
#else
void do_signal(struct pt_regs *regs)
{
	struct ksignal ksig;
	int handled_sig = 0;

	while (get_signal(&ksig)) {
		handled_sig = 1;
		/* Whee!  Actually deliver the signal.  */
		handle_signal(&ksig, regs);
	}

	/* Did we come from a system call? */
	if (!handled_sig && ((int)PT_REGS_SYSCALL_NR(regs) >= 0)) {
		/* Restart the system call - no handlers present */
		switch (PT_REGS_SYSCALL_RET(regs)) {
		case -ERESTARTNOHAND:
		case -ERESTARTSYS:
		case -ERESTARTNOINTR:
			PT_REGS_ORIG_SYSCALL(regs) = PT_REGS_SYSCALL_NR(regs);
			PT_REGS_RESTART_SYSCALL(regs);
			break;
		case -ERESTART_RESTARTBLOCK:
			PT_REGS_ORIG_SYSCALL(regs) = __NR_restart_syscall;
			PT_REGS_RESTART_SYSCALL(regs);
			break;
		}
	}

	/*
	 * if there's no signal to deliver, we just put the saved sigmask
	 * back
	 */
	if (!handled_sig)
		restore_saved_sigmask();
}
#endif
