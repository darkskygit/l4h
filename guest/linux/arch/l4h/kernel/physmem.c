// SPDX-License-Identifier: GPL-2.0
/*
 * Guest-physical memory of the l4h guest.  All of it is mapped by the host
 * at uml_physmem (GPA 0), so there is nothing to map here: we only tell
 * memblock about it.
 */

#include <linux/module.h>
#include <linux/memblock.h>
#include <linux/mm.h>
#include <linux/pfn.h>
#include <asm/page.h>
#include <asm/sections.h>
#include <as-layout.h>
#include <init.h>
#include <kern.h>
#include <kern_util.h>
#include <mem_user.h>
#include <os.h>

/* Changed during early boot */
unsigned long high_physmem;
EXPORT_SYMBOL(high_physmem);

/**
 * setup_physmem() - register guest RAM
 * @start:	VA of GPA 0 (the start of the kernel image)
 * @reserve_end: VA of the end of the kernel image
 * @len:	size of guest RAM in bytes
 */
void __init setup_physmem(unsigned long start, unsigned long reserve_end,
			  unsigned long len)
{
	const struct l4h_bootinfo *bi = l4h_bootinfo;
	unsigned long reserve = reserve_end - start;

	if (len <= reserve)
		panic("l4h: kernel image (%lu bytes) larger than RAM (%lu)\n", reserve, len);

	memblock_add(0, len);
	memblock_reserve(0, reserve);
	if (bi->initrd_size)
		memblock_reserve(bi->initrd_gpa, PAGE_ALIGN(bi->initrd_size));

	min_low_pfn = PFN_UP(reserve);
	max_low_pfn = len >> PAGE_SHIFT;
	max_pfn = max_low_pfn;
}
