/* SPDX-License-Identifier: GPL-2.0 */
/* Private definitions of arch/l4h/os-sel4 (built against libsel4). */
#ifndef __L4H_OS_H
#define __L4H_OS_H

#include <stdint.h>
#include <stddef.h>
#include <sel4/sel4.h>
#include <l4h_sys.h>

/* Per-vCPU thread-local block.  FS base points at `tls`; the 128 bytes
 * before it hold the static TLS segment (libsel4's __sel4_ipc_buffer). */
#define L4H_TLS_SIZE 128

struct l4h_tls {
	struct l4h_tls *self;		/* x86_64 ABI: %fs:0 == TP */
	struct l4h_vcpu *vcpu;
	long cpu;
	void *current;			/* struct task_struct * */
	char pad[L4H_TLS_SIZE - 32];
};

struct l4h_tls_block {
	char tbss[L4H_TLS_SIZE];
	struct l4h_tls tls;
} __attribute__((aligned(64)));

extern struct l4h_tls_block l4h_tls_blocks[L4H_MAX_VCPUS];

static inline struct l4h_tls *l4h_tls(void)
{
	struct l4h_tls *t;
	__asm__("movq %%fs:0, %0" : "=r"(t));
	return t;
}

#define VCPU_CAP(v, which) L4H_CPTR(L4H_SLOT_VCPU(v) + (which))

/* Kernel functions callable from here. */
extern void l4h_linux_main(void);
extern void l4h_secondary_main(int cpu);
extern int _printk(const char *fmt, ...);
#define printk _printk
extern void panic(const char *fmt, ...) __attribute__((noreturn));

#define l4h_BUG_ON(c) do { if (__builtin_expect(!!(c), 0)) panic("l4h: %s:%d: %s\n", __FILE__, __LINE__, #c); } while (0)

void *memcpy(void *d, const void *s, size_t n);
void *memset(void *d, int c, size_t n);

#endif
