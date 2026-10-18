/* SPDX-License-Identifier: GPL-2.0 */
.section .head.text, "ax"
.globl _start
_start:
 ldr x1, =l4h_boot_stack_top
 mov sp, x1
 mov x19, x0
 mov x0, #0
 bl l4h_tls_setup_asm
 mov x0, x19
 bl l4h_boot
1: b 1b
.globl l4h_secondary_entry
l4h_secondary_entry:
 mov x19, x0
 bl l4h_tls_setup_asm
 mov x0, x19
 bl l4h_secondary_boot
2: b 2b
l4h_tls_setup_asm:
 ldr x1, =l4h_tls_blocks
 add x1, x1, x0, lsl #8
 msr tpidr_el0, x1
 ret
.bss
.balign 16
l4h_boot_stack:
.skip 65536
.globl l4h_boot_stack_top
l4h_boot_stack_top:
