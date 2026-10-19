/* SPDX-License-Identifier: GPL-2.0-only */
/*
 * The subset of arch/arm64/include/asm/assembler.h that the arm64 crypto
 * code (arch/arm64/crypto) uses.  The upstream header pulls in asm-offsets
 * and arm64 kernel internals that l4h does not have.
 *
 * Copyright (C) 1996-2000 Russell King
 * Copyright (C) 2012 ARM Ltd.
 */
#ifndef __ASSEMBLY__
#error "Only include this from assembly code"
#endif

#ifndef L4H_ARM64_ASSEMBLER_H
#define L4H_ARM64_ASSEMBLER_H

/* l4h guests are little-endian. */
#define CPU_BE(code...)
#define CPU_LE(code...) code

	/*
	 * Symbol addresses, PC-relative (+/- 4 GiB): fine for built-in code,
	 * which is where l4h links these algorithms.
	 */
	.macro	adr_l, dst, sym
	adrp	\dst, \sym
	add	\dst, \dst, :lo12:\sym
	.endm

	.macro	ldr_l, dst, sym, tmp=
	.ifb	\tmp
	adrp	\dst, \sym
	ldr	\dst, [\dst, :lo12:\sym]
	.else
	adrp	\tmp, \sym
	ldr	\dst, [\tmp, :lo12:\sym]
	.endif
	.endm

	.macro	str_l, src, sym, tmp
	adrp	\tmp, \sym
	str	\src, [\tmp, :lo12:\sym]
	.endm

	.macro	mov_q, reg, val
	.if (((\val) >> 31) == 0 || ((\val) >> 31) == 0x1ffffffff)
	movz	\reg, :abs_g1_s:\val
	.else
	.if (((\val) >> 47) == 0 || ((\val) >> 47) == 0x1ffff)
	movz	\reg, :abs_g2_s:\val
	.else
	movz	\reg, :abs_g3:\val
	movk	\reg, :abs_g2_nc:\val
	.endif
	movk	\reg, :abs_g1_nc:\val
	.endif
	movk	\reg, :abs_g0_nc:\val
	.endm

	/*
	 * frame_push - Push @regcount callee saved registers to the stack,
	 *              starting at x19, as well as x29/x30, and set x29 to
	 *              the new value of sp. Add @extra bytes of stack space
	 *              for locals.
	 */
	.macro		frame_push, regcount:req, extra
	__frame		st, \regcount, \extra
	.endm

	/* frame_pop - Undo the most recent frame_push. */
	.macro		frame_pop
	__frame		ld
	.endm

	.macro		__frame_regs, reg1, reg2, op, num
	.if		.Lframe_regcount == \num
	\op\()r		\reg1, [sp, #(\num + 1) * 8]
	.elseif		.Lframe_regcount > \num
	\op\()p		\reg1, \reg2, [sp, #(\num + 1) * 8]
	.endif
	.endm

	.macro		__frame, op, regcount, extra=0
	.ifc		\op, st
	.if		(\regcount) < 0 || (\regcount) > 10
	.error		"regcount should be in the range [0 ... 10]"
	.endif
	.if		((\extra) % 16) != 0
	.error		"extra should be a multiple of 16 bytes"
	.endif
	.ifdef		.Lframe_regcount
	.if		.Lframe_regcount != -1
	.error		"frame_push/frame_pop may not be nested"
	.endif
	.endif
	.set		.Lframe_regcount, \regcount
	.set		.Lframe_extra, \extra
	.set		.Lframe_local_offset, ((\regcount + 3) / 2) * 16
	stp		x29, x30, [sp, #-.Lframe_local_offset - .Lframe_extra]!
	mov		x29, sp
	.endif

	__frame_regs	x19, x20, \op, 1
	__frame_regs	x21, x22, \op, 3
	__frame_regs	x23, x24, \op, 5
	__frame_regs	x25, x26, \op, 7
	__frame_regs	x27, x28, \op, 9

	.ifc		\op, ld
	.if		.Lframe_regcount == -1
	.error		"frame_push/frame_pop may not be nested"
	.endif
	ldp		x29, x30, [sp], #.Lframe_local_offset + .Lframe_extra
	.set		.Lframe_regcount, -1
	.endif
	.endm

	/*
	 * Whether asm code should return early so its caller can reschedule.
	 * Never on l4h: the guest kernel polls for events, and the C callers
	 * already end their NEON sections between chunks of work.
	 */
	.macro		cond_yield, lbl:req, tmp:req, tmp2
	.endm

#endif
