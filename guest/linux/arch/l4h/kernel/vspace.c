// SPDX-License-Identifier: GPL-2.0
/*
 * seL4 address spaces of the l4h guest.
 *
 * The kernel keeps ordinary Linux page tables; the seL4 VSpaces are caches
 * of them, filled by the UML TLB-sync machinery (tlb.c, through map() and
 * unmap()) and by the page fault path (l4h_map_fault()).
 *
 * Process VSpaces come from a fixed pool of pre-assigned VSpace roots
 * (seL4 ASIDs are scarce, see DESIGN.md).  An mm gets a pool entry when one
 * of its threads first runs; when the pool is exhausted the least recently
 * activated mm without running threads loses its entry, and simply refaults
 * its pages when it runs again.
 *
 * Every mapping needs its own frame capability: the slot is recorded in a
 * per-address-space xarray (virtual page -> slot | pfn) so that it can be
 * deleted again on unmap.
 */
#include <linux/kernel.h>
#include <linux/mm.h>
#include <linux/sched.h>
#include <linux/spinlock.h>
#include <linux/xarray.h>
#include <linux/memblock.h>
#include <linux/slab.h>
#include <linux/list.h>
#include <linux/export.h>
#include <asm/tlbflush.h>
#include <asm/pgtable.h>
#include <as-layout.h>
#include <os.h>
#include <l4h_sys.h>
#include <l4h_caps.h>
#include <l4h_vspace.h>

#define SLOT_BITS	30
#define SLOT_MASK	((1ul << SLOT_BITS) - 1)

static inline void *mk_entry(unsigned long slot, unsigned long pfn)
{
	return xa_mk_value((pfn << SLOT_BITS) | slot);
}

static inline unsigned long entry_slot(void *e)
{
	return xa_to_value(e) & SLOT_MASK;
}

static inline unsigned long entry_pfn(void *e)
{
	return xa_to_value(e) >> SLOT_BITS;
}

struct table {
	struct list_head link;
	struct l4h_obj obj;
};

/* One seL4 address space: a VSpace root plus the page tables we added. */
struct l4h_as {
	unsigned long root;
	struct list_head tables;
	struct xarray pages;
};

static void as_init(struct l4h_as *as, unsigned long root)
{
	as->root = root;
	INIT_LIST_HEAD(&as->tables);
	xa_init(&as->pages);
}

static int as_add_table(struct l4h_as *as, int level, unsigned long va)
{
	struct table *t;
	int err;

	if (level >= L4H_NR_PT_LEVELS)
		return -EFAULT;
	t = kzalloc(sizeof(*t), GFP_ATOMIC);
	if (!t)
		return -ENOMEM;
	err = l4h_obj_alloc(l4h_pt_level_obj[level], 0, &t->obj);
	if (err) {
		kfree(t);
		return err;
	}
	for (;;) {
		err = l4h_pt_map(l4h_pt_level_obj[level], t->obj.cptr, as->root, va);
		if (err != L4H_ERR_FAILED_LOOKUP)
			break;
		err = as_add_table(as, level + 1, va);
		if (err)
			break;
	}
	if (err) {
		l4h_obj_free(&t->obj);
		kfree(t);
		return err == L4H_ERR_DELETE_FIRST ? 0 : -EFAULT;
	}
	list_add(&t->link, &as->tables);
	return 0;
}

static int as_map_page(struct l4h_as *as, unsigned long va, unsigned long phys, int prot)
{
	unsigned long vpn = va >> PAGE_SHIFT, pfn = phys >> PAGE_SHIFT;
	void *e = xa_load(&as->pages, vpn);
	unsigned long slot;
	int err;

	if (e && entry_pfn(e) == pfn) {
		/* Same frame: remap with the new rights. */
		err = l4h_page_map(entry_slot(e), as->root, va, prot);
		return err ? -EFAULT : 0;
	}
	if (e) {
		xa_erase(&as->pages, vpn);
		l4h_slot_delete(entry_slot(e));
	}
	slot = l4h_cap_copy(L4H_FRAME_CPTR(phys), L4H_RIGHTS_ALL);
	if (!slot)
		return -ENOMEM;
	for (;;) {
		err = l4h_page_map(slot, as->root, va, prot);
		if (err != L4H_ERR_FAILED_LOOKUP)
			break;
		err = as_add_table(as, 0, va);
		if (err)
			break;
	}
	if (err) {
		l4h_slot_delete(slot);
		return err < 0 ? err : -EFAULT;
	}
	err = xa_err(xa_store(&as->pages, vpn, mk_entry(slot, pfn), GFP_ATOMIC));
	if (err) {
		l4h_slot_delete(slot);
		return err;
	}
	return 0;
}

