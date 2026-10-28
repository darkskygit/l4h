// SPDX-License-Identifier: GPL-2.0
/*
 * Thin wrappers around seL4 invocations, so that the rest of the kernel does
 * not need libsel4.  All capability arguments are CPtrs in the guest CSpace
 * (see l4h/abi.h).
 */
#include "l4h_os.h"

__thread seL4_IPCBuffer *__sel4_ipc_buffer;

struct l4h_tls_block l4h_tls_blocks[L4H_MAX_VCPUS];
struct l4h_vcpu l4h_vcpus[L4H_MAX_VCPUS];
const struct l4h_bootinfo *l4h_bootinfo;
unsigned long long l4h_tsc_hz;

struct l4h_vcpu *l4h_this_vcpu(void)
{
	return l4h_tls()->vcpu;
}

int l4h_this_vcpu_id(void)
{
	return (int)l4h_tls()->cpu;
}

static const seL4_Word obj_type[L4H_OBJ_NR] = {
	[L4H_OBJ_UNTYPED] = seL4_UntypedObject,
	[L4H_OBJ_TCB] = seL4_TCBObject,
	[L4H_OBJ_ENDPOINT] = seL4_EndpointObject,
	[L4H_OBJ_NOTIFICATION] = seL4_NotificationObject,
	[L4H_OBJ_CNODE] = seL4_CapTableObject,
	[L4H_OBJ_SCHED_CONTEXT] = seL4_SchedContextObject,
	[L4H_OBJ_REPLY] = seL4_ReplyObject,
#ifdef __aarch64__
	[L4H_OBJ_FRAME] = seL4_ARM_SmallPageObject,
	[L4H_OBJ_PT] = seL4_ARM_PageTableObject,
	[L4H_OBJ_PD] = seL4_ARM_PageTableObject,
	[L4H_OBJ_PDPT] = seL4_ARM_PageTableObject,
#else
	[L4H_OBJ_FRAME] = seL4_X86_4K,
	[L4H_OBJ_PT] = seL4_X86_PageTableObject,
	[L4H_OBJ_PD] = seL4_X86_PageDirectoryObject,
	[L4H_OBJ_PDPT] = seL4_X86_PDPTObject,
#endif
};

int l4h_obj_bits(int type, int size_bits)
{
	switch (type) {
	case L4H_OBJ_UNTYPED:
		return size_bits;
	case L4H_OBJ_TCB:
		return seL4_TCBBits;
	case L4H_OBJ_ENDPOINT:
		return seL4_EndpointBits;
	case L4H_OBJ_NOTIFICATION:
		return seL4_NotificationBits;
	case L4H_OBJ_CNODE:
		return size_bits + seL4_SlotBits;
	case L4H_OBJ_SCHED_CONTEXT:
		return size_bits;
	case L4H_OBJ_REPLY:
		return seL4_ReplyBits;
	case L4H_OBJ_FRAME:
	case L4H_OBJ_PT:
	case L4H_OBJ_PD:
	case L4H_OBJ_PDPT:
		return seL4_PageBits;
	}
	return -1;
}

const int l4h_pt_level_obj[L4H_NR_PT_LEVELS] = { L4H_OBJ_PT, L4H_OBJ_PD, L4H_OBJ_PDPT };
const int l4h_pt_level_shift[L4H_NR_PT_LEVELS] = { 21, 30, 39 };

int l4h_retype(unsigned long ut, int type, int size_bits, unsigned long node_index,
	       int node_depth, unsigned long offset, unsigned long n)
{
	return seL4_Untyped_Retype(ut, obj_type[type], size_bits, L4H_CNODE_CPTR, node_index,
				   node_depth, offset, n);
}

static seL4_CapRights_t rights(int r)
{
	return seL4_CapRights_new(!!(r & L4H_RIGHT_GRANT_REPLY), !!(r & L4H_RIGHT_GRANT),
				  !!(r & L4H_RIGHT_READ), !!(r & L4H_RIGHT_WRITE));
}

