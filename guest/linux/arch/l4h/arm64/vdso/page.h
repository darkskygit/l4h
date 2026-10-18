/* SPDX-License-Identifier: GPL-2.0 */
#include <linux/const.h>
#define PAGE_SHIFT 12
#define PAGE_SIZE (_AC(1, UL) << PAGE_SHIFT)
#define PAGE_MASK (~(PAGE_SIZE - 1))
