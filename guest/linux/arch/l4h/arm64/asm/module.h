/* SPDX-License-Identifier: GPL-2.0 */
#ifndef L4H_ARM64_MODULE_H
#define L4H_ARM64_MODULE_H
struct mod_arch_specific {
	unsigned int plt_section;
	unsigned int plt_count;
	unsigned int plt_max;
};
#include <asm-generic/module.h>
#endif