int l4h_cnode_copy(unsigned long dst, int dst_depth, unsigned long src, int src_depth, int r)
{
	return seL4_CNode_Copy(L4H_CNODE_CPTR, dst, dst_depth, L4H_CNODE_CPTR, src, src_depth,
			       rights(r));
}

int l4h_cnode_mint(unsigned long dst, int dst_depth, unsigned long src, int src_depth, int r,
		   unsigned long badge)
{
	return seL4_CNode_Mint(L4H_CNODE_CPTR, dst, dst_depth, L4H_CNODE_CPTR, src, src_depth,
			       rights(r), badge);
}

int l4h_cnode_mutate(unsigned long dst, int dst_depth, unsigned long src, int src_depth,
		     unsigned long data)
{
	return seL4_CNode_Mutate(L4H_CNODE_CPTR, dst, dst_depth, L4H_CNODE_CPTR, src, src_depth,
				 data);
}

int l4h_cnode_delete(unsigned long cap, int depth)
{
	return seL4_CNode_Delete(L4H_CNODE_CPTR, cap, depth);
}

int l4h_cnode_revoke(unsigned long cap, int depth)
{
	return seL4_CNode_Revoke(L4H_CNODE_CPTR, cap, depth);
}

int l4h_page_map(unsigned long frame, unsigned long vspace, unsigned long va, int prot)
{
	seL4_CapRights_t r = (prot & L4H_PROT_WRITE) ? seL4_ReadWrite :
			     (prot & (L4H_PROT_READ | L4H_PROT_EXEC)) ? seL4_CanRead : seL4_NoRights;
	/* Readable pages are executable unless marked execute-never (seL4
	 * patch 0009); there is no execute-only. */
#ifdef __aarch64__
	int err = seL4_ARM_Page_Map(frame, vspace, va, r, seL4_ARM_Default_VMAttributes |
		((prot & L4H_PROT_EXEC) ? 0 : seL4_ARM_ExecuteNever));
	if (!err && (prot & L4H_PROT_EXEC))
		err = seL4_ARM_Page_Unify_Instruction(frame, 0, 4096);
	return err;
#else
	seL4_X86_VMAttributes attr = (prot & L4H_PROT_EXEC) ? seL4_X86_Default_VMAttributes :
				     seL4_X86_ExecuteNever;

	if (prot & L4H_PROT_DEFER_FLUSH)
		attr |= seL4_X86_DeferFlush;
	return seL4_X86_Page_Map(frame, vspace, va, r, attr);
#endif
}

int l4h_page_unmap(unsigned long frame)
{
#ifdef __aarch64__
	return seL4_ARM_Page_Unmap(frame);
#else
	return seL4_X86_Page_Unmap(frame);
#endif
}

int l4h_page_test_and_clear_accessed(unsigned long frame, int *accessed)
{
#ifdef __aarch64__
	seL4_ARM_Page_TestAndClearAccessed_t ret = seL4_ARM_Page_TestAndClearAccessed(frame);
#else
	seL4_X86_Page_TestAndClearAccessed_t ret = seL4_X86_Page_TestAndClearAccessed(frame);
#endif
	*accessed = ret.error ? 0 : ret.accessed;
	return ret.error;
}

/* Complete the maps made with L4H_PROT_DEFER_FLUSH in `vspace`. */
int l4h_vspace_flush(unsigned long vspace)
{
#ifdef __aarch64__
	return 0;
#else
	return seL4_X64_PML4_Flush(vspace);
#endif
}

