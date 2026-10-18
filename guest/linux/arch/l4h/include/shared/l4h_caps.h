/* SPDX-License-Identifier: GPL-2.0 */
/* Capability slots, seL4 objects and RAM blocks of the l4h guest kernel. */
#ifndef __L4H_CAPS_H
#define __L4H_CAPS_H

struct l4h_obj {
	unsigned long cptr;	/* 0 if none */
	void *chunk;		/* the Untyped chunk it was retyped from */
};

unsigned long l4h_slot_alloc(void);
void l4h_slot_free(unsigned long cptr);
void l4h_slot_delete(unsigned long cptr);
int l4h_obj_alloc(int type, int size_bits, struct l4h_obj *out);
void l4h_obj_free(struct l4h_obj *o);
unsigned long l4h_cap_copy(unsigned long src, int rights);
unsigned long l4h_cap_mint_root(unsigned long root_slot, int rights, unsigned long badge);
void l4h_caps_init(void);

#endif
