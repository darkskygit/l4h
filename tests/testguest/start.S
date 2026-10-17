/* Entry point of the l4h test guest: rdi = boot info. */
	.section .text.start, "ax"
	.globl _start
_start:
	leaq	boot_stack_top(%rip), %rsp
	/* x86_64 TLS variant II: thread pointer at the end of the block,
	 * pointing to itself. libsel4 keeps __sel4_ipc_buffer there. */
	leaq	tls_tp(%rip), %rax
	movq	%rax, (%rax)
	wrfsbase %rax
	call	tg_main
1:	jmp	1b

/* User-mode test program, alone on its own page so it can be mapped into a
 * guest process VSpace at the same address. */
	.section .text.user, "ax"
	.balign 4096
	.globl user_prog
user_prog:
	/* xmm0 = 0x1122334455667788 to check FPU state transfer. */
	movabsq	$0x1122334455667788, %rax
	movq	%rax, %xmm0
	/* getpid(); rdx = -1 would be an seL4 syscall number if not trapped */
	movq	$39, %rax
	movq	$-1, %rdx
	syscall
	/* touch an unmapped address -> VM fault */
	movq	$0x500000, %rbx
	movq	(%rbx), %rcx
	/* exit(rax) */
	movq	%rax, %rdi
	movq	$60, %rax
	syscall
2:	jmp	2b
	.balign 4096

	.section .bss
	.balign 64
tls_block:
	.skip 256
tls_tp:
	.skip 64
	.balign 16
boot_stack:
	.skip 65536
boot_stack_top:
