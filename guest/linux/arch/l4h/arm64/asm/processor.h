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
/* arch/l4h/os-sel4/events.c: deliver pending events if interrupts are on. */
void l4h_poll_events(void);

/* Spinning with interrupts on polls for them, as on x86 (asm/processor.h
 * there): a CPU would take them. */
static inline void cpu_relax(void)
{
	asm volatile("yield" ::: "memory");
	l4h_poll_events();
}
/* The user PC, as on arm64 (seccomp, SIGSYS); KSTK_ESP is the kernel's. */
#define KSTK_EIP(t) PT_REGS_IP(task_pt_regs(t))
#define KSTK_ESP(t) KSTK_REG(t, HOST_SP)
#define KSTK_EBP(t) KSTK_REG(t, HOST_BP)
#define ARCH_IS_STACKGROW(a) ((a) + 65536 >= UPT_SP(&current->thread.regs.regs))
#define task_pt_regs(t) (&(t)->thread.regs)
#include <asm/processor-generic.h>
#endif
