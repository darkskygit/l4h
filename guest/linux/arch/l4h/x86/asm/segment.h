/* SPDX-License-Identifier: GPL-2.0 */
#ifndef __UM_SEGMENT_H
#define __UM_SEGMENT_H

extern int host_gdt_entry_tls_min;

#define GDT_ENTRY_TLS_ENTRIES 3
#define GDT_ENTRY_TLS_MIN host_gdt_entry_tls_min
#define GDT_ENTRY_TLS_MAX (GDT_ENTRY_TLS_MIN + GDT_ENTRY_TLS_ENTRIES - 1)

/*
 * The user selectors of the x86_64 Linux ABI, which ptrace and sigcontext
 * report (gdb takes CS 0x33 for a 64-bit inferior).  They are not loaded:
 * seL4's own user selectors are 0x2b (code) and 0x33 (stack), which is
 * what user code reading %cs or %ss sees.
 */
#define __USER_CS 0x33
#define __USER_DS 0x2b

#endif
