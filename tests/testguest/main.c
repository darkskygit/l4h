/*
 * l4h test guest: a freestanding "guest kernel" that exercises the l4h
 * guest ABI and the seL4 extensions E1-E3 end to end:
 *   - hypercall console output,
 *   - a user thread with seL4_TCBFlag_trapSyscalls in a pool VSpace,
 *     whose Linux-style syscalls and page faults arrive as fault IPC,
 *   - seL4_TCB_ReadFPU of the user thread's xmm0,
 *   - seL4 debug syscalls refused to guest threads (an unknown syscall),
 *   - seL4_X86_ExecuteNever mappings,
 *   - seL4_TCB_SetTimer delivering a timer badge.
 * It prints "TESTGUEST: PASS" or "TESTGUEST: FAIL ..." and powers off.
 */
#include <stdint.h>
#include <stddef.h>
#include <sel4/sel4.h>
#include <l4h/abi.h>

__thread seL4_IPCBuffer *__sel4_ipc_buffer;

static const struct l4h_bootinfo *bi;

/* The compiler may emit calls to these. */
void *memset(void *d, int c, size_t n)
{
	unsigned char *p = d;
	while (n--)
		*p++ = (unsigned char)c;
	return d;
}

static size_t slen(const char *s)
{
	size_t n = 0;
	while (s[n])
		n++;
	return n;
}

static void putsn(const char *s, size_t len)
{
	while (len) {
		size_t n = len < 8 * L4H_HC_CONSOLE_MAX_WORDS ? len : 8 * L4H_HC_CONSOLE_MAX_WORDS;
		seL4_SetMR(0, n);
		for (size_t w = 0; w < (n + 7) / 8; w++) {
			seL4_Word v = 0;
			for (size_t b = 0; b < 8 && w * 8 + b < n; b++)
				v |= (seL4_Word)(uint8_t)s[w * 8 + b] << (8 * b);
			seL4_SetMR(1 + w, v);
		}
		seL4_Call(L4H_CPTR(L4H_SLOT_HYP_EP),
			  seL4_MessageInfo_new(L4H_HC_CONSOLE_WRITE, 0, 0, 1 + (n + 7) / 8));
		s += n;
		len -= n;
	}
}

static void puts(const char *s)
{
	putsn(s, slen(s));
}

static void puthex(const char *label, uint64_t v)
{
	char buf[64];
	size_t n = 0;
	const char *hex = "0123456789abcdef";
	while (*label)
		buf[n++] = *label++;
	buf[n++] = '0';
	buf[n++] = 'x';
	for (int i = 60; i >= 0; i -= 4)
		buf[n++] = hex[(v >> i) & 0xf];
	buf[n++] = '\n';
	putsn(buf, n);
}

static void power(uint64_t how)
{
	seL4_SetMR(0, how);
	seL4_Call(L4H_CPTR(L4H_SLOT_HYP_EP), seL4_MessageInfo_new(L4H_HC_POWER, 0, 0, 1));
	for (;;)
		;
}

static void fail(const char *what, uint64_t v)
{
	puts("TESTGUEST: FAIL ");
	puthex(what, v);
	power(L4H_POWER_CRASH);
}

#define CHECK(cond, what, v) do { if (!(cond)) fail(what, (uint64_t)(v)); } while (0)

/* The time counter seL4 deadlines are in: TSC / virtual counter. */
static inline uint64_t rdtsc(void)
{
#ifdef __aarch64__
	uint64_t v;
	__asm__ volatile("isb; mrs %0, cntvct_el0" : "=r"(v));
	return v;
#else
	uint32_t lo, hi;
	__asm__ volatile("rdtsc" : "=a"(lo), "=d"(hi));
	return ((uint64_t)hi << 32) | lo;
#endif
}

