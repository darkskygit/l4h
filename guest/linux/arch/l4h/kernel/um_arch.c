// SPDX-License-Identifier: GPL-2.0
/*
 * Copyright (C) 2000 - 2007 Jeff Dike (jdike@{addtoit,linux.intel}.com)
 *
 * Boot of the l4h guest kernel: memory layout and command line come from
 * the l4h boot info instead of the host process.
 */

#include <linux/cpu.h>
#include <linux/delay.h>
#include <linux/init.h>
#include <linux/mm.h>
#include <linux/ctype.h>
#include <linux/module.h>
#include <linux/panic_notifier.h>
#include <linux/seq_file.h>
#include <linux/string.h>
#include <linux/string_choices.h>
#include <linux/utsname.h>
#include <linux/sched.h>
#include <linux/sched/task.h>
#include <linux/kmsg_dump.h>
#include <linux/random.h>
#include <linux/memblock.h>

#include <asm/processor.h>
#include <asm/cpufeature.h>
#include <asm/sections.h>
#include <asm/setup.h>
#include <asm/text-patching.h>
#include <as-layout.h>
#include <arch.h>
#include <init.h>
#include <kern.h>
#include <kern_util.h>
#include <mem_user.h>
#include <os.h>
#include <l4h_sys.h>
#include <l4h_caps.h>
#include <l4h_vspace.h>
#include <l4h_uctx.h>

#include "um_arch.h"

static char __initdata command_line[COMMAND_LINE_SIZE] = { 0 };

struct cpuinfo_um boot_cpu_data = {
	.loops_per_jiffy	= 0,
	.cache_alignment	= L1_CACHE_BYTES,
	.x86_capability		= { 0 }
};
EXPORT_SYMBOL(boot_cpu_data);

static char host_info[(__NEW_UTS_LEN + 1) * 5];

static int show_cpuinfo(struct seq_file *m, void *v)
{
	int i, cpu = (struct cpuinfo_um *)v - &boot_cpu_data;

	seq_printf(m, "processor\t: %d\n", cpu);
	seq_printf(m, "vendor_id\t: l4h\n");
	seq_printf(m, "model name\t: l4h paravirtual vCPU\n");
	seq_printf(m, "host\t\t: %s\n", host_info);
	seq_printf(m, "fpu\t\t: %s\n", str_yes_no(cpu_has(&boot_cpu_data, X86_FEATURE_FPU)));
	seq_printf(m, "flags\t\t:");
	for (i = 0; i < 32*NCAPINTS; i++)
		if (cpu_has(&boot_cpu_data, i) && (x86_cap_flags[i] != NULL))
			seq_printf(m, " %s", x86_cap_flags[i]);
	seq_printf(m, "\n");
	seq_printf(m, "cache_alignment\t: %d\n", boot_cpu_data.cache_alignment);
	seq_printf(m, "bogomips\t: %lu.%02lu\n\n",
		   loops_per_jiffy/(500000/HZ),
		   (loops_per_jiffy/(5000/HZ)) % 100);
	return 0;
}

static void *c_start(struct seq_file *m, loff_t *pos)
{
	while (*pos < nr_cpu_ids && !cpu_online(*pos))
		++*pos;
	/* All vCPUs share boot_cpu_data; pass the index through the pointer. */
	return *pos < nr_cpu_ids ? &boot_cpu_data + *pos : NULL;
}

static void *c_next(struct seq_file *m, void *v, loff_t *pos)
{
	++*pos;
	return c_start(m, pos);
}

static void c_stop(struct seq_file *m, void *v)
{
}

const struct seq_operations cpuinfo_op = {
	.start	= c_start,
	.next	= c_next,
	.stop	= c_stop,
	.show	= show_cpuinfo,
};

/* Guest RAM is mapped at uml_physmem: GPA 0 == uml_physmem. */
unsigned long uml_physmem;
EXPORT_SYMBOL(uml_physmem);
unsigned long uml_reserved;
unsigned long start_vm;
unsigned long end_vm;
unsigned long long physmem_size;
EXPORT_SYMBOL(physmem_size);

unsigned long task_size;
EXPORT_SYMBOL(task_size);
long elf_aux_hwcap;
unsigned long end_iomem;
EXPORT_SYMBOL(end_iomem);

/* Process address spaces end below the last 512 GiB of the seL4 user
 * address range (PGDIR aligned, as UML requires). */
#define L4H_TASK_SIZE	0x00007f8000000000UL

static int panic_exit(struct notifier_block *self, unsigned long unused1,
		      void *unused2)
{
	kmsg_dump(KMSG_DUMP_PANIC);
	bust_spinlocks(1);
	bust_spinlocks(0);
	os_dump_core();
	return NOTIFY_DONE;
}

static struct notifier_block panic_exit_notifier = {
	.notifier_call	= panic_exit,
	.priority	= INT_MAX - 1, /* run as 2nd notifier, won't return */
};

static void uml_postsetup(void)
{
	initcall_t *p;

	p = &__uml_postsetup_start;
	while (p < &__uml_postsetup_end) {
		(*p)();
		p++;
	}
}

void uml_finishsetup(void)
{
	atomic_notifier_chain_register(&panic_notifier_list,
				       &panic_exit_notifier);

	uml_postsetup();

	new_thread_handler();
}

static void __init cpuid(u32 leaf, u32 sub, u32 *a, u32 *b, u32 *c, u32 *d)
{
	asm volatile("cpuid" : "=a"(*a), "=b"(*b), "=c"(*c), "=d"(*d) : "a"(leaf), "c"(sub));
}