static void as_unmap_range(struct l4h_as *as, unsigned long start, unsigned long end)
{
	unsigned long vpn;
	void *e;

	xa_for_each_range(&as->pages, vpn, e, start >> PAGE_SHIFT, (end - 1) >> PAGE_SHIFT) {
		xa_erase(&as->pages, vpn);
		/* Deleting the frame capability removes the mapping. */
		l4h_slot_delete(entry_slot(e));
	}
}

/* Remove every mapping and page table, leaving an empty VSpace root. */
static void as_reset(struct l4h_as *as)
{
	struct table *t, *n;

	as_unmap_range(as, 0, ~0ul);
	list_for_each_entry_safe(t, n, &as->tables, link) {
		list_del(&t->link);
		l4h_obj_free(&t->obj);
		kfree(t);
	}
}

/* ---- the guest-kernel VSpace (vmalloc, modules, ...) */

static struct l4h_as kernel_as;
static DEFINE_RAW_SPINLOCK(kernel_as_lock);

int os_map_memory(void *virt, unsigned long phys, unsigned long len, int r, int w, int x)
{
	int prot = (r ? L4H_PROT_READ : 0) | (w ? L4H_PROT_WRITE : 0) | (x ? L4H_PROT_EXEC : 0);
	unsigned long va = (unsigned long)virt, off, flags;
	int err = 0;

	raw_spin_lock_irqsave(&kernel_as_lock, flags);
	for (off = 0; off < len && !err; off += PAGE_SIZE)
		err = as_map_page(&kernel_as, va + off, phys + off, prot);
	raw_spin_unlock_irqrestore(&kernel_as_lock, flags);
	return err;
}

int os_unmap_memory(void *virt, int len)
{
	unsigned long flags;

	raw_spin_lock_irqsave(&kernel_as_lock, flags);
	as_unmap_range(&kernel_as, (unsigned long)virt, (unsigned long)virt + len);
	raw_spin_unlock_irqrestore(&kernel_as_lock, flags);
	return 0;
}

int os_protect_memory(void *addr, unsigned long len, int r, int w, int x)
{
	/* The kernel image lives in the direct map, which the host set up;
	 * we do not change its protection. */
	return 0;
}

/* Is [addr, addr+size) mapped in the guest-kernel VSpace? */
bool l4h_kernel_range_ok(unsigned long addr, unsigned long size)
{
	unsigned long end = addr + size;

	if (end < addr)
		return false;
	if (addr >= uml_physmem && end <= high_physmem)
		return true;
	if (addr >= l4h_io_pool_start && end <= l4h_io_pool_end)
		return true;
	if (addr >= L4H_SPECIAL_BASE && end <= L4H_SPECIAL_BASE + L4H_BOOTINFO_SIZE)
		return true;
	for (addr &= PAGE_MASK; addr < end; addr += PAGE_SIZE) {
		if (!xa_load(&kernel_as.pages, addr >> PAGE_SHIFT))
			return false;
	}
	return true;
}
EXPORT_SYMBOL(l4h_kernel_range_ok);

/* ---- process VSpaces */

struct vs_slot {
	struct l4h_as as;
	struct mm_struct *owner;	/* NULL if free */
	struct list_head lru;
};

static struct vs_slot *vs_pool;
static int nr_vs;
static LIST_HEAD(vs_lru);		/* owned entries, least recently used first */
static LIST_HEAD(vs_free);
static DEFINE_RAW_SPINLOCK(vs_lock);
static atomic64_t vs_generation = ATOMIC64_INIT(1);

