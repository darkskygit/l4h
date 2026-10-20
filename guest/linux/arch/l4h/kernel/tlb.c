// SPDX-License-Identifier: GPL-2.0
/*
 * Copyright (C) 2000 - 2007 Jeff Dike (jdike@{addtoit,linux.intel}.com)
 */

#include <linux/mm.h>
#include <linux/module.h>
#include <linux/sched/signal.h>

#include <asm/tlbflush.h>
#include <asm/mmu_context.h>
#include <as-layout.h>
#include <mem_user.h>
#include <os.h>
#include <skas.h>
#include <kern_util.h>
#include <l4h_vspace.h>
#include <l4h_caps.h>

struct vm_ops {
	struct mm_id *mm_idp;

	int (*mmap)(struct mm_id *mm_idp,
		    unsigned long virt, unsigned long len, int prot,
		    unsigned long phys);
	int (*unmap)(struct mm_id *mm_idp,
		     unsigned long virt, unsigned long len);
	/* Kernel syncs: a map ran out of seL4 objects (see um_tlb_sync()). */
	bool enomem;
};

static int kern_map(struct mm_id *mm_idp,
		    unsigned long virt, unsigned long len, int prot,
		    unsigned long phys)
{
	return os_map_memory((void *)virt, phys, len,
			     prot & UM_PROT_READ, prot & UM_PROT_WRITE, 1);
}

static int kern_unmap(struct mm_id *mm_idp,
		      unsigned long virt, unsigned long len)
{
	return os_unmap_memory((void *)virt, len);
}

static int sync_one_pte(pte_t *pte, unsigned long addr, struct vm_ops *ops)
{
	for (;;) {
		pte_t v = READ_ONCE(*pte);
		int ret;

		if (!pte_needsync(v))
			return 0;

		if (pte_present(v)) {
			unsigned long phys = pte_val(v) & PAGE_MASK;
			int r, w, x, prot;

			r = pte_read(v);
			w = pte_write(v);
			x = pte_exec(v);
			/*
			 * Old pages stay mapped readable (seL4 has no accessed
			 * bit to emulate; unmapping them only costs faults), and
			 * clean ones read-only so that the first write marks
			 * them dirty.
			 */
			if (!pte_young(v) || !pte_dirty(v))
				w = 0;

			prot = (r ? UM_PROT_READ : 0) |
			       (w ? UM_PROT_WRITE : 0) |
			       (x ? UM_PROT_EXEC : 0);

			ret = ops->mmap(ops->mm_idp, addr, PAGE_SIZE, prot, phys);
			if (ret == -ENOMEM && ops->mmap == kern_map) {
				/* Left to sync again; the walk goes on, since
				 * its unmaps must all happen. */
				ops->enomem = true;
				return 0;
			}
		} else
			ret = ops->unmap(ops->mm_idp, addr, PAGE_SIZE);
		if (ret)
			return ret;
		/* Only mark the PTE synced if nobody changed it meanwhile;
		 * otherwise apply the new value too. */
		if (try_cmpxchg(&pte_val(*pte), &pte_val(v), pte_val(pte_mkuptodate(v))))
			return 0;
	}
}

static inline int update_pte_range(pmd_t *pmd, unsigned long addr,
				   unsigned long end,
				   struct vm_ops *ops)
{
	pte_t *pte;
	int ret = 0;

	pte = pte_offset_kernel(pmd, addr);
	do {
		ret = sync_one_pte(pte, addr, ops);
	} while (pte++, addr += PAGE_SIZE, ((addr < end) && !ret));
	return ret;
}

static inline int update_pmd_range(pud_t *pud, unsigned long addr,
				   unsigned long end,
				   struct vm_ops *ops)
{
	pmd_t *pmd;
	unsigned long next;
	int ret = 0;

	pmd = pmd_offset(pud, addr);
	do {
		next = pmd_addr_end(addr, end);
		if (!pmd_present(*pmd)) {
			if (pmd_needsync(*pmd)) {
				ret = ops->unmap(ops->mm_idp, addr,
						 next - addr);
				pmd_mkuptodate(*pmd);
			}
		}
		else ret = update_pte_range(pmd, addr, next, ops);
	} while (pmd++, addr = next, ((addr < end) && !ret));
	return ret;
}

static inline int update_pud_range(p4d_t *p4d, unsigned long addr,
				   unsigned long end,
				   struct vm_ops *ops)
{
	pud_t *pud;
	unsigned long next;
	int ret = 0;

	pud = pud_offset(p4d, addr);
	do {
		next = pud_addr_end(addr, end);
		if (!pud_present(*pud)) {
			if (pud_needsync(*pud)) {
				ret = ops->unmap(ops->mm_idp, addr,
						 next - addr);
				pud_mkuptodate(*pud);
			}
		}
		else ret = update_pmd_range(pud, addr, next, ops);
	} while (pud++, addr = next, ((addr < end) && !ret));
	return ret;
}

static inline int update_p4d_range(pgd_t *pgd, unsigned long addr,
				   unsigned long end,
				   struct vm_ops *ops)
{
	p4d_t *p4d;
	unsigned long next;
	int ret = 0;

