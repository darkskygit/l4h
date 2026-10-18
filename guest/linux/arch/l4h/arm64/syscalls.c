// SPDX-License-Identifier: GPL-2.0
#include <linux/syscalls.h>
#include <linux/sys.h>
#include <linux/cache.h>
#include <asm/syscall.h>
SYSCALL_DEFINE6(mmap, unsigned long, addr, unsigned long, len, unsigned long,
		prot, unsigned long, flags, unsigned long, fd, unsigned long,
		off)
{
	if (off & ~PAGE_MASK)
		return -EINVAL;
	return ksys_mmap_pgoff(addr, len, prot, flags, fd, off >> PAGE_SHIFT);
}