static struct l4h_mm *mm_l4h(struct mm_struct *mm)
{
	return &mm->context.l4h;
}

void l4h_mm_init(struct mm_struct *mm)
{
	struct l4h_mm *m = mm_l4h(mm);

	raw_spin_lock_init(&m->lock);
	m->vs = -1;
	m->gen = 0;
	atomic_set(&m->running, 0);
}

/* Must hold vs_lock and the owner's lock. */
static void vs_detach(struct vs_slot *s)
{
	struct l4h_mm *m = mm_l4h(s->owner);

	as_reset(&s->as);
	m->vs = -1;
	m->gen = 0;
	s->owner = NULL;
	list_move(&s->lru, &vs_free);
}

/*
 * Make sure `mm` has a VSpace and return it (and its generation).  Called
 * before running one of its threads, with the mm marked running so that it
 * is not evicted meanwhile.
 */
unsigned long l4h_mm_activate(struct mm_struct *mm, u64 *gen)
{
	struct l4h_mm *m = mm_l4h(mm);
	unsigned long flags;
	struct vs_slot *s;

	raw_spin_lock_irqsave(&vs_lock, flags);
	if (m->vs >= 0) {
		s = &vs_pool[m->vs];
		list_move_tail(&s->lru, &vs_lru);
		goto out;
	}
	if (list_empty(&vs_free)) {
		struct vs_slot *victim = NULL, *it;
		struct l4h_mm *vm;

		list_for_each_entry(it, &vs_lru, lru) {
			if (!atomic_read(&mm_l4h(it->owner)->running)) {
				victim = it;
				break;
			}
		}
		if (!victim)
			panic("l4h: all %d process VSpaces are in use by running threads\n", nr_vs);
		/* vs_detach() clears the owner: unlock the lock we took. */
		vm = mm_l4h(victim->owner);
		raw_spin_lock(&vm->lock);
		vs_detach(victim);
		raw_spin_unlock(&vm->lock);
	}
	s = list_first_entry(&vs_free, struct vs_slot, lru);
	raw_spin_lock(&m->lock);
	s->owner = mm;
	m->vs = s - vs_pool;
	m->gen = atomic64_inc_return(&vs_generation);
	raw_spin_unlock(&m->lock);
	list_move_tail(&s->lru, &vs_lru);
out:
	*gen = m->gen;
	raw_spin_unlock_irqrestore(&vs_lock, flags);
	return s->as.root;
}

void l4h_mm_release(struct mm_struct *mm)
{
	struct l4h_mm *m = mm_l4h(mm);
	unsigned long flags;

	raw_spin_lock_irqsave(&vs_lock, flags);
	if (m->vs >= 0) {
		raw_spin_lock(&m->lock);
		vs_detach(&vs_pool[m->vs]);
		raw_spin_unlock(&m->lock);
	}
	raw_spin_unlock_irqrestore(&vs_lock, flags);
}

static struct mm_struct *idp_mm(struct mm_id *mm_idp)
{
	return container_of(mm_idp, struct mm_struct, context.id);
}

/*
 * Map [virt, virt+len) to physical `phys` with UM_PROT_* `prot`.  With
 * `skip_enomem`, a page that cannot be mapped for lack of seL4 objects is
 * left unmapped (any old mapping of it is gone) and the others are still
 * done: its next access faults, and the fault path waits for memory.
 */
static int map_pages(struct mm_id *mm_idp, unsigned long virt, unsigned long len, int prot,
		     unsigned long phys, bool skip_enomem)
{
	struct l4h_mm *m = mm_l4h(idp_mm(mm_idp));
	unsigned long flags, off;
	int err = 0;

	raw_spin_lock_irqsave(&m->lock, flags);
	if (m->vs >= 0) {
		struct l4h_as *as = &vs_pool[m->vs].as;

		if (!prot) {
			as_unmap_range(as, virt, virt + len);
		} else {
			for (off = 0; off < len && !err; off += PAGE_SIZE) {
				err = as_map_page(as, virt + off, phys + off, prot);
				if (err == -ENOMEM && skip_enomem)
					err = 0;
			}
		}
	}
	raw_spin_unlock_irqrestore(&m->lock, flags);
	return err;
}

