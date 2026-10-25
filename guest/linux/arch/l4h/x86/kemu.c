// SPDX-License-Identifier: GPL-2.0-only
/* kemu enters through seL4's syscall fault, like native l4h processes. */
#include <linux/highmem.h>
#include <linux/kemu.h>
#include <linux/kemu_abi.h>
#include <linux/mm.h>
#include <linux/random.h>
#include <linux/sched.h>
#include <linux/timekeeping.h>
#include <asm/syscall.h>
#include <asm/unistd.h>

void kemu_svc(struct pt_regs *regs);
void kemu_op(struct pt_regs *regs, unsigned long op);

bool kemu_do_syscall(struct pt_regs *regs, int nr)
{
	if ((unsigned int)nr == KEMU_OP_SVC) {
		kemu_svc(regs);
		syscall_trace_leave(regs);
	} else {
		PT_REGS_SYSCALL_NR(regs) = -1;
		kemu_op(regs, (unsigned int)nr);
	}
	return false;
}

/* Use the native table, including its sys_ni_syscall entries for disabled
 * features, so the guest ABI adapters need no duplicate config guards. */
#define KEMU_SYSCALL(nr, sym) \
	asmlinkage long __x64_##sym(const struct pt_regs *regs); \
	asmlinkage long __x64_##sym(const struct pt_regs *regs) \
	{ \
		return sys_call_table[nr](PT_REGS_DI(regs), PT_REGS_SI(regs), \
			PT_REGS_DX(regs), PT_REGS_R10(regs), PT_REGS_R8(regs), \
			PT_REGS_R9(regs)); \
	}
#include "kemu-syscalls.h"

/* The generic 64-bit ABI uses this name for x86_64's fadvise64. */
KEMU_SYSCALL(__NR_fadvise64, sys_fadvise64_64)

/*
 * User memory is in a separate seL4 VSpace: pin and map the page before
 * doing an atomic operation. Natural alignment keeps it within one page.
 */
int kemu_arch_cmpxchg(u64 addr, u32 size, const u64 *exp, const u64 *new, u64 *old)
{
	struct page *page;
	void *base, *p;
	int ret;

	ret = get_user_pages_fast(addr & PAGE_MASK, 1, FOLL_WRITE, &page);
	if (ret != 1)
		return -EFAULT;
	base = kmap_local_page(page);
	p = base + (addr & ~PAGE_MASK);
	switch (size) {
	case 1:
		old[0] = cmpxchg((u8 *)p, (u8)exp[0], (u8)new[0]);
		break;
	case 2:
		old[0] = cmpxchg((u16 *)p, (u16)exp[0], (u16)new[0]);
		break;
	case 4:
		old[0] = cmpxchg((u32 *)p, (u32)exp[0], (u32)new[0]);
		break;
	case 8:
		old[0] = cmpxchg((u64 *)p, exp[0], new[0]);
		break;
	case 16: {
		u128 e = ((u128)exp[1] << 64) | exp[0];
		u128 n = ((u128)new[1] << 64) | new[0];

		try_cmpxchg128((u128 *)p, &e, n);
		old[0] = (u64)e;
		old[1] = e >> 64;
		break;
	}
	}
	kunmap_local(base);
	set_page_dirty_lock(page);
	put_page(page);
	return 0;
}

/* Expose the guest kernel's monotonic clock with a 1 GHz counter. */
u64 kemu_arch_cntvct(void)
{
	return ktime_get_ns();
}

u64 kemu_arch_cntfrq(void)
{
	return NSEC_PER_SEC;
}

bool kemu_arch_random(u64 *out)
{
	get_random_bytes(out, sizeof(*out));
	return true;
}
