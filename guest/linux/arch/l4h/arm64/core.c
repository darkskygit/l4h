// SPDX-License-Identifier: GPL-2.0
#include <linux/export.h>
#include <linux/mm.h>
#include <linux/delay.h>
#include <linux/sched.h>
#include <linux/sched/debug.h>
#include <linux/elf.h>
#include <linux/uaccess.h>
#include <l4h_sys.h>
#include <l4h_uctx.h>
#include <arch.h>
#include <registers.h>

void arch_check_bugs(void)
{
}
void arch_examine_signal(int sig, struct uml_pt_regs *r)
{
}
void arch_switch_to(struct task_struct *to)
{
}
void clear_flushed_tls(struct task_struct *t)
{
}
int arch_set_tls(struct task_struct *t, unsigned long tls)
{
	t->thread.regs.regs.gp[HOST_TLS] = tls;
	l4h_seg_dirty(t);
	return 0;
}
/* loops_per_jiffy is in counter ticks (lpj_fine, kernel/time.c), as on
 * arm64: wait for the counter to advance by `loops`. */
void __delay(unsigned long loops)
{
	unsigned long long start = l4h_rdtsc();

	while (l4h_rdtsc() - start < loops)
		cpu_relax();
}
EXPORT_SYMBOL(__delay);
void __udelay(unsigned long us)
{
	unsigned long long end = l4h_rdtsc() + us * l4h_tsc_hz / 1000000;
	while ((long long)(l4h_rdtsc() - end) < 0)
		cpu_relax();
}
EXPORT_SYMBOL(__udelay);
void __ndelay(unsigned long ns)
{
	unsigned long long end = l4h_rdtsc() + ns * l4h_tsc_hz / 1000000000;
	while ((long long)(l4h_rdtsc() - end) < 0)
		cpu_relax();
}
EXPORT_SYMBOL(__ndelay);
/* xloops is in 2^-32 s (asm-generic/delay.h): a constant ndelay() passes
 * it too, so it must not be rounded down to whole microseconds. */
void __const_udelay(unsigned long xloops)
{
	__delay(((xloops * l4h_tsc_hz) >> 32) + 1);
}
EXPORT_SYMBOL(__const_udelay);
void show_regs(struct pt_regs *r)
{
	int i;
	pr_info("PC: %pS SP: %016lx PSTATE: %08lx\n", (void *)PT_REGS_IP(r),
		PT_REGS_SP(r), r->regs.gp[33]);
	for (i = 0; i < 31; i++)
		pr_info("x%d: %016lx\n", i, r->regs.gp[i]);
}
const char *arch_vma_name(struct vm_area_struct *vma)
{
	return vma->vm_start == um_vdso_addr ? "[vdso]" : NULL;
}
