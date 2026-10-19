/* SPDX-License-Identifier: GPL-2.0 */
#ifndef L4H_ARM64_SIMD_H
#define L4H_ARM64_SIMD_H

#include <linux/compiler.h>
#include <asm/neon.h>

/*
 * may_use_simd - whether kernel_neon_begin() may be called now; false in
 * a section that the current context interrupted (asm/neon.h).
 */
static __must_check inline bool may_use_simd(void)
{
	return l4h_neon_usable();
}

#endif
