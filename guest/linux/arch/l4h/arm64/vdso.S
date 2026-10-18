/* SPDX-License-Identifier: GPL-2.0 */
.section .rodata
.balign 4096
.globl vdso_start, vdso_end
vdso_start:
.incbin "arch/l4h/arm64/vdso.so"
.zero 4096 - (. - vdso_start)
vdso_end:
