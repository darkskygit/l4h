// SPDX-License-Identifier: GPL-2.0
/*
 * Guest-physical memory of the l4h guest.  All of it is mapped by the host
 * at uml_physmem (GPA 0), so there is nothing to map here: we only tell
 * memblock about it.
 *
 * The host's I/O pool follows RAM (bootinfo.io_pool_gpa).  It is in the
 * direct map and has struct pages (swiotlb hands out its pages for
 * coherent allocations), but it is reserved: Linux uses it only through
 * the DMA API (restricted DMA, see drivers/virtio_l4h.c).
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

/* VA range of the I/O pool in the direct map (empty if none). */
unsigned long l4h_io_pool_start, l4h_io_pool_end;

/**
 * setup_physmem() - register guest RAM and the I/O pool
 * @start:	VA of GPA 0 (the start of the kernel image)
 * @reserve_end: VA of the end of the kernel image
 * @len:	size of guest RAM in bytes
 */
void __init setup_physmem(unsigned long start, unsigned long reserve_end,
			  unsigned long len)
{
	const struct l4h_bootinfo *bi = l4h_bootinfo;
	unsigned long reserve = reserve_end - start;
	unsigned long end = len;

	if (len <= reserve)
		panic("l4h: kernel image (%lu bytes) larger than RAM (%lu)\n", reserve, len);

	memblock_add(0, len);
	memblock_reserve(0, reserve);
	if (bi->initrd_size)
		memblock_reserve(bi->initrd_gpa, PAGE_ALIGN(bi->initrd_size));
	if (bi->io_pool_size) {
		if (bi->io_pool_gpa < len || !PAGE_ALIGNED(bi->io_pool_gpa) ||
		    !PAGE_ALIGNED(bi->io_pool_size))
			panic("l4h: bad I/O pool %#llx+%#llx\n", bi->io_pool_gpa, bi->io_pool_size);
		memblock_add(bi->io_pool_gpa, bi->io_pool_size);
		memblock_reserve(bi->io_pool_gpa, bi->io_pool_size);
		end = bi->io_pool_gpa + bi->io_pool_size;
		l4h_io_pool_start = start + bi->io_pool_gpa;
		l4h_io_pool_end = l4h_io_pool_start + bi->io_pool_size;
	}

	min_low_pfn = PFN_UP(reserve);
	max_low_pfn = end >> PAGE_SHIFT;
	max_pfn = max_low_pfn;
}
