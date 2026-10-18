/* SPDX-License-Identifier: GPL-2.0 */
#include <linux/stddef.h>
#include <linux/sched.h>
#include <linux/elf.h>
#include <linux/crypto.h>
#include <linux/kbuild.h>
#include <linux/audit.h>
#include <asm/mman.h>
#include <asm/seccomp.h>
#define GDT_ENTRY_TLS_ENTRIES 0

/* workaround for a warning with -Wmissing-prototypes */
void foo(void);

void foo(void)
{
#include <common-offsets.h>
}
