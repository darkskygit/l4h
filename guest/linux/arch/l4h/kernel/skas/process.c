// SPDX-License-Identifier: GPL-2.0
/*
 * Copyright (C) 2002 - 2007 Jeff Dike (jdike@{addtoit,linux.intel}.com)
 */

#include <linux/init.h>
#include <linux/sched/mm.h>
#include <linux/sched/task_stack.h>
#include <linux/sched/task.h>

#include <asm/tlbflush.h>

#include <as-layout.h>
#include <kern.h>
#include <os.h>
#include <skas.h>
#include <kern_util.h>

extern void start_kernel(void);

static int __init start_kernel_proc(void *unused)
{
	block_signals_trace();

	start_kernel();
	return 0;
}

int __init start_uml(void)
{
	init_task.thread.request.thread.proc = start_kernel_proc;
	init_task.thread.request.thread.arg = NULL;
	return start_idle_thread(task_stack_page(&init_task),
				 &init_task.thread.switch_buf);
}

struct mm_id *current_mm_id(void)
{
	if (current->mm == NULL)
		return NULL;

	return &current->mm->context.id;
}

void current_mm_sync(void)
{
	if (current->mm == NULL)
		return;

	um_tlb_sync(current->mm);
}
