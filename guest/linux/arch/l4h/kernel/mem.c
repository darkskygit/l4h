// SPDX-License-Identifier: GPL-2.0
/*
 * Copyright (C) 2000 - 2007 Jeff Dike (jdike@{addtoit,linux.intel}.com)
 */

#include <linux/stddef.h>
#include <linux/module.h>
#include <linux/memblock.h>
#include <linux/mm.h>
#include <linux/swap.h>
#include <linux/slab.h>
#include <linux/init.h>
#include <asm/sections.h>
#include <asm/page.h>
#include <asm/pgalloc.h>
#include <as-layout.h>
#include <init.h>
#include <kern.h>
#include <kern_util.h>
#include <mem_user.h>
#include <os.h>
#include <um_malloc.h>
#include <linux/sched/task.h>

/* allocated in paging_init, zeroed in mem_init, and unchanged thereafter */
unsigned long *empty_zero_page = NULL;
EXPORT_SYMBOL(empty_zero_page);

/*
 * Initialized during boot, and readonly for initializing page tables
 * afterwards
 */
pgd_t swapper_pg_dir[PTRS_PER_PGD];

/* Initialized at boot time, and readonly after that */
int kmalloc_ok = 0;

void __init arch_mm_preinit(void)
{
	/* clear the zero-page */
	memset(empty_zero_page, 0, PAGE_SIZE);
}

void __init mem_init(void)
{
	kmalloc_ok = 1;
}

void __init paging_init(void)
{
	unsigned long max_zone_pfn[MAX_NR_ZONES] = { 0 };

	empty_zero_page = (unsigned long *) memblock_alloc_low(PAGE_SIZE,
							       PAGE_SIZE);
	if (!empty_zero_page)
		panic("%s: Failed to allocate %lu bytes align=%lx\n",
		      __func__, PAGE_SIZE, PAGE_SIZE);

	max_zone_pfn[ZONE_NORMAL] = end_iomem >> PAGE_SHIFT;
	free_area_init(max_zone_pfn);

}

/*
 * This can't do anything because nothing in the kernel image can be freed
 * since it's not in kernel physical memory.
 */

void free_initmem(void)
{
}

/* Allocate and free page tables. */

pgd_t *pgd_alloc(struct mm_struct *mm)
{
	pgd_t *pgd = __pgd_alloc(mm, 0);

	if (pgd)
		memcpy(pgd + USER_PTRS_PER_PGD,
		       swapper_pg_dir + USER_PTRS_PER_PGD,
		       (PTRS_PER_PGD - USER_PTRS_PER_PGD) * sizeof(pgd_t));

	return pgd;
}

void *uml_kmalloc(int size, int flags)
{
	return kmalloc(size, flags);
}

static const pgprot_t protection_map[16] = {
	[VM_NONE]					= PAGE_NONE,
	[VM_READ]					= PAGE_READONLY,
	[VM_WRITE]					= PAGE_COPY,
	[VM_WRITE | VM_READ]				= PAGE_COPY,
	[VM_EXEC]					= PAGE_READONLY_EXEC,
	[VM_EXEC | VM_READ]				= PAGE_READONLY_EXEC,
	[VM_EXEC | VM_WRITE]				= PAGE_COPY_EXEC,
	[VM_EXEC | VM_WRITE | VM_READ]			= PAGE_COPY_EXEC,
	[VM_SHARED]					= PAGE_NONE,
	[VM_SHARED | VM_READ]				= PAGE_READONLY,
	[VM_SHARED | VM_WRITE]				= PAGE_SHARED,
	[VM_SHARED | VM_WRITE | VM_READ]		= PAGE_SHARED,
	[VM_SHARED | VM_EXEC]				= PAGE_READONLY_EXEC,
	[VM_SHARED | VM_EXEC | VM_READ]			= PAGE_READONLY_EXEC,
	[VM_SHARED | VM_EXEC | VM_WRITE]		= PAGE_SHARED_EXEC,
	[VM_SHARED | VM_EXEC | VM_WRITE | VM_READ]	= PAGE_SHARED_EXEC
};
DECLARE_VM_GET_PAGE_PROT

void mark_rodata_ro(void)
{
	/*
	 * The host maps text and rodata read-only, shared by all guests
	 * (vmlinux.lds.S); __ro_after_init data is with the data, writable.
	 */
}

/*
 * set_ptes() with nr > 1 must map consecutive pages.  Without THP this
 * kernel has no large folios, so nothing calls it with nr > 1 at run time:
 * check the stride once at boot instead.  A scratch mm takes the sync mark.
 */
static struct mm_struct set_ptes_mm __initdata;

static int __init set_ptes_check(void)
{
	pte_t ptes[4];
	unsigned long pfn = 0x1234;
	int i;

	raw_spin_lock_init(&set_ptes_mm.context.sync_lock);
	set_ptes(&set_ptes_mm, 0, ptes, pfn_pte(pfn, PAGE_READONLY), ARRAY_SIZE(ptes));
	for (i = 0; i < ARRAY_SIZE(ptes); i++)
		if (pte_pfn(ptes[i]) != pfn + i)
			panic("l4h: set_ptes() entry %d maps pfn %lx, not %lx\n",
			      i, pte_pfn(ptes[i]), pfn + i);
	return 0;
}
early_initcall(set_ptes_check);
