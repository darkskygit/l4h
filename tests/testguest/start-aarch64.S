/* Entry point of the l4h test guest on aarch64: x0 = boot info. */
	.section .text.start, "ax"
	.globl _start
_start:
	adrp	x1, boot_stack_top
	add	x1, x1, :lo12:boot_stack_top
	mov	sp, x1
	/* aarch64 TLS variant I: the thread pointer is the start of a 16-byte
	 * TCB followed by the TLS block. libsel4 keeps __sel4_ipc_buffer there. */
	adrp	x1, tls_area
	add	x1, x1, :lo12:tls_area
	msr	tpidr_el0, x1
	bl	tg_main
1:	b	1b

/* User-mode test program, alone on its own page so it can be mapped into a
 * guest process VSpace at the same address. */
	.section .text.user, "ax"
	.balign 4096
	.globl user_prog
user_prog:
	/* d0 = 0x1122334455667788 to check FPU state transfer. */
	movz	x0, #0x7788
	movk	x0, #0x5566, lsl #16
	movk	x0, #0x3344, lsl #32
	movk	x0, #0x1122, lsl #48
	fmov	d0, x0
	/* getpid(); x7 = -1 would be an seL4 syscall number if not trapped */
	mov	x8, #172
	mov	x7, #-1
	svc	#0
	/* touch an unmapped address -> VM fault */
	mov	x1, #0x500000
	ldr	x2, [x1]
	/* seL4 issue #477 on aarch64: every seL4 syscall number in x7 must
	 * still trap. */
	mov	x12, x0
	mov	x13, #-1
3:	mov	x8, #172
	mov	x7, x13
	svc	#0
	sub	x13, x13, #1
	cmn	x13, #64
	b.ge	3b
	/* exit(saved x0) */
	mov	x0, x12
	mov	x8, #93
	svc	#0
2:	b	2b

/* seL4_DebugHalt from a thread without the trap flag: must fault (patch
 * 0008), not halt the host. */
	.globl user_debug
user_debug:
	mov	x7, #-14
	svc	#0
4:	b	4b
	.balign 4096

	.section .bss
	.balign 64
tls_area:
	.skip 512
	.balign 16
boot_stack:
	.skip 65536
boot_stack_top:
