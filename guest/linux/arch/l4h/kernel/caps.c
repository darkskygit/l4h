// SPDX-License-Identifier: GPL-2.0
/*
 * Capability slots and seL4 kernel objects of the l4h guest.
 *
 * The guest owns an Untyped quota (boot info) from which it creates the
 * kernel objects it needs: TCBs of user threads, page tables of process
 * address spaces, CNodes for capability slots, reply objects, ...
 *
 * Slots live in second-level CNodes (radix 12, guard 6) that we create in
 * root slots [L4H_SLOT_DYN_BASE, 4096).  A slot is named by
 * L4H_CPTR2(root, index) and addressed in CNode operations with depth 64.
 *
 * Objects are carved out of Untyped "chunks" split off the quota, one
 * chunk per size class.  seL4 only reuses an Untyped's memory once all of
 * its children are gone, so a chunk becomes reusable when its live count
 * drops to zero.
 */
#include <linux/kernel.h>
#include <linux/init.h>
#include <linux/spinlock.h>
#include <linux/slab.h>
#include <linux/bitmap.h>
#include <linux/list.h>
#include <linux/memblock.h>
#include <linux/errno.h>
#include <l4h_sys.h>
#include <l4h_caps.h>

#define L2_BITS		L4H_ROOT_CNODE_BITS		/* radix of our L2 CNodes */
#define L2_SLOTS	(1u << L2_BITS)
#define L2_GUARD	(L4H_L2_INDEX_BITS - L2_BITS)
#define NR_L2		((1u << L4H_ROOT_CNODE_BITS) - L4H_SLOT_DYN_BASE)
#define SCRATCH_SLOT	(L4H_SLOT_DYN_BASE - 1)	/* root slot for CNode setup */

static DEFINE_RAW_SPINLOCK(caps_lock);

struct l2_cnode {
	unsigned long used[BITS_TO_LONGS(L2_SLOTS)];
	unsigned int free;
};

/* Bookkeeping may be needed before the slab allocator is up. */
static void *early_zalloc(size_t size)
{
	if (slab_is_available())
		return kzalloc(size, GFP_ATOMIC);
	return memblock_alloc(size, SMP_CACHE_BYTES);
}

static struct l2_cnode *l2[NR_L2];
static unsigned int nr_l2;

/* ---- Untyped quota and chunks */

struct quota_ut {
	unsigned long cptr;
	int size_bits;
	unsigned long used;	/* bytes consumed (our view of the watermark) */
};

static struct quota_ut quota[L4H_MAX_UNTYPED];
static int nr_quota;

struct chunk {
	struct list_head link;
	unsigned long cptr;	/* Untyped capability of the chunk */
	int size_bits;
	unsigned long used;
	unsigned long live;	/* objects alive in this chunk */
	int class;
};

/* Size classes: objects up to 2^class_obj_bits[c] bytes come from chunks of
 * 2^class_chunk_bits[c] bytes. */
enum { CLASS_SMALL, CLASS_PAGE, CLASS_CNODE, NR_CLASSES };
static const int class_obj_bits[NR_CLASSES] = { 10, 12, 17 };
static const int class_chunk_bits[NR_CLASSES] = { 14, 16, 17 };
static struct list_head chunks[NR_CLASSES];

static unsigned long quota_bytes_total, quota_bytes_used;

static int obj_class(int bits)
{
	int c;

	for (c = 0; c < NR_CLASSES; c++)
		if (bits <= class_obj_bits[c])
			return c;
	return -1;
}

/* Retype a new Untyped of 2^bits from the quota into slot `dst`. */
static int quota_split(int bits, unsigned long dst)
{
	int i;

	for (i = 0; i < nr_quota; i++) {
		struct quota_ut *q = &quota[i];
		unsigned long align = 1ul << bits;
		unsigned long off = ALIGN(q->used, align);

		if (bits > q->size_bits || off + align > (1ul << q->size_bits))
			continue;
		if (l4h_retype(q->cptr, L4H_OBJ_UNTYPED, bits, dst >> L4H_L2_INDEX_BITS,
			       L4H_ROOT_DEPTH, dst & (L2_SLOTS - 1), 1))
			continue;
		q->used = off + align;
		quota_bytes_used += align;
		return 0;
	}
	return -ENOMEM;
}

/* ---- slots */

