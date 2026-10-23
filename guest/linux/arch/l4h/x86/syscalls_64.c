/*
 * Copyright (C) 2003 - 2007 Jeff Dike (jdike@{addtoit,linux.intel}.com)
 * Copyright 2003 PathScale, Inc.
 *
 * Licensed under the GPL
 */

#include <linux/sched.h>
#include <linux/sched/mm.h>
#include <linux/syscalls.h>
#include <linux/uaccess.h>
#include <asm/prctl.h> /* XXX This should get the constants from libc */
#include <registers.h>
#include <os.h>
#include <l4h_uctx.h>

long arch_prctl(struct task_struct *task, int option,
		unsigned long __user *arg2)
{
	unsigned long *gp = task->thread.regs.regs.gp;
	long ret = -EINVAL;

	/* task is current, or (PTRACE_ARCH_PRCTL) a tracee stopped by ptrace. */
	l4h_sync_seg(task);
	switch (option) {
	case ARCH_SET_FS:
	case ARCH_SET_GS:
		/* As on x86; seL4 would make a non-canonical base 0. */
		if ((unsigned long) arg2 >= TASK_SIZE_MAX)
			return -EPERM;
		break;
	}
	switch (option) {
	case ARCH_SET_FS:
		gp[FS_BASE / sizeof(unsigned long)] = (unsigned long) arg2;
		l4h_seg_dirty(task);
		ret = 0;
		break;
	case ARCH_SET_GS:
		gp[GS_BASE / sizeof(unsigned long)] = (unsigned long) arg2;
		l4h_seg_dirty(task);
		ret = 0;
		break;
	case ARCH_GET_FS:
		ret = put_user(gp[FS_BASE / sizeof(unsigned long)], arg2);
		break;
	case ARCH_GET_GS:
		ret = put_user(gp[GS_BASE / sizeof(unsigned long)], arg2);
		break;
	}

	return ret;
}

SYSCALL_DEFINE2(arch_prctl, int, option, unsigned long, arg2)
{
	return arch_prctl(current, option, (unsigned long __user *) arg2);
}

void arch_switch_to(struct task_struct *to)
{
	/*
	 * Nothing needs to be done on x86_64.
	 * The FS_BASE/GS_BASE registers are saved in the ptrace register set.
	 */
}

SYSCALL_DEFINE6(mmap, unsigned long, addr, unsigned long, len,
		unsigned long, prot, unsigned long, flags,
		unsigned long, fd, unsigned long, off)
{
	if (off & ~PAGE_MASK)
		return -EINVAL;

	return ksys_mmap_pgoff(addr, len, prot, flags, fd, off >> PAGE_SHIFT);
}
