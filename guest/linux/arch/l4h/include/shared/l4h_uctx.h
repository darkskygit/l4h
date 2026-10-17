/* SPDX-License-Identifier: GPL-2.0 */
/* Kernel-side helpers around a thread's seL4 user context (kernel/uctx.c). */
#ifndef __L4H_UCTX_H
#define __L4H_UCTX_H

struct task_struct;

extern unsigned long host_fp_size;

void l4h_fpu_fetch(struct task_struct *t);
void l4h_fpu_dirty(struct task_struct *t);
void l4h_sync_regs(struct task_struct *t);
void l4h_sync_seg(struct task_struct *t);
void l4h_seg_dirty(struct task_struct *t);
void l4h_uctx_switch_out(struct task_struct *t);
void l4h_uctx_destroy(struct task_struct *t);
void l4h_uctx_init(void);

#endif
