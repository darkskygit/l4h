// SPDX-License-Identifier: GPL-2.0
/*
 * Capability slots, seL4 kernel objects and the RAM of the l4h guest.
 *
 * Guest RAM is a list of 2 MiB blocks (DESIGN.md §15.2).  The host
 * prepares the boot blocks [0, bootinfo.boot_blocks): frames, mapped in the
 * direct map.  For every other block the guest holds a copy of its Untyped
 * (L4H_BLOCK_CPTR) and decides what it is:
 *   - a frame block: 512 frames in the block's level-2 RAM CNode
 *     (L4H_FRAME_CPTR), mapped by us at ram_base_va + GPA and given to the
 *     page allocator;
 *   - an object block: seL4 objects retyped from the Untyped (bump
 *     allocation), and not in the page allocator.
 *
 * Objects come from Untyped *chunks* (16 to 128 KiB, by object size class)
 * carved out of object blocks, one open block per class.  seL4 reuses an
 * Untyped's memory only once all its children are gone, so a chunk is
 * reusable (its next retype resets it) when its live count drops to zero:
 * small chunks keep freed objects reusable even while long-lived ones
 * still pin a part of the block.  A block whose chunks are all empty is
 * free.
 *
 * Slots live in second-level CNodes (radix 12, guard 6) that we create in
 * root slots [L4H_SLOT_DYN_BASE, 4096).  A slot is named by
 * L4H_CPTR2(root, index) and addressed in CNode operations with depth 64.
 */
#include <linux/kernel.h>
#include <linux/init.h>
#include <linux/spinlock.h>
#include <linux/slab.h>
#include <linux/bitmap.h>
#include <linux/list.h>
#include <linux/memblock.h>
#include <linux/mm.h>
#include <linux/seq_file.h>
#include <linux/proc_fs.h>
#include <linux/errno.h>
#include <asm/page.h>
#include <as-layout.h>
#include <l4h_sys.h>
#include <l4h_caps.h>

#define L2_BITS		L4H_ROOT_CNODE_BITS		/* radix of our L2 CNodes */
#define L2_SLOTS	(1u << L2_BITS)
#define L2_GUARD	(L4H_L2_INDEX_BITS - L2_BITS)
#define NR_L2		((1u << L4H_ROOT_CNODE_BITS) - L4H_SLOT_DYN_BASE)
#define SCRATCH_SLOT	(L4H_SLOT_DYN_BASE - 1)	/* root slot for CNode setup */

#define RAM_BLOCK	((unsigned long)L4H_BLOCK_SIZE)
#define RAM_BLOCK_PAGES	(RAM_BLOCK >> PAGE_SHIFT)

static DEFINE_RAW_SPINLOCK(caps_lock);

static unsigned long __slot_alloc(void);
static void __slot_free(unsigned long cptr);

/* Bookkeeping may be needed before the slab allocator is up. */
static void *early_zalloc(size_t size)
{
	/* Early on, bookkeeping is carved from pages taken from memblock:
	 * one memblock allocation per object would exhaust memblock's
	 * static table of reserved regions. */
	static char *pool;
	static size_t left;
	void *p;

	if (slab_is_available())
		return kzalloc(size, GFP_ATOMIC);
	size = ALIGN(size, sizeof(long));
	if (size > left) {
		left = max_t(size_t, size, 16 * PAGE_SIZE);
		pool = memblock_alloc(left, PAGE_SIZE);
		if (!pool) {
			left = 0;
			return NULL;
		}
	}
	p = pool;
	pool += size;
	left -= size;
	return p;
}


/* ---- blocks */

enum block_state {
	BLK_BOOT,	/* frames prepared by the host; never converted */
	BLK_FRAMES,	/* frames retyped by us, in the page allocator */
	BLK_OBJECTS,	/* an object block */
	BLK_BUSY,	/* being made a frame block */
};

struct block {
	u8 state;
	u8 class;		/* chunk class (BLK_OBJECTS) */
	u8 has_l2;		/* its level-2 RAM CNode exists */
	u32 busy;		/* chunks with live objects (BLK_OBJECTS) */
	u32 used;		/* our view of the Untyped's watermark */
	struct list_head chunks;	/* chunks carved from it */
};

/* Size classes: objects up to 2^class_obj_bits[c] bytes come from chunks
 * of 2^class_chunk_bits[c] bytes.  Page tables, TCBs and frames are
 * PAGE; CNodes (slot CNodes, RAM CNodes) are CNODE. */
enum { CLASS_SMALL, CLASS_PAGE, CLASS_CNODE, NR_CLASSES };
static const int class_obj_bits[NR_CLASSES] = { 10, 12, 17 };
static const int class_chunk_bits[NR_CLASSES] = { 14, 16, 17 };

struct chunk {
	struct list_head link;		/* in chunks[class] */
	struct list_head block_link;	/* in its block's list */
	unsigned long cptr;		/* the chunk's Untyped */
	unsigned int block;
	u32 used;			/* our view of its watermark */
	u32 live;			/* objects alive in it */
	u8 class;
	u8 early;			/* bookkeeping from memblock */
};

static struct block *blocks;
static unsigned int nr_blocks, boot_blocks;
static struct list_head chunks[NR_CLASSES];
/* The open object block of each class, -1 if none. */
static int open_block[NR_CLASSES] = { -1, -1, -1 };
static unsigned int nr_object_blocks;

static int obj_class(int bits)
{
	int c;

	for (c = 0; c < NR_CLASSES; c++)
		if (bits <= class_obj_bits[c])
			return c;
	return -1;
}

static inline unsigned long block_cptr(unsigned int b)
{
	return L4H_BLOCK_CPTR(b);
}

static bool is_open(unsigned int b)
{
	int c;

	for (c = 0; c < NR_CLASSES; c++)
		if (open_block[c] == (int)b)
			return true;
	return false;
}

/* A free object block: no chunk has live objects, and it is not open. */
static bool is_free_block(unsigned int b)
{
	return blocks[b].state == BLK_OBJECTS && !blocks[b].busy && !is_open(b);
}

/* Forget the chunks of block `b`, which are all empty: a free block has no
 * chunks, so that nothing is retyped from it until it is taken again.  Its
 * next retype resets it. */
static void __block_drop_chunks(unsigned int b)
{
	struct chunk *ch, *n;

	list_for_each_entry_safe(ch, n, &blocks[b].chunks, block_link) {
		BUG_ON(ch->live);
		list_del(&ch->link);
		list_del(&ch->block_link);
		WARN_ON(l4h_cnode_delete(ch->cptr, 64));
		__slot_free(ch->cptr);
		if (!ch->early)
			kfree(ch);
	}
	blocks[b].used = 0;
}

/* A free object block for chunks of `class`, or -1. */
static int __take_block(int class)
{
	unsigned int i;

	for (i = boot_blocks; i < nr_blocks; i++)
		if (is_free_block(i))
			return i;
	return -1;
}

/* An object block of `class` with room for a chunk. */
static int block_for(int class)
{
	unsigned long size = 1ul << class_chunk_bits[class];
	int b = open_block[class];

	if (b >= 0 && ALIGN(blocks[b].used, size) + size <= RAM_BLOCK)
		return b;
	open_block[class] = -1;
	if (b >= 0 && !blocks[b].busy)
		__block_drop_chunks(b);
	b = __take_block(class);
	if (b < 0)
		return -1;
	blocks[b].used = 0;
	blocks[b].class = class;
	open_block[class] = b;
	return b;
}

/* A chunk of `class` with room for 2^bits bytes. */
static struct chunk *chunk_get(int class, int bits)
{
	int cbits = class_chunk_bits[class];
	struct chunk *ch;
	unsigned long slot;
	int b;

	list_for_each_entry(ch, &chunks[class], link) {
		unsigned long off = ALIGN(ch->used, 1ul << bits);

		if (!ch->live)
			ch->used = off = 0;	/* seL4 resets an Untyped without children */
		if (off + (1ul << bits) <= (1ul << cbits))
			return ch;
	}
	ch = early_zalloc(sizeof(*ch));
	if (!ch)
		return NULL;
	slot = __slot_alloc();
	b = slot ? block_for(class) : -1;
	if (b < 0 || l4h_retype(block_cptr(b), L4H_OBJ_UNTYPED, cbits, slot >> L4H_L2_INDEX_BITS,
				L4H_ROOT_DEPTH, slot & (L2_SLOTS - 1), 1)) {
		if (slot)
			__slot_free(slot);
		if (slab_is_available())
			kfree(ch);
		return NULL;
	}
	blocks[b].used = ALIGN(blocks[b].used, 1ul << cbits) + (1ul << cbits);
	ch->cptr = slot;
	ch->block = b;
	ch->class = class;
	ch->early = !slab_is_available();
	list_add(&ch->link, &chunks[class]);
	list_add(&ch->block_link, &blocks[b].chunks);
	return ch;
}

/* Retype one object of `type` into (node_index, node_depth, offset).
 * Returns its chunk, or NULL if there is no memory for it. */
static struct chunk *__obj_retype(int type, int size_bits, unsigned long node_index,
				  int node_depth, unsigned long offset)
{
	int bits = l4h_obj_bits(type, size_bits);
	int class = obj_class(bits);
	struct chunk *ch;
	int err;

	if (class < 0)
		return NULL;
	ch = chunk_get(class, bits);
	if (!ch)
		return NULL;
	err = l4h_retype(ch->cptr, type, size_bits, node_index, node_depth, offset, 1);
	if (err) {
		pr_err("l4h: retype type %d bits %d failed: %d\n", type, bits, err);
		return NULL;
	}
	ch->used = ALIGN(ch->used, 1ul << bits) + (1ul << bits);
	if (!ch->live++)
		blocks[ch->block].busy++;
	return ch;
}

/* Retype a permanent object, as big as a chunk of its class, straight
 * from an object block into root slot `root_slot`: the block stays busy
 * for good. */
static int __block_retype_permanent(int type, int size_bits, unsigned long root_slot)
{
	int bits = l4h_obj_bits(type, size_bits);
	int class = obj_class(bits);
	int b;

	if (class < 0 || bits != class_chunk_bits[class])
		return -EINVAL;
	b = block_for(class);
	if (b < 0)
		return -ENOMEM;
	if (l4h_retype(block_cptr(b), type, size_bits, 0, 0, root_slot, 1))
		return -ENOMEM;
	blocks[b].used = ALIGN(blocks[b].used, 1ul << bits) + (1ul << bits);
	blocks[b].busy++;
	return 0;
}

static void __obj_put(struct chunk *ch)

{
	unsigned int b = ch->block;

	BUG_ON(!ch->live);
	if (!--ch->live && !--blocks[b].busy && !is_open(b))
		__block_drop_chunks(b);	/* the block is free */
}


/* ---- slots */

struct l2_cnode {
	unsigned long used[BITS_TO_LONGS(L2_SLOTS)];
	unsigned int free;
};

static struct l2_cnode *l2[NR_L2];
static unsigned int nr_l2;