/* Architecture differences of the test. */
#ifdef __aarch64__
#define SYS_getpid		172
#define SYS_exit		93
/* Linux syscall number (x8; patch 0007) and the seL4 one (x7). */
#define MR_SYSNO		seL4_UnknownSyscall_Syscall
#define MR_SEL4NO		seL4_UnknownSyscall_X7
#define MR_RET			seL4_UnknownSyscall_X0
#define MR_ARG0			seL4_UnknownSyscall_X0
#define MR_COUNT		seL4_UnknownSyscall_Length
#define SYSCALL_INSN_LEN	4
#define FPU_V0_OFFSET		0
#else
#define SYS_getpid		39
#define SYS_exit		60
#define MR_SYSNO		seL4_UnknownSyscall_RAX
#define MR_SEL4NO		seL4_UnknownSyscall_RDX
#define MR_RET			seL4_UnknownSyscall_RAX
#define MR_ARG0			seL4_UnknownSyscall_RDI
#define MR_COUNT		seL4_UnknownSyscall_Syscall
#define SYSCALL_INSN_LEN	2
#define FPU_V0_OFFSET		160
#endif

/* ---- object allocation from the untyped quota into dynamic root slots */
static seL4_Word next_slot = L4H_SLOT_DYN_BASE;

static seL4_CPtr alloc_obj(seL4_Word type, seL4_Word size_bits)
{
	seL4_Word slot = next_slot++;
	int err = seL4_Untyped_Retype(L4H_CPTR(L4H_SLOT_UNTYPED_BASE), type, size_bits,
				      L4H_CNODE_CPTR, 0, 0, slot, 1);
	CHECK(err == 0, "retype err=", err);
	return L4H_CPTR(slot);
}

/* Copy a capability into a fresh dynamic root slot. */
static seL4_CPtr copy_cap(seL4_CPtr src, seL4_CapRights_t rights)
{
	seL4_Word slot = next_slot++;
	int err = seL4_CNode_Copy(L4H_CNODE_CPTR, slot, L4H_ROOT_DEPTH,
				  L4H_CNODE_CPTR, src, 64, rights);
	CHECK(err == 0, "copy err=", err);
	return L4H_CPTR(slot);
}

/* Mint a badged copy of the capability in root slot `src_slot`. */
static seL4_CPtr mint_cap(seL4_Word src_slot, seL4_CapRights_t rights, seL4_Word badge)
{
	seL4_Word slot = next_slot++;
	int err = seL4_CNode_Mint(L4H_CNODE_CPTR, slot, L4H_ROOT_DEPTH,
				  L4H_CNODE_CPTR, src_slot, L4H_ROOT_DEPTH, rights, badge);
	CHECK(err == 0, "mint err=", err);
	return L4H_CPTR(slot);
}

#ifdef __aarch64__
static void map_user_page(seL4_CPtr vspace, seL4_CPtr frame, seL4_Word va, seL4_CapRights_t rights)
{
	for (int tries = 0; tries < 5; tries++) {
		int err = seL4_ARM_Page_Map(frame, vspace, va, rights, seL4_ARM_Default_VMAttributes);
		if (err == 0)
			return;
		CHECK(err == seL4_FailedLookup, "page map err=", err);
		/* A page table goes in at the first missing level. */
		err = seL4_ARM_PageTable_Map(alloc_obj(seL4_ARM_PageTableObject, 0), vspace, va,
					     seL4_ARM_Default_VMAttributes);
		CHECK(err == 0, "pt map err=", err);
	}
	fail("map_user_page va=", va);
}
#else
static void map_user_page(seL4_CPtr vspace, seL4_CPtr frame, seL4_Word va, seL4_CapRights_t rights)
{
	for (int tries = 0; tries < 4; tries++) {
		int err = seL4_X86_Page_Map(frame, vspace, va, rights, seL4_X86_Default_VMAttributes);
		if (err == 0)
			return;
		CHECK(err == seL4_FailedLookup, "page map err=", err);
		err = seL4_X86_PageTable_Map(alloc_obj(seL4_X86_PageTableObject, 0), vspace, va,
					     seL4_X86_Default_VMAttributes);
		if (err == seL4_FailedLookup) {
			err = seL4_X86_PageDirectory_Map(alloc_obj(seL4_X86_PageDirectoryObject, 0), vspace,
							 va, seL4_X86_Default_VMAttributes);
			if (err == seL4_FailedLookup) {
				err = seL4_X86_PDPT_Map(alloc_obj(seL4_X86_PDPTObject, 0), vspace, va,
							seL4_X86_Default_VMAttributes);
				CHECK(err == 0, "pdpt map err=", err);
			}
		}
	}
	fail("map_user_page va=", va);
}
#endif