int l4h_pt_map(int type, unsigned long table, unsigned long vspace, unsigned long va)
{
#ifdef __aarch64__
	return seL4_ARM_PageTable_Map(table, vspace, va, seL4_ARM_Default_VMAttributes);
#else
	switch (type) {
	case L4H_OBJ_PT:
		return seL4_X86_PageTable_Map(table, vspace, va, seL4_X86_Default_VMAttributes);
	case L4H_OBJ_PD:
		return seL4_X86_PageDirectory_Map(table, vspace, va, seL4_X86_Default_VMAttributes);
	case L4H_OBJ_PDPT:
		return seL4_X86_PDPT_Map(table, vspace, va, seL4_X86_Default_VMAttributes);
	}
	return seL4_InvalidArgument;
#endif
}

int l4h_tcb_configure_user(unsigned long tcb, unsigned long cspace, unsigned long vspace)
{
	return seL4_TCB_Configure(tcb, cspace, 0, vspace, 0, 0, seL4_CapNull);
}

/* On MCS kernels SetSpace also (re)installs the fault handler. */
int l4h_tcb_set_space(unsigned long tcb, unsigned long fault_ep, unsigned long cspace,
		      unsigned long vspace)
{
	return seL4_TCB_SetSpace(tcb, fault_ep, cspace, 0, vspace, 0);
}

int l4h_tcb_sched(unsigned long tcb, unsigned long authority, int prio, unsigned long sc,
		  unsigned long fault_ep)
{
	return seL4_TCB_SetSchedParams(tcb, authority, prio, prio, sc, fault_ep);
}

int l4h_tcb_set_flags(unsigned long tcb, unsigned long clear, unsigned long set)
{
	seL4_TCB_SetFlags_t r = seL4_TCB_SetFlags(tcb, clear, set);

	if (r.error)
		return r.error;
	/* The kernel ignores flags it does not support (trapSyscalls with the
	 * x86 SKIM window). */
	return (r.flags & set) == set ? 0 : seL4_IllegalOperation;
}

int l4h_tcb_resume(unsigned long tcb)
{
	return seL4_TCB_Resume(tcb);
}

int l4h_tcb_suspend(unsigned long tcb)
{
	return seL4_TCB_Suspend(tcb);
}

int l4h_tcb_bind_notification(unsigned long tcb, unsigned long ntfn)
{
	return seL4_TCB_BindNotification(tcb, ntfn);
}

int l4h_sc_unbind(unsigned long sc)
{
	return seL4_SchedContext_Unbind(sc);
}

void l4h_signal(unsigned long ntfn)
{
	seL4_Signal(ntfn);
}

void l4h_yield(void)
{
	seL4_Yield();
}

unsigned long l4h_hypercall(unsigned long label, unsigned long *mrs, int n_in, int n_out)
{
	seL4_MessageInfo_t info;
	int i;

	for (i = 0; i < n_in; i++)
		seL4_SetMR(i, mrs[i]);
	info = seL4_Call(L4H_CPTR(L4H_SLOT_HYP_EP), seL4_MessageInfo_new(label, 0, 0, n_in));
	for (i = 0; i < n_out && i < (int)seL4_MessageInfo_get_length(info); i++)
		mrs[i] = seL4_GetMR(i);
	return seL4_MessageInfo_get_length(info) ? seL4_GetMR(0) : 0;
}

static void console_hypercall(const char *s, unsigned int len)
{
	unsigned long mrs[1 + L4H_HC_CONSOLE_MAX_WORDS];

	while (len) {
		unsigned int n = len < 8 * L4H_HC_CONSOLE_MAX_WORDS ? len : 8 * L4H_HC_CONSOLE_MAX_WORDS;
		unsigned int w;

		mrs[0] = n;
		for (w = 0; w < (n + 7) / 8; w++) {
			unsigned long v = 0;
			unsigned int b;

			for (b = 0; b < 8 && w * 8 + b < n; b++)
				v |= (unsigned long)(unsigned char)s[w * 8 + b] << (8 * b);
			mrs[1 + w] = v;
		}
		l4h_hypercall(L4H_HC_CONSOLE_WRITE, mrs, 1 + (n + 7) / 8, 0);
		s += n;
		len -= n;
	}
}

