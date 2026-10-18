/* SPDX-License-Identifier: GPL-2.0 */
#ifndef L4H_ARM64_PROCESSOR_H
#define L4H_ARM64_PROCESSOR_H
#include <sysdep/faultinfo.h>
struct arch_thread {
	struct faultinfo faultinfo;
};
#define INIT_ARCH_THREAD \
	{                \
	}
#define STACKSLOTS_PER_LINE 4
static inline void arch_flush_thread(struct arch_thread *t)
{
}
static inline void arch_copy_thread(struct arch_thread *f,
				    struct arch_thread *t)
{
}
#define current_sp()                          \
	({                                    \
		void *sp;                     \
		asm("mov %0, sp" : "=r"(sp)); \
		sp;                           \
	})
#define current_bp()                           \
	({                                     \
		unsigned long fp;              \
		asm("mov %0, x29" : "=r"(fp)); \
		fp;                            \
	})
static inline void cpu_relax(void)
{
	asm volatile("yield" ::: "memory");
}
#define KSTK_EIP(t) KSTK_REG(t, HOST_IP)
#define KSTK_ESP(t) KSTK_REG(t, HOST_SP)
#define KSTK_EBP(t) KSTK_REG(t, HOST_BP)
#define ARCH_IS_STACKGROW(a) ((a) + 65536 >= UPT_SP(&current->thread.regs.regs))
#define task_pt_regs(t) (&(t)->thread.regs)
#include <asm/processor-generic.h>
#endif