extern char user_prog[], user_debug[];

void tg_main(const struct l4h_bootinfo *bootinfo)
{
	bi = bootinfo;
	__sel4_ipc_buffer = (seL4_IPCBuffer *)L4H_VCPU_IPCBUF_VA(0);
#ifdef __aarch64__
	puts("testguest: aarch64\n");
#endif
	puts("testguest: hello from the guest kernel\n");
	CHECK(bi->magic == L4H_BOOTINFO_MAGIC, "magic=", bi->magic);
	CHECK(bi->abi_version == L4H_ABI_VERSION, "abi=", bi->abi_version);
	puthex("testguest: ram_size=", bi->ram_size);
	puthex("testguest: tsc_hz=", bi->tsc_hz);

	const seL4_CPtr vcpu = L4H_SLOT_VCPU(0);
	const seL4_CPtr ktcb = L4H_CPTR(vcpu + L4H_VCPU_KTCB);
	const seL4_CPtr ep = L4H_CPTR(vcpu + L4H_VCPU_EP);

	/* User thread in pool VSpace 0, running user_prog mapped at its own VA. */
	seL4_CPtr vspace = L4H_CPTR2(L4H_SLOT_VSPACE_POOL, 0);
	uint64_t prog_gpa = (uint64_t)user_prog - bi->ram_base_va;
	seL4_CPtr prog_frame = copy_cap(L4H_FRAME_CPTR(prog_gpa), seL4_AllRights);
	map_user_page(vspace, prog_frame, (seL4_Word)user_prog, seL4_CanRead);

	int err;
	seL4_CPtr utcb = alloc_obj(seL4_TCBObject, 0);
	seL4_CPtr reply = alloc_obj(seL4_ReplyObject, 0);
	seL4_CPtr fault_ep = mint_cap(vcpu + L4H_VCPU_EP, seL4_AllRights, L4H_EP_BADGE_TASK | 1);
	/* User threads get an empty CSpace; with trapSyscalls it is never used. */
	err = seL4_Untyped_Retype(L4H_CPTR(L4H_SLOT_UNTYPED_BASE), seL4_CapTableObject, 1,
				  L4H_CNODE_CPTR, L4H_SLOT_META, L4H_ROOT_DEPTH, L4H_META_DYN_BASE, 1);
	CHECK(err == 0, "empty cnode err=", err);
	seL4_CPtr empty_cnode = L4H_CPTR2(L4H_SLOT_META, L4H_META_DYN_BASE);
	err = seL4_TCB_Configure(utcb, empty_cnode, 0, vspace, 0, 0, seL4_CapNull);
	CHECK(err == 0, "utcb configure err=", err);
	err = seL4_TCB_SetSchedParams(utcb, ktcb, L4H_PRIO_USER, L4H_PRIO_USER,
				      L4H_CPTR(vcpu + L4H_VCPU_SC_U), fault_ep);
	CHECK(err == 0, "utcb sched err=", err);
	seL4_TCB_SetFlags_t fl = seL4_TCB_SetFlags(utcb, seL4_TCBFlag_fpuDisabled, seL4_TCBFlag_trapSyscalls);
	CHECK(fl.error == 0 && fl.flags == seL4_TCBFlag_trapSyscalls, "setflags=", fl.flags);

	seL4_UserContext regs = { 0 };
#ifdef __aarch64__
	regs.pc = (seL4_Word)user_prog;
	regs.sp = 0x7f0000000000;
#else
	regs.rip = (seL4_Word)user_prog;
	regs.rsp = 0x7f0000000000;
#endif
	err = seL4_TCB_WriteRegisters(utcb, 1, 0, sizeof(regs) / sizeof(seL4_Word), &regs);
	CHECK(err == 0, "write regs err=", err);

	/* 1: getpid syscall arrives as UnknownSyscall with Linux registers. */
	seL4_Word badge;
	seL4_MessageInfo_t info = seL4_Recv(ep, &badge, reply);
	CHECK(badge == (L4H_EP_BADGE_TASK | 1), "badge1=", badge);
	CHECK(seL4_MessageInfo_get_label(info) == seL4_Fault_UnknownSyscall, "label1=",
	      seL4_MessageInfo_get_label(info));
	CHECK(seL4_GetMR(MR_SYSNO) == SYS_getpid, "sysno=", seL4_GetMR(MR_SYSNO));
	CHECK(seL4_GetMR(MR_SEL4NO) == (seL4_Word)-1, "sel4 sysno reg=", seL4_GetMR(MR_SEL4NO));
	seL4_Word sysregs[MR_COUNT];
	for (int i = 0; i < MR_COUNT; i++)
		sysregs[i] = seL4_GetMR(i);
	puthex("testguest: syscall FaultIP - user_prog = ", sysregs[seL4_UnknownSyscall_FaultIP] - (seL4_Word)user_prog);
	puts("testguest: E1 trapped Linux syscall from user thread\n");

	/* E2: the user thread's xmm0 is visible through ReadFPU. */
	err = seL4_TCB_ReadFPU(utcb, L4H_CPTR(vcpu + L4H_VCPU_IPCBUF), L4H_FPU_SCRATCH_OFFSET);
	CHECK(err == 0, "readfpu err=", err);
	uint64_t xmm0 = *(volatile uint64_t *)(L4H_VCPU_IPCBUF_VA(0) + L4H_FPU_SCRATCH_OFFSET + FPU_V0_OFFSET);
	CHECK(xmm0 == 0x1122334455667788ull, "xmm0=", xmm0);
	puts("testguest: E2 read user FPU state\n");

	/* Return pid 777 and step over the syscall instruction. */
	sysregs[MR_RET] = 777;
	sysregs[seL4_UnknownSyscall_FaultIP] += SYSCALL_INSN_LEN;
	for (int i = 0; i < MR_COUNT; i++)
		seL4_SetMR(i, sysregs[i]);
	seL4_Send(reply, seL4_MessageInfo_new(0, 0, 0, MR_COUNT));

	/* 2: the load from 0x500000 is a VM fault. */
	info = seL4_Recv(ep, &badge, reply);
	CHECK(seL4_MessageInfo_get_label(info) == seL4_Fault_VMFault, "label2=",
	      seL4_MessageInfo_get_label(info));
	CHECK(seL4_GetMR(seL4_VMFault_Addr) == 0x500000, "fault addr=", seL4_GetMR(seL4_VMFault_Addr));
	puts("testguest: user page fault delivered\n");
	/* Map a zero page there and restart the instruction. */
	uint64_t zero_gpa = (bi->kernel_end_gpa + 0x200000 + 0xfff) & ~0xfffull;
	map_user_page(vspace, copy_cap(L4H_FRAME_CPTR(zero_gpa), seL4_AllRights), 0x500000,
		      seL4_CanRead);
	seL4_Send(reply, seL4_MessageInfo_new(0, 0, 0, 0));

	/* 3: 64 syscalls with rdx = -1 .. -64 (all seL4 syscall numbers,
	 * including non-faulting ones such as Yield, NBSend and SetTLSBase). */
	uint64_t sweep_t0 = rdtsc();
	for (long n = -1; n >= -64; n--) {
		info = seL4_Recv(ep, &badge, reply);
		CHECK(seL4_MessageInfo_get_label(info) == seL4_Fault_UnknownSyscall, "sweep label=",
		      seL4_MessageInfo_get_label(info));
		CHECK(seL4_GetMR(MR_SEL4NO) == (seL4_Word)n, "sweep sel4 sysno reg=",
		      seL4_GetMR(MR_SEL4NO));
		CHECK(seL4_GetMR(MR_SYSNO) == SYS_getpid, "sweep sysno=", seL4_GetMR(MR_SYSNO));
		seL4_Word r[MR_COUNT];
		for (int i = 0; i < MR_COUNT; i++)
			r[i] = seL4_GetMR(i);
		r[seL4_UnknownSyscall_FaultIP] += SYSCALL_INSN_LEN;
		for (int i = 0; i < MR_COUNT; i++)
			seL4_SetMR(i, r[i]);
		seL4_Send(reply, seL4_MessageInfo_new(0, 0, 0, MR_COUNT));
	}
	puts("testguest: E1 traps every seL4 syscall number (seL4 issue #477)\n");
	puthex("testguest: trapped syscall round trip, ticks=", (rdtsc() - sweep_t0) / 64);

	/* 4: exit(777). */
	info = seL4_Recv(ep, &badge, reply);
	CHECK(seL4_MessageInfo_get_label(info) == seL4_Fault_UnknownSyscall, "label3=",
	      seL4_MessageInfo_get_label(info));
	CHECK(seL4_GetMR(MR_SYSNO) == SYS_exit, "sysno3=", seL4_GetMR(MR_SYSNO));
	CHECK(seL4_GetMR(MR_ARG0) == 777, "exit code=", seL4_GetMR(MR_ARG0));
	puts("testguest: syscall return value reached user mode\n");
	seL4_TCB_Suspend(utcb);

	/* 5: without the trap flag, seL4_DebugHalt (which would stop the whole
	 * host) is an unknown syscall for a guest thread: its MCP is too low. */
	fl = seL4_TCB_SetFlags(utcb, seL4_TCBFlag_trapSyscalls, 0);
	CHECK(fl.error == 0 && fl.flags == 0, "clearflags=", fl.flags);
#ifdef __aarch64__
	regs.pc = (seL4_Word)user_debug;
#else
	regs.rip = (seL4_Word)user_debug;
#endif
	err = seL4_TCB_WriteRegisters(utcb, 1, 0, sizeof(regs) / sizeof(seL4_Word), &regs);
	CHECK(err == 0, "write regs err=", err);
	info = seL4_Recv(ep, &badge, reply);
	CHECK(seL4_MessageInfo_get_label(info) == seL4_Fault_UnknownSyscall, "debug label=",
	      seL4_MessageInfo_get_label(info));
	CHECK(seL4_GetMR(seL4_UnknownSyscall_Syscall) == (seL4_Word)-14, "debug sysno=",
	      seL4_GetMR(seL4_UnknownSyscall_Syscall));
	puts("testguest: debug syscalls refused to guest threads\n");
	seL4_TCB_Suspend(utcb);

#ifndef __aarch64__
	/* 6: seL4_X86_ExecuteNever (patch 0009): the program page, remapped
	 * execute-never, faults on instruction fetch. */
	err = seL4_X86_Page_Map(prog_frame, vspace, (seL4_Word)user_prog, seL4_CanRead,
				seL4_X86_ExecuteNever);
	CHECK(err == 0, "remap xn err=", err);
	regs.rip = (seL4_Word)user_prog;
	err = seL4_TCB_WriteRegisters(utcb, 1, 0, sizeof(regs) / sizeof(seL4_Word), &regs);
	CHECK(err == 0, "write regs err=", err);
	info = seL4_Recv(ep, &badge, reply);
	CHECK(seL4_MessageInfo_get_label(info) == seL4_Fault_VMFault, "xn label=",
	      seL4_MessageInfo_get_label(info));
	CHECK(seL4_GetMR(seL4_VMFault_Addr) == (seL4_Word)user_prog, "xn addr=",
	      seL4_GetMR(seL4_VMFault_Addr));
	CHECK(seL4_GetMR(seL4_VMFault_PrefetchFault) == 1, "xn prefetch=",
	      seL4_GetMR(seL4_VMFault_PrefetchFault));
	puts("testguest: execute-never mappings fault on instruction fetch\n");
	seL4_TCB_Suspend(utcb);
#endif

	/* E3: deadline timer 10 ms from now signals the bound notification. */
	uint64_t t0 = rdtsc();
	uint64_t deadline = t0 + bi->tsc_hz / 100;
	err = seL4_TCB_SetTimer(ktcb, deadline, L4H_BADGE_TIMER);
	CHECK(err == 0, "settimer err=", err);
	seL4_Wait(L4H_CPTR(vcpu + L4H_VCPU_NTFN), &badge);
	uint64_t t1 = rdtsc();
	CHECK(badge & L4H_BADGE_TIMER, "timer badge=", badge);
	CHECK(t1 >= deadline - bi->tsc_hz / 10000, "timer early by=", deadline - t1);
	puthex("testguest: E3 timer fired, latency ticks=", t1 - deadline);

	puts("TESTGUEST: PASS\n");
	power(L4H_POWER_OFF);
}
