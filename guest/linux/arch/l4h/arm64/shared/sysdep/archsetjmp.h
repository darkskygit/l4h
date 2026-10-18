/* SPDX-License-Identifier: GPL-2.0 */
#ifndef L4H_ARM64_SETJMP_H
#define L4H_ARM64_SETJMP_H
struct __jmp_buf {
	unsigned long x19[10], fp, ip, sp;
};
typedef struct __jmp_buf jmp_buf[1];
#define JB_IP ip
#define JB_SP sp
#define JB_BP fp
unsigned long get_thread_reg(int reg, jmp_buf *buf);
#endif
