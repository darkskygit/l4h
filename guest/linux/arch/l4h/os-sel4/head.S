/* SPDX-License-Identifier: GPL-2.0 */
/*
 * Entry points of the l4h guest kernel.
 *
 * vCPU 0 starts at _start with %rdi = L4H_BOOTINFO_VA and no stack.
 * Secondary vCPUs are started by vCPU 0 at l4h_secondary_entry with
 * %rdi = cpu number and %rsp = an initial stack.
 *
 * Each vCPU's FS base points at its struct l4h_tls (see l4h_os.h); the
 * TLS variables of os-sel4 (libsel4's __sel4_ipc_buffer) sit just below.
 */
#include <linux/linkage.h>

#define L4H_TLS_SIZE	128	/* must match l4h_os.h */

	.section .head.text, "ax"
	.globl _start
_start:
	movabsq	$l4h_boot_stack_top, %rsp
	xorl	%esi, %esi
	call	l4h_tls_setup_asm
	movabsq	$l4h_boot, %rax
	call	*%rax
1:	hlt
	jmp	1b

	.globl l4h_secondary_entry
l4h_secondary_entry:
	movq	%rdi, %r12
	movq	%rdi, %rsi
	call	l4h_tls_setup_asm
	movq	%r12, %rdi
	movabsq	$l4h_secondary_boot, %rax
	call	*%rax
2:	hlt
	jmp	2b

/* %rsi = cpu: point FS at l4h_tls_blocks[cpu].tls, which points to itself. */
l4h_tls_setup_asm:
	movabsq	$l4h_tls_blocks, %rax
	imulq	$L4H_TLS_SIZE * 2, %rsi, %rsi
	addq	%rsi, %rax
	addq	$L4H_TLS_SIZE, %rax
	movq	%rax, (%rax)
	wrfsbase %rax
	ret

	.section .bss
	.balign 16
l4h_boot_stack:
	.skip 65536
	.globl l4h_boot_stack_top
l4h_boot_stack_top:
