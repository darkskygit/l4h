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
void __delay(unsigned long loops)
{
	while (loops--)
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
void __const_udelay(unsigned long xloops)
{
	__udelay((xloops * 1000000ULL) >> 32);
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
