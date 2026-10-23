#ifndef __UM_ASM_CACHEFLUSH_H
#define __UM_ASM_CACHEFLUSH_H

#include <asm/tlbflush.h>
#define flush_cache_vmap l4h_kernel_map_sync
#define flush_cache_vunmap flush_tlb_kernel_range

#ifdef CONFIG_UML_ARM64
void l4h_flush_icache(unsigned long start, unsigned long end);
#define flush_icache_range l4h_flush_icache
/* As arm64's flush_ptrace_access(): code written by ptrace (breakpoints)
 * into a page that is already mapped executable. */
#define copy_to_user_page(vma, page, vaddr, dst, src, len)		\
	do {								\
		memcpy(dst, src, len);					\
		if ((vma)->vm_flags & VM_EXEC)				\
			l4h_flush_icache((unsigned long)(dst),		\
					 (unsigned long)(dst) + (len));	\
	} while (0)
#endif
#include <asm-generic/cacheflush.h>
#endif /* __UM_ASM_CACHEFLUSH_H */
