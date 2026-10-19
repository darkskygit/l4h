/* SPDX-License-Identifier: GPL-2.0 */
#ifndef __L4H_VSPACE_H
#define __L4H_VSPACE_H

struct mm_struct;

void l4h_mm_init(struct mm_struct *mm);
unsigned long l4h_mm_activate(struct mm_struct *mm, u64 *gen);
void l4h_mm_release(struct mm_struct *mm);
int l4h_map_fault(struct mm_struct *mm, unsigned long address);
void l4h_mm_flush_deferred(struct mm_struct *mm);
void l4h_vspace_init(void);

#endif
