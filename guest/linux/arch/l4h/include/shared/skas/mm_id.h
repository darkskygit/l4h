/* SPDX-License-Identifier: GPL-2.0 */
#ifndef __MM_ID_H
#define __MM_ID_H

/* Kept for the UML code that passes mm ids around; all state is in
 * struct l4h_mm (asm/mmu.h). */
struct mm_id {
	int unused;
};

#endif
