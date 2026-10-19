/* SPDX-License-Identifier: GPL-2.0 */
#ifndef L4H_ARM64_CPUFEATURE_H
#define L4H_ARM64_CPUFEATURE_H
#include <linux/log2.h>
#include <asm/hwcap.h>
#include <asm/processor.h>

/*
 * For <linux/cpufeature.h> (module_cpu_feature_match): CPU features are
 * the HWCAP_* bits of AT_HWCAP, set from the ID registers in bootinfo
 * (arch/l4h/kernel/um_arch.c).
 */
extern long elf_aux_hwcap;

#define MAX_CPU_FEATURES	64
#define cpu_feature(x)		const_ilog2(HWCAP_ ## x)
#define cpu_have_named_feature(x) cpu_have_feature(cpu_feature(x))

static inline bool cpu_have_feature(unsigned int num)
{
	return num < MAX_CPU_FEATURES && (elf_aux_hwcap & (1UL << num));
}

#endif