/* The console output ring (L4H_DEV_CONSOLE_OUT), if the host gives one. */
static struct l4h_console_out_page *console_out;
static unsigned long console_out_kick;
static int console_out_looked;
/* Taken with a trylock: a writer that finds it held (another vCPU, or a
 * crash on this one) uses the hypercall, which never waits for it. */
static int console_out_lock;

static void console_find_out(void)
{
	const struct l4h_bootinfo *bi = l4h_bootinfo;
	unsigned long i;

	if (!bi)
		return;
	console_out_looked = 1;
	if (bi->magic != L4H_BOOTINFO_MAGIC || bi->abi_version != L4H_ABI_VERSION)
		return;
	for (i = 0; i < bi->nr_devices && i < L4H_MAX_DEVICES; i++) {
		const struct l4h_dev_desc *d = &bi->devices[i];

		if (d->type == L4H_DEV_CONSOLE_OUT && d->page_size >= sizeof(*console_out)) {
			console_out_kick = L4H_CPTR2(L4H_SLOT_DEV_CNODE, d->kick_slot);
			console_out = (struct l4h_console_out_page *)d->page_va;
		}
	}
}

/* Put as much of s in the ring as fits; returns how much. */
static unsigned int console_ring_write(const char *s, unsigned int len)
{
	struct l4h_console_out_page *p = console_out;
	unsigned int head = p->head;
	/* The host is done with the bytes before tail. */
	unsigned int used = head - __atomic_load_n(&p->tail, __ATOMIC_ACQUIRE);
	unsigned int n, i;

	n = used < L4H_CONSOLE_OUT_SIZE ? L4H_CONSOLE_OUT_SIZE - used : 0;
	if (n > len)
		n = len;
	if (!n)
		return 0;
	for (i = 0; i < n; i++)
		p->out[(head + i) % L4H_CONSOLE_OUT_SIZE] = s[i];
	__atomic_store_n(&p->head, head + n, __ATOMIC_RELEASE);
	/* Kick only a host that caught up: one still copying looks at head
	 * again after publishing its tail. */
	__atomic_thread_fence(__ATOMIC_SEQ_CST);
	if (__atomic_load_n(&p->tail, __ATOMIC_RELAXED) == head)
		seL4_Signal(console_out_kick);
	return n;
}

void l4h_console_write(const char *s, unsigned int len)
{
	unsigned int n = 0;

	if (!console_out_looked)
		console_find_out();
	if (console_out && !__atomic_exchange_n(&console_out_lock, 1, __ATOMIC_ACQUIRE)) {
		n = console_ring_write(s, len);
		__atomic_store_n(&console_out_lock, 0, __ATOMIC_RELEASE);
	}
	/* The host drains the ring before it prints what the hypercall
	 * carries. */
	if (n < len)
		console_hypercall(s + n, len - n);
}

void l4h_power(int how)
{
	unsigned long mr = how;

	l4h_hypercall(L4H_HC_POWER, &mr, 1, 0);
	for (;;)
		seL4_Yield();
}

/* libsel4 debug assertions */
void __assert_fail(const char *assertion, const char *file, int line, const char *function)
{
	panic("libsel4 assertion failed: %s (%s:%d %s)\n", assertion, file, line, function);
}

#ifdef __aarch64__
void l4h_flush_icache(unsigned long start, unsigned long end)
{
	/* seL4's VSpace flush may not cross a page boundary. Module text
	 * can span many pages, with an unaligned start or end. */
	while (start < end) {
		unsigned long len = (1UL << seL4_PageBits) -
			(start & ((1UL << seL4_PageBits) - 1));

		if (len > end - start)
			len = end - start;
		l4h_BUG_ON(seL4_ARM_VSpace_Unify_Instruction(L4H_CPTR(L4H_SLOT_VSPACE), start, start + len));
		start += len;
	}
}
#endif