/* Words of x86_capability[] (as in arch/x86/include/asm/cpufeature.h). */
enum {
	CPUID_1_EDX = 0, CPUID_8000_0001_EDX = 1, CPUID_1_ECX = 4,
	CPUID_8000_0001_ECX = 6, CPUID_7_0_EBX = 9, CPUID_7_ECX = 16, CPUID_7_EDX = 18,
};

/* CPU features as seen by user code: cpuid is not intercepted. */
static void __init l4h_cpu_features(void)
{
	u32 a, b, c, d, max;

	cpuid(0, 0, &max, &b, &c, &d);
	cpuid(1, 0, &a, &b, &c, &d);
	elf_aux_hwcap = d;
	boot_cpu_data.x86_capability[CPUID_1_EDX] = d;
	boot_cpu_data.x86_capability[CPUID_1_ECX] = c;
	boot_cpu_data.cache_alignment = ((b >> 8) & 0xff) * 8 ?: L1_CACHE_BYTES;
	if (max >= 7) {
		cpuid(7, 0, &a, &b, &c, &d);
		boot_cpu_data.x86_capability[CPUID_7_0_EBX] = b;
		boot_cpu_data.x86_capability[CPUID_7_ECX] = c;
		boot_cpu_data.x86_capability[CPUID_7_EDX] = d;
	}
	cpuid(0x80000000, 0, &max, &b, &c, &d);
	if (max >= 0x80000001) {
		cpuid(0x80000001, 0, &a, &b, &c, &d);
		boot_cpu_data.x86_capability[CPUID_8000_0001_EDX] = d;
		boot_cpu_data.x86_capability[CPUID_8000_0001_ECX] = c;
	}
}

void __init setup_machinename(char *machine_out)
{
	strcpy(machine_out, "x86_64");
}

void __init setup_hostinfo(char *buf, int len)
{
	snprintf(buf, len, "l4h guest %llu", (unsigned long long)l4h_bootinfo->guest_id);
}

/* Entered from os-sel4/boot.c on vCPU 0. */
void __init l4h_linux_main(void)
{
	const struct l4h_bootinfo *bi = l4h_bootinfo;

	strscpy(command_line, bi->cmdline, sizeof(command_line));

	uml_physmem = bi->ram_base_va;
	physmem_size = bi->ram_size;
	uml_reserved = uml_physmem + bi->kernel_end_gpa;
	high_physmem = uml_physmem + physmem_size;
	end_iomem = high_physmem;

	task_size = L4H_TASK_SIZE;
	start_vm = VMALLOC_START;
	end_vm = VMALLOC_END;

	l4h_cpu_features();
	setup_machinename(init_utsname()->machine);

	l4h_uctx_init();
	arch_task_struct_size = sizeof(struct task_struct) + host_fp_size;

	start_uml();
}

void __init setup_arch(char **cmdline_p)
{
	u8 rng_seed[32];

	setup_physmem(uml_physmem, uml_reserved, physmem_size);
	l4h_caps_init();
	l4h_vspace_init();
	paging_init();
	strscpy(boot_command_line, command_line, COMMAND_LINE_SIZE);
	*cmdline_p = command_line;
	setup_hostinfo(host_info, sizeof host_info);

	if (os_getrandom(rng_seed, sizeof(rng_seed), 0) == sizeof(rng_seed)) {
		add_bootloader_randomness(rng_seed, sizeof(rng_seed));
		memzero_explicit(rng_seed, sizeof(rng_seed));
	}
}

void __init arch_cpu_finalize_init(void)
{
	arch_check_bugs();
}

void apply_seal_endbr(s32 *start, s32 *end)
{
}

void apply_retpolines(s32 *start, s32 *end)
{
}

void apply_returns(s32 *start, s32 *end)
{
}

void apply_fineibt(s32 *start_retpoline, s32 *end_retpoline,
		   s32 *start_cfi, s32 *end_cfi)
{
}

void apply_alternatives(struct alt_instr *start, struct alt_instr *end)
{
}

void *text_poke(void *addr, const void *opcode, size_t len)
{
	WARN_ON(1);
	return memcpy(addr, opcode, len);
}

void *text_poke_copy(void *addr, const void *opcode, size_t len)
{
	return text_poke(addr, opcode, len);
}

void smp_text_poke_sync_each_cpu(void)
{
}

/* ---- misc host services */

static void os_vprint(const char *prefix, const char *fmt, va_list ap)
{
	char buf[256];
	int n = vscnprintf(buf, sizeof(buf), fmt, ap);

	if (prefix)
		l4h_console_write(prefix, strlen(prefix));
	l4h_console_write(buf, n);
}

void os_info(const char *fmt, ...)
{
	va_list ap;

	va_start(ap, fmt);
	os_vprint(NULL, fmt, ap);
	va_end(ap);
}

void os_warn(const char *fmt, ...)
{
	va_list ap;

	va_start(ap, fmt);
	os_vprint("l4h: ", fmt, ap);
	va_end(ap);
}

void os_dump_core(void)
{
	l4h_power(L4H_POWER_CRASH);
}

void um_early_printk(const char *s, unsigned int n)
{
	l4h_console_write(s, n);
}

ssize_t os_getrandom(void *buf, size_t len, unsigned int flags)
{
	size_t i;

	if (!cpu_has(&boot_cpu_data, X86_FEATURE_RDRAND))
		return -ENOSYS;
	for (i = 0; i < len; i += sizeof(unsigned long)) {
		unsigned long v;
		unsigned char ok;
		int tries = 10;

		do {
			asm volatile("rdrand %0; setc %1" : "=r"(v), "=qm"(ok));
		} while (!ok && --tries);
		if (!ok)
			return i;
		memcpy(buf + i, &v, min(sizeof(v), len - i));
	}
	return len;
}
