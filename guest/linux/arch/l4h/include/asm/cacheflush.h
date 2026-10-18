#ifndef __UM_ASM_CACHEFLUSH_H
#define __UM_ASM_CACHEFLUSH_H

#include <asm/tlbflush.h>
#define flush_cache_vmap flush_tlb_kernel_range
#define flush_cache_vunmap flush_tlb_kernel_range

#ifdef CONFIG_UML_ARM64
void l4h_flush_icache(unsigned long start, unsigned long end);
#define flush_icache_range l4h_flush_icache
#endif
#include <asm-generic/cacheflush.h>
#endif /* __UM_ASM_CACHEFLUSH_H */