static int l2_add(void)
{
	unsigned int idx = nr_l2;
	unsigned long root = L4H_SLOT_DYN_BASE + idx;
	struct l2_cnode *c;
	int err, i;

	if (idx >= NR_L2)
		return -ENOSPC;
	c = early_zalloc(sizeof(*c));
	if (!c)
		return -ENOMEM;
	/* The CNode itself comes straight from the quota (it is never freed).
	 * Retype it into the scratch root slot, then mutate it into place
	 * with the guard that makes it resolve exactly 18 bits. */
	for (i = 0; i < nr_quota; i++) {
		struct quota_ut *q = &quota[i];
		int bits = L2_BITS + 5;
		unsigned long align = 1ul << bits;
		unsigned long off = ALIGN(q->used, align);

		if (bits > q->size_bits || off + align > (1ul << q->size_bits))
			continue;
		err = l4h_retype(q->cptr, L4H_OBJ_CNODE, L2_BITS, 0, 0, SCRATCH_SLOT, 1);
		if (err)
			continue;
		q->used = off + align;
		quota_bytes_used += align;
		break;
	}
	if (i == nr_quota)
		return -ENOMEM;	/* c is leaked; we are out of memory anyway */
	err = l4h_cnode_mutate(root, L4H_ROOT_DEPTH, SCRATCH_SLOT, L4H_ROOT_DEPTH,
			       /* seL4_CNode_CapData_new(guard 0, guard size) */ (L2_GUARD & 0x3f));
	BUG_ON(err);
	c->free = L2_SLOTS;
	/* Index 0 is never handed out so that cptr 0 stays "no capability". */
	__set_bit(0, c->used);
	c->free--;
	l2[idx] = c;
	nr_l2++;
	return 0;
}

static unsigned long __slot_alloc(void)
{
	unsigned int i;

	for (;;) {
		for (i = nr_l2; i-- > 0;) {
			struct l2_cnode *c = l2[i];
			unsigned long bit;

			if (!c->free)
				continue;
			bit = find_first_zero_bit(c->used, L2_SLOTS);
			BUG_ON(bit >= L2_SLOTS);
			__set_bit(bit, c->used);
			c->free--;
			return L4H_CPTR2(L4H_SLOT_DYN_BASE + i, bit);
		}
		if (l2_add())
			return 0;
	}
}

static void __slot_free(unsigned long cptr)
{
	unsigned int root = cptr >> L4H_L2_INDEX_BITS;
	unsigned int bit = cptr & (L2_SLOTS - 1);
	struct l2_cnode *c;

	BUG_ON(root < L4H_SLOT_DYN_BASE || root - L4H_SLOT_DYN_BASE >= nr_l2);
	c = l2[root - L4H_SLOT_DYN_BASE];
	BUG_ON(!test_bit(bit, c->used));
	__clear_bit(bit, c->used);
	c->free++;
}

unsigned long l4h_slot_alloc(void)
{
	unsigned long flags, s;

	raw_spin_lock_irqsave(&caps_lock, flags);
	s = __slot_alloc();
	raw_spin_unlock_irqrestore(&caps_lock, flags);
	return s;
}

void l4h_slot_free(unsigned long cptr)
{
	unsigned long flags;

	raw_spin_lock_irqsave(&caps_lock, flags);
	__slot_free(cptr);
	raw_spin_unlock_irqrestore(&caps_lock, flags);
}

/* Delete the capability in `cptr` and free the slot. */
void l4h_slot_delete(unsigned long cptr)
{
	int err = l4h_cnode_delete(cptr, 64);

	WARN_ON(err);
	l4h_slot_free(cptr);
}

/* ---- objects */

static struct chunk *chunk_get(int class, int bits)
{
	struct chunk *ch;
	unsigned long slot;

	list_for_each_entry(ch, &chunks[class], link) {
		unsigned long off = ALIGN(ch->used, 1ul << bits);

		if (!ch->live)
			ch->used = off = 0;	/* seL4 resets an Untyped without children */
		if (off + (1ul << bits) <= (1ul << ch->size_bits))
			return ch;
	}
	ch = early_zalloc(sizeof(*ch));
	if (!ch)
		return NULL;
	slot = __slot_alloc();
	if (!slot || quota_split(class_chunk_bits[class], slot)) {
		if (slot)
			__slot_free(slot);
		return NULL;
	}
	ch->cptr = slot;
	ch->size_bits = class_chunk_bits[class];
	ch->class = class;
	list_add(&ch->link, &chunks[class]);
	return ch;
}