static int l2_add(void)
{
	unsigned int idx = nr_l2;
	unsigned long root = L4H_SLOT_DYN_BASE + idx;
	struct l2_cnode *c;
	int err;

	if (idx >= NR_L2)
		return -ENOSPC;
	c = early_zalloc(sizeof(*c));
	if (!c)
		return -ENOMEM;
	/* The CNode lives as long as the guest.  It is retyped straight from
	 * an object block (a chunk would need a slot, which needs this CNode)
	 * into the scratch root slot, then mutated into place with the guard
	 * that makes it resolve exactly 18 bits. */
	if (__block_retype_permanent(L4H_OBJ_CNODE, L2_BITS, SCRATCH_SLOT)) {
		if (slab_is_available())
			kfree(c);	/* (memblock memory is simply leaked) */
		return -ENOMEM;
	}
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

int l4h_obj_alloc(int type, int size_bits, struct l4h_obj *out)
{
	unsigned long flags, slot;
	struct chunk *ch;
	int err = -ENOMEM;

	if (obj_class(l4h_obj_bits(type, size_bits)) < 0)
		return -EINVAL;
	raw_spin_lock_irqsave(&caps_lock, flags);
	slot = __slot_alloc();
	if (!slot)
		goto out;
	ch = __obj_retype(type, size_bits, slot >> L4H_L2_INDEX_BITS, L4H_ROOT_DEPTH,
			  slot & (L2_SLOTS - 1));
	if (!ch) {
		__slot_free(slot);
		goto out;
	}
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

	if (!o->cptr)
		return;
	WARN_ON(l4h_cnode_delete(o->cptr, 64));
	raw_spin_lock_irqsave(&caps_lock, flags);
	__slot_free(o->cptr);
	__obj_put(o->chunk);
	raw_spin_unlock_irqrestore(&caps_lock, flags);
	o->cptr = 0;
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

/* ---- frame blocks */

/* Add the paging structure of `level` for `va` to the kernel VSpace, and
 * the levels above it as needed.  They are permanent objects. */
static int __add_kernel_table(int level, unsigned long va)
{
	unsigned long slot;
	struct chunk *ch;
	int err;

	if (level >= L4H_NR_PT_LEVELS)
		return -EFAULT;
	slot = __slot_alloc();
	if (!slot)
		return -ENOMEM;
	ch = __obj_retype(l4h_pt_level_obj[level], 0, slot >> L4H_L2_INDEX_BITS, L4H_ROOT_DEPTH,
			  slot & (L2_SLOTS - 1));
	if (!ch) {
		__slot_free(slot);
		return -ENOMEM;
	}
	for (;;) {
		err = l4h_pt_map(l4h_pt_level_obj[level], slot, L4H_CPTR(L4H_SLOT_VSPACE), va);
		if (err != L4H_ERR_FAILED_LOOKUP)
			break;
		err = __add_kernel_table(level + 1, va);
		if (err)
			break;
	}
	if (err) {
		WARN_ON(l4h_cnode_delete(slot, 64));
		__slot_free(slot);
		__obj_put(ch);
		return err == L4H_ERR_DELETE_FIRST ? 0 : err < 0 ? err : -EFAULT;
	}
	return 0;
}

/* Map frame `frame` read-write at `va` in the kernel VSpace. */
static int __map_kernel(unsigned long frame, unsigned long va)
{
	int err;

	for (;;) {
		err = l4h_page_map(frame, L4H_CPTR(L4H_SLOT_VSPACE), va,
				   L4H_PROT_READ | L4H_PROT_WRITE);
		if (err != L4H_ERR_FAILED_LOOKUP)
			return err ? -EFAULT : 0;
		err = __add_kernel_table(0, va);
		if (err)
			return err;
	}
}


/* Make block `b` (whose Untyped has no children) a frame block: its RAM
 * CNode, 512 frames, mapped in the direct map. */
static int __block_to_frames(unsigned int b)
{
	struct block *bl = &blocks[b];
	unsigned long l1 = L4H_SLOT_RAM_L1_BASE + (b >> L4H_RAM_L2_BITS);
	unsigned long gpa = (unsigned long)b << L4H_BLOCK_SHIFT, i;
	int err;

	if (!bl->has_l2) {
		/* The level-2 RAM CNode goes straight into its L1 slot.  It is
		 * kept when the block becomes an object block. */
		if (!__obj_retype(L4H_OBJ_CNODE, L4H_RAM_L2_BITS, l1, L4H_ROOT_DEPTH,
				  b & ((1u << L4H_RAM_L2_BITS) - 1)))
			return -ENOMEM;
		bl->has_l2 = 1;
	}
	err = l4h_retype(block_cptr(b), L4H_OBJ_FRAME, 0, L4H_FRAME_CPTR(gpa) >> L4H_RAM_L2_BITS,
			 L4H_ROOT_DEPTH + L4H_RAM_L2_BITS, 0, RAM_BLOCK_PAGES);
	if (err) {
		pr_err("l4h: block %u: frames: %d\n", b, err);
		return -ENOMEM;
	}
	for (i = 0; i < RAM_BLOCK_PAGES; i++) {
		err = __map_kernel(L4H_FRAME_CPTR(gpa + i * PAGE_SIZE), uml_physmem + gpa + i * PAGE_SIZE);
		if (err) {
			pr_err("l4h: block %u: map: %d\n", b, err);
			return err;
		}
	}
	bl->state = BLK_FRAMES;
	bl->used = RAM_BLOCK;
	return 0;
}

/* ---- boot */

/* Object blocks at boot: 4 MiB plus RAM/32, out of the guest's own RAM.
 * They hold the RAM CNodes and direct-map page tables (1% of RAM), and
 * the processes' page tables and mapping slots. */
static unsigned int __init boot_object_blocks(void)
{
	unsigned long ram = (unsigned long)nr_blocks << L4H_BLOCK_SHIFT;
	unsigned long want = (4ul << 20) + ram / 32;

	return DIV_ROUND_UP(want, RAM_BLOCK);
}

/*
 * Early boot: blocks, the first capability CNode, the user CSpace, and
 * every RAM block, mapped.  memblock only knows the boot blocks so far.
 */
void __init l4h_caps_init(void)
{
	const struct l4h_bootinfo *bi = l4h_bootinfo;
	unsigned int b, want;
	unsigned long flags;
	int err;

	nr_blocks = bi->nr_blocks;
	boot_blocks = bi->boot_blocks;
	if (!nr_blocks || boot_blocks >= nr_blocks)
		panic("l4h: no RAM block for seL4 objects (%u of %u are boot blocks)\n",
		      boot_blocks, nr_blocks);
	blocks = memblock_alloc(nr_blocks * sizeof(*blocks), SMP_CACHE_BYTES);
	if (!blocks)
		panic("l4h: cannot allocate the block table\n");
	for (b = 0; b < NR_CLASSES; b++)
		INIT_LIST_HEAD(&chunks[b]);
	for (b = 0; b < nr_blocks; b++)
		INIT_LIST_HEAD(&blocks[b].chunks);
	for (b = 0; b < boot_blocks; b++) {
		blocks[b].state = BLK_BOOT;
		blocks[b].has_l2 = 1;
	}
	want = min(boot_object_blocks(), nr_blocks - boot_blocks);
	for (b = boot_blocks; b < nr_blocks; b++)
		blocks[b].state = b < boot_blocks + want ? BLK_OBJECTS : BLK_BUSY;
	nr_object_blocks = want;

	raw_spin_lock_irqsave(&caps_lock, flags);
	if (l2_add())
		panic("l4h: cannot create the first capability CNode\n");
	/* The (empty) CSpace of user threads, in the meta CNode so that it
	 * can be named by a CPtr. */
	if (!__obj_retype(L4H_OBJ_CNODE, 1, L4H_SLOT_META, L4H_ROOT_DEPTH, L4H_META_DYN_BASE))
		panic("l4h: cannot create the user CSpace\n");
	for (b = boot_blocks; b < nr_blocks; b++) {
		if (blocks[b].state != BLK_BUSY)
			continue;
		err = __block_to_frames(b);
		if (err)
			panic("l4h: cannot map RAM block %u: %d\n", b, err);
	}
	raw_spin_unlock_irqrestore(&caps_lock, flags);

	/* Tell memblock about RAM: frame blocks are memory, object blocks are
	 * memory that is reserved (they have struct pages, for phase 3). */
	for (b = boot_blocks; b < nr_blocks; b++) {
		phys_addr_t gpa = (phys_addr_t)b << L4H_BLOCK_SHIFT;

		memblock_add(gpa, RAM_BLOCK);
		if (blocks[b].state == BLK_OBJECTS)
			memblock_reserve(gpa, RAM_BLOCK);
	}
	pr_info("l4h: %u MiB of RAM in %u blocks: %u boot, %u object, %u frame\n",
		nr_blocks * (unsigned int)(RAM_BLOCK >> 20), nr_blocks, boot_blocks, nr_object_blocks,
		nr_blocks - boot_blocks - nr_object_blocks);
}

/* /proc/meminfo: guest RAM holding seL4 objects (not in the page
 * allocator). */
void arch_report_meminfo(struct seq_file *m)
{
	unsigned long flags, used = 0;
	unsigned int b, n = 0;

	raw_spin_lock_irqsave(&caps_lock, flags);
	for (b = boot_blocks; b < nr_blocks; b++) {
		struct chunk *ch;

		if (blocks[b].state != BLK_OBJECTS)
			continue;
		n++;
		list_for_each_entry(ch, &blocks[b].chunks, block_link)
			if (ch->live)
				used += 1ul << class_chunk_bits[ch->class];
	}
	raw_spin_unlock_irqrestore(&caps_lock, flags);
	seq_printf(m, "L4hObjects:     %8lu kB\n", (unsigned long)n * (RAM_BLOCK >> 10));
	seq_printf(m, "L4hObjectsUsed: %8lu kB\n", used >> 10);
}