	p4d = p4d_offset(pgd, addr);
	do {
		next = p4d_addr_end(addr, end);
		if (!p4d_present(*p4d)) {
			if (p4d_needsync(*p4d)) {
				ret = ops->unmap(ops->mm_idp, addr,
						 next - addr);
				p4d_mkuptodate(*p4d);
			}
		} else
			ret = update_pud_range(p4d, addr, next, ops);
	} while (p4d++, addr = next, ((addr < end) && !ret));
	return ret;
}

/*
 * Apply the pending range of `mm` to its seL4 address space.
 *
 * Every vCPU syncs current->mm before returning to user, so the range a
 * flush has just marked may be taken by another vCPU's sync.  The flush
 * must not return before that sync is complete: its caller goes on to free
 * the pages (or page tables), and a thread of the mm on a third vCPU could
 * still write to them through the old mapping.  Two concurrent syncs would
 * also race on a PTE: one maps the value it read, after the other has
 * unmapped the newer value and marked it synced.  Syncs of an mm therefore
 * hold sync_walk_lock from taking the range until it is applied.
 *
 * Callers include flushes under a PTE lock, so this must not sleep.  Kernel
 * syncs are rare and short and also keep interrupts off; user syncs may be
 * long and leave them on (no interrupt handler syncs a user mm).
 *
 * User maps that find no memory for seL4 objects are dropped: the page
 * faults again.  Kernel maps cannot be: a fault of the guest kernel stops
 * the guest.  Their PTEs stay unsynced, the range is marked again, and the
 * sync returns -ENOMEM for l4h_kernel_map_sync() to retry.
 */
int um_tlb_sync(struct mm_struct *mm)
{
	pgd_t *pgd;
	struct vm_ops ops = {};
	unsigned long start, addr, end, next, flags, wflags = 0;
	int ret = 0;

	if (mm == &init_mm)
		raw_spin_lock_irqsave(&mm->context.sync_walk_lock, wflags);
	else
		raw_spin_lock(&mm->context.sync_walk_lock);

	/* Take the pending range; marks made from now on go to the next sync. */
	raw_spin_lock_irqsave(&mm->context.sync_lock, flags);
	addr = mm->context.sync_tlb_range_from;
	end = mm->context.sync_tlb_range_to;
	mm->context.sync_tlb_range_from = 0;
	mm->context.sync_tlb_range_to = 0;
	raw_spin_unlock_irqrestore(&mm->context.sync_lock, flags);
	if (end == 0)
		goto out;
	start = addr;

	ops.mm_idp = &mm->context.id;
	if (mm == &init_mm) {
		ops.mmap = kern_map;
		ops.unmap = kern_unmap;
	} else {
		ops.mmap = map;
		ops.unmap = unmap;
	}

	pgd = pgd_offset(mm, addr);
	do {
		next = pgd_addr_end(addr, end);
		if (!pgd_present(*pgd)) {
			if (pgd_needsync(*pgd)) {
				ret = ops.unmap(ops.mm_idp, addr,
						next - addr);
				pgd_mkuptodate(*pgd);
			}
		} else
			ret = update_p4d_range(pgd, addr, next, &ops);
	} while (pgd++, addr = next, ((addr < end) && !ret));
	if (mm != &init_mm)
		l4h_mm_flush_deferred(mm);
	if (ops.enomem) {
		um_tlb_mark_sync(mm, start, end);
		if (!ret)
			ret = -ENOMEM;
	}

out:
	if (mm == &init_mm)
		raw_spin_unlock_irqrestore(&mm->context.sync_walk_lock, wflags);
	else
		raw_spin_unlock(&mm->context.sync_walk_lock);

	return ret;
}

/*
 * A kernel map found no memory.  It may have lacked seL4 objects or an
 * atomic allocation (a slot CNode, the mapping's xarray node): make room
 * for both, and sleep, so that the retry neither spins with the vCPU taken
 * nor waits for memory nobody frees.
 */
static void kernel_map_wait(void)
{
	unsigned long page;

	if (!l4h_objmem_wait())
		l4h_objmem_oom();
	/* Reclaims, or calls the OOM killer, below the watermarks. */
	page = __get_free_page(GFP_KERNEL | __GFP_NOWARN);
	if (page)
		free_page(page);
	schedule_timeout_uninterruptible(1);
}

/*
 * Make new kernel mappings (vmap, vmalloc, percpu) reachable before the
 * caller uses them.  Every caller may sleep: wait for memory and retry, as
 * the page fault path does.
 */
void l4h_kernel_map_sync(unsigned long start, unsigned long end)
{
	um_tlb_mark_sync(&init_mm, start, end);
	while (um_tlb_sync(&init_mm) == -ENOMEM)
		kernel_map_wait();
}

void flush_tlb_all(void)
{
	/*
	 * Don't bother flushing if this address space is about to be
	 * destroyed.
	 */
	if (atomic_read(&current->mm->mm_users) == 0)
		return;

	flush_tlb_mm(current->mm);
}

void flush_tlb_mm(struct mm_struct *mm)
{
	struct vm_area_struct *vma;
	VMA_ITERATOR(vmi, mm, 0);

	for_each_vma(vmi, vma)
		um_tlb_mark_sync(mm, vma->vm_start, vma->vm_end);
	l4h_tlb_flush_sync(mm);
}
