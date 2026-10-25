// SPDX-License-Identifier: GPL-2.0
/*
 * What the x86-64 BPF JIT (arch/x86/net/bpf_jit_comp.c) needs from l4h:
 * memory for its images, the NOP table and code patching.
 */

#include <linux/execmem.h>
#include <linux/filter.h>
#include <linux/memory.h>
#include <linux/mm.h>
#include <linux/sizes.h>
#include <linux/stop_machine.h>
#include <linux/string.h>
#include <linux/vmalloc.h>
#include <asm/cfi.h>
#include <asm/nops.h>
#include <asm/text-patching.h>

/*
 * The JIT calls helpers in the kernel image with a rel32 call, and fails
 * the program (leaving it to the interpreter) when the target is out of
 * reach.  The kernel image starts guest RAM at PAGE_OFFSET; vmalloc and
 * modules come after all of RAM and the I/O pool, beyond 2 GiB from it in
 * a guest with more than about 1.5 GiB.  So BPF images go in the 1 GiB
 * below the kernel image, which nothing else in the guest-kernel VSpace
 * uses.  Modules keep vmalloc: they are built with the large code model.
 */
static struct execmem_info execmem_info __ro_after_init;

struct execmem_info __init *execmem_arch_setup(void)
{
	execmem_info = (struct execmem_info){
		.ranges = {
			[EXECMEM_DEFAULT] = {
				.start = VMALLOC_START,
				.end = VMALLOC_END,
				.pgprot = PAGE_KERNEL_EXEC,
				.alignment = 1,
			},
			[EXECMEM_BPF] = {
				.start = PAGE_OFFSET - SZ_1G,
				.end = PAGE_OFFSET,
				.pgprot = PAGE_KERNEL_EXEC,
				.alignment = 1,
			},
		},
	};
	return &execmem_info;
}

u64 bpf_jit_alloc_exec_limit(void)
{
	return SZ_1G;
}

/* arch/x86/kernel/alternative.c: no kCFI or FineIBT preambles. */
enum cfi_mode cfi_mode __ro_after_init = CFI_OFF;

/* arch/x86/kernel/alternative.c */
static const unsigned char x86nops[] = {
	BYTES_NOP1,
	BYTES_NOP2,
	BYTES_NOP3,
	BYTES_NOP4,
	BYTES_NOP5,
	BYTES_NOP6,
	BYTES_NOP7,
	BYTES_NOP8,
	BYTES_NOP9,
	BYTES_NOP10,
	BYTES_NOP11,
};

const unsigned char * const x86_nops[ASM_NOP_MAX + 1] = {
	NULL,
	x86nops,
	x86nops + 1,
	x86nops + 1 + 2,
	x86nops + 1 + 2 + 3,
	x86nops + 1 + 2 + 3 + 4,
	x86nops + 1 + 2 + 3 + 4 + 5,
	x86nops + 1 + 2 + 3 + 4 + 5 + 6,
	x86nops + 1 + 2 + 3 + 4 + 5 + 6 + 7,
	x86nops + 1 + 2 + 3 + 4 + 5 + 6 + 7 + 8,
	x86nops + 1 + 2 + 3 + 4 + 5 + 6 + 7 + 8 + 9,
	x86nops + 1 + 2 + 3 + 4 + 5 + 6 + 7 + 8 + 9 + 10,
};

/*
 * BPF images are mapped writable and executable, so code that no
 * CPU runs yet (a new BPF image, or the freed part of one) is written
 * directly.
 */
void *text_poke_set(void *addr, int c, size_t len)
{
	return memset(addr, c, len);
}

/*
 * Code that other vCPUs may be running (a tail call or trampoline jump):
 * x86 patches it through an int3 handler, but a breakpoint in the guest
 * kernel goes to the host.  Instead every vCPU stops with interrupts off,
 * one writes, and each then runs a serializing instruction before it
 * returns to the patched code, as the SDM asks of cross-modifying code.
 */
struct text_poke {
	void *addr;
	const void *opcode;
	size_t len;
	atomic_t cpus;
	bool written;
};

static int text_poke_stop(void *data)
{
	struct text_poke *p = data;
	unsigned int a = 0, b, c = 0, d;

	if (atomic_inc_return(&p->cpus) == 1) {
		memcpy(p->addr, p->opcode, p->len);
		smp_store_release(&p->written, true);
	} else {
		while (!smp_load_acquire(&p->written))
			cpu_relax();
	}
	/* cpuid serializes; ring 3 runs it like the kernel does. */
	asm volatile("cpuid" : "+a"(a), "=b"(b), "+c"(c), "=d"(d) : : "memory");
	return 0;
}

void smp_text_poke_single(void *addr, const void *opcode, size_t len, const void *emulate)
{
	struct text_poke p = { .addr = addr, .opcode = opcode, .len = len };

	lockdep_assert_held(&text_mutex);
	stop_machine(text_poke_stop, &p, cpu_online_mask);
}