int l4h_obj_alloc(int type, int size_bits, struct l4h_obj *out)
{
	int bits = l4h_obj_bits(type, size_bits);
	int class = obj_class(bits);
	unsigned long flags, slot;
	struct chunk *ch;
	int err = -ENOMEM;

	if (class < 0)
		return -EINVAL;
	raw_spin_lock_irqsave(&caps_lock, flags);
	ch = chunk_get(class, bits);
	if (!ch)
		goto out;
	slot = __slot_alloc();
	if (!slot)
		goto out;
	err = l4h_retype(ch->cptr, type, size_bits, slot >> L4H_L2_INDEX_BITS, L4H_ROOT_DEPTH,
			 slot & (L2_SLOTS - 1), 1);
	if (err) {
		__slot_free(slot);
		pr_err("l4h: retype type %d bits %d failed: %d\n", type, bits, err);
		err = -ENOMEM;
		goto out;
	}
	ch->used = ALIGN(ch->used, 1ul << bits) + (1ul << bits);
	ch->live++;
	out->cptr = slot;
	out->chunk = ch;
	err = 0;
out:
	raw_spin_unlock_irqrestore(&caps_lock, flags);
	return err;
}

void l4h_obj_free(struct l4h_obj *o)
{
	unsigned long flags;
	struct chunk *ch = o->chunk;

	if (!o->cptr)
		return;
	WARN_ON(l4h_cnode_delete(o->cptr, 64));
	raw_spin_lock_irqsave(&caps_lock, flags);
	__slot_free(o->cptr);
	ch->live--;
	raw_spin_unlock_irqrestore(&caps_lock, flags);
	o->cptr = 0;
	o->chunk = NULL;
}

/* Copy capability `src` (depth 64 CPtr) into a new slot. */
unsigned long l4h_cap_copy(unsigned long src, int rights)
{
	unsigned long s = l4h_slot_alloc();

	if (!s)
		return 0;
	if (l4h_cnode_copy(s, 64, src, 64, rights)) {
		l4h_slot_free(s);
		return 0;
	}
	return s;
}

/* Mint a badged copy of the capability in root slot `root_slot`. */
unsigned long l4h_cap_mint_root(unsigned long root_slot, int rights, unsigned long badge)
{
	unsigned long s = l4h_slot_alloc();

	if (!s)
		return 0;
	if (l4h_cnode_mint(s, 64, root_slot, L4H_ROOT_DEPTH, rights, badge)) {
		l4h_slot_free(s);
		return 0;
	}
	return s;
}

void l4h_quota_info(unsigned long *total, unsigned long *used)
{
	*total = quota_bytes_total;
	*used = quota_bytes_used;
}

void __init l4h_caps_init(void)
{
	const struct l4h_bootinfo *bi = l4h_bootinfo;
	int i;

	for (i = 0; i < NR_CLASSES; i++)
		INIT_LIST_HEAD(&chunks[i]);
	nr_quota = min_t(int, bi->nr_untyped, L4H_MAX_UNTYPED);
	for (i = 0; i < nr_quota; i++) {
		quota[i].cptr = L4H_CPTR(L4H_SLOT_UNTYPED_BASE + i);
		quota[i].size_bits = bi->untyped[i].size_bits;
		quota_bytes_total += 1ul << quota[i].size_bits;
	}
	if (l2_add())
		panic("l4h: cannot create the first capability CNode\n");

	/* The (empty) CSpace of user threads, in the meta CNode so that it
	 * can be named by a CPtr. */
	for (i = 0; i < nr_quota; i++) {
		int bits = 1 + 5;
		struct quota_ut *q = &quota[i];
		unsigned long off = ALIGN(q->used, 1ul << bits);

		if (off + (1ul << bits) > (1ul << q->size_bits))
			continue;
		if (!l4h_retype(q->cptr, L4H_OBJ_CNODE, 1, L4H_SLOT_META, L4H_ROOT_DEPTH,
				L4H_META_DYN_BASE, 1)) {
			q->used = off + (1ul << bits);
			break;
		}
	}
	if (i == nr_quota)
		panic("l4h: cannot create the user CSpace\n");
	pr_info("l4h: %lu MiB of seL4 object quota\n", quota_bytes_total >> 20);
}