/* tlb.c: a TLB sync must apply every unmap in its range, so pages it
 * cannot map do not stop it. */
int map(struct mm_id *mm_idp, unsigned long virt, unsigned long len, int prot, unsigned long phys)
{
	return map_pages(mm_idp, virt, len, prot, phys, true);
}

int unmap(struct mm_id *mm_idp, unsigned long addr, unsigned long len)
{
	struct l4h_mm *m = mm_l4h(idp_mm(mm_idp));
	unsigned long flags;

	raw_spin_lock_irqsave(&m->lock, flags);
	if (m->vs >= 0)
		as_unmap_range(&vs_pool[m->vs].as, addr, addr + len);
	raw_spin_unlock_irqrestore(&m->lock, flags);
	return 0;
}

/*
 * After a successful page fault, install the PTE for `address` directly,
 * with the protection UML's TLB sync would use (it emulates the accessed
 * and dirty bits by mapping old pages inaccessible and clean ones
 * read-only).  Returns -ENOMEM if there was no memory for the seL4
 * objects the mapping needs.
 */
int l4h_map_fault(struct mm_struct *mm, unsigned long address)
{
	unsigned long va = address & PAGE_MASK;
	pgd_t *pgd;
	p4d_t *p4d;
	pud_t *pud;
	pmd_t *pmd;
	pte_t *pte, entry;
	spinlock_t *ptl;
	int prot, err = 0;

	/* handle_page_fault() releases mmap_lock before returning to segv(),
	 * so a concurrent munmap() may be freeing the page tables of `va`.
	 * It frees them under a read lock (after downgrading mmap_lock), but
	 * only once their VMAs are detached, and never the tables a remaining
	 * VMA spans: holding the read lock and finding `va`'s VMA still there
	 * keeps the hierarchy stable while we walk it.  (The PTE lock below
	 * protects only the leaf entry.)
	 */
	mmap_read_lock(mm);
	if (!vma_lookup(mm, va))
		goto out_unlock;
	pgd = pgd_offset(mm, va);
	if (!pgd_present(*pgd))
		goto out_unlock;
	p4d = p4d_offset(pgd, va);
	if (!p4d_present(*p4d))
		goto out_unlock;
	pud = pud_offset(p4d, va);
	if (!pud_present(*pud))
		goto out_unlock;
	pmd = pmd_offset(pud, va);
	if (!pmd_present(*pmd))
		goto out_unlock;
	/* Hold the PTE lock so that a concurrent unmap on another vCPU cannot
	 * complete (and sync) between reading the PTE and installing it. */
	pte = pte_offset_map_lock(mm, pmd, va, &ptl);
	if (!pte)
		goto out_unlock;
	entry = *pte;
	if (pte_present(entry) && pte_young(entry) && pte_read(entry)) {
		prot = UM_PROT_READ;
		if (pte_exec(entry))
			prot |= UM_PROT_EXEC;
		if (pte_write(entry) && pte_dirty(entry))
			prot |= UM_PROT_WRITE;
		err = map_pages(&mm->context.id, va, PAGE_SIZE, prot, pte_val(entry) & PAGE_MASK, false);
	}
	pte_unmap_unlock(pte, ptl);
out_unlock:
	mmap_read_unlock(mm);
	return err == -ENOMEM ? err : 0;
}


void __init l4h_vspace_init(void)
{
	int i;

	as_init(&kernel_as, L4H_CPTR(L4H_SLOT_VSPACE));
	nr_vs = l4h_bootinfo->nr_vspaces;
	if (nr_vs < 1)
		panic("l4h: no process VSpaces in boot info\n");
	vs_pool = memblock_alloc(nr_vs * sizeof(*vs_pool), SMP_CACHE_BYTES);
	if (!vs_pool)
		panic("l4h: cannot allocate VSpace pool\n");
	for (i = 0; i < nr_vs; i++) {
		as_init(&vs_pool[i].as, L4H_CPTR2(L4H_SLOT_VSPACE_POOL, i));
		list_add_tail(&vs_pool[i].lru, &vs_free);
	}
}
