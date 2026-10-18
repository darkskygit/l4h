/*
 * l4h test guest: a freestanding "guest kernel" that exercises the l4h
 * guest ABI and the seL4 extensions E1-E3 end to end:
 *   - hypercall console output,
 *   - a user thread with seL4_TCBFlag_trapSyscalls in a pool VSpace,
 *     whose Linux-style syscalls and page faults arrive as fault IPC,
 *   - seL4_TCB_ReadFPU of the user thread's xmm0,
 *   - seL4 debug syscalls refused to guest threads (an unknown syscall),
 *   - seL4_X86_ExecuteNever mappings,
 *   - seL4_TCB_SetTimer delivering a timer badge,
 *   - the I/O pool: a virtio-blk driver that ignores ACCESS_PLATFORM or
 *     puts rings or buffers outside the pool is refused.
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

/* ---- object allocation into dynamic root slots, from the first block of
 * RAM the host gives us as an Untyped (DESIGN.md §15.2) */
static seL4_Word next_slot = L4H_SLOT_DYN_BASE;
#define OBJ_UT		L4H_BLOCK_CPTR(bi->boot_blocks)

static seL4_CPtr alloc_obj(seL4_Word type, seL4_Word size_bits)
{
	seL4_Word slot = next_slot++;
	int err = seL4_Untyped_Retype(OBJ_UT, type, size_bits,
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

/* ---- seL4 semantics of Untyped copies (DESIGN.md §15.2)
 *
 * l4h-root keeps the original Untyped of each 2 MiB block of guest RAM and
 * gives the guest a copy.  Check, on a 1 MiB Untyped `orig` (a child of a
 * block) with a copy `copy`, playing l4h-root's and the guest's caps:
 *   - while the copy exists, the original cannot be retyped;
 *   - objects retyped from the copy can be revoked through the copy;
 *   - a retype from a copy without children starts again at its base and
 *     the memory is zeroed (reset), whatever it held before;
 *   - a copy with children cannot be copied again;
 *   - revoking the original deletes the copy and everything retyped from it.
 */
#define UT_TEST_VA	(L4H_SPECIAL_BASE + 0x30000000ull)
#ifdef __aarch64__
#define UT_FRAME	seL4_ARM_SmallPageObject
#define ut_unmap	seL4_ARM_Page_Unmap
#else
#define UT_FRAME	seL4_X86_4K
#define ut_unmap	seL4_X86_Page_Unmap
#endif

static void ut_map(seL4_CPtr frame, seL4_Word va)
{
	map_user_page(L4H_CPTR(L4H_SLOT_VSPACE), frame, va, seL4_ReadWrite);
}

#define UT_BITS		20
#define UT_FRAMES	(1 << (UT_BITS - 12))

static void untyped_copy_test(seL4_CPtr quota)
{
	seL4_Word orig_slot = next_slot++, copy_slot = next_slot++;
	seL4_CPtr orig = L4H_CPTR(orig_slot), copy = L4H_CPTR(copy_slot);
	int err;

	err = seL4_Untyped_Retype(quota, seL4_UntypedObject, UT_BITS, L4H_CNODE_CPTR, 0, 0, orig_slot, 1);
	CHECK(err == 0, "1 MiB untyped err=", err);
	err = seL4_CNode_Copy(L4H_CNODE_CPTR, copy_slot, L4H_ROOT_DEPTH, L4H_CNODE_CPTR, orig_slot,
			      L4H_ROOT_DEPTH, seL4_AllRights);
	CHECK(err == 0, "untyped copy err=", err);

	/* Frames from the copy, the last one mapped and written. */
	seL4_Word f0 = next_slot;
	next_slot += UT_FRAMES;
	err = seL4_Untyped_Retype(copy, UT_FRAME, 0, L4H_CNODE_CPTR, 0, 0, f0, UT_FRAMES);
	CHECK(err == 0, "frames from the copy err=", err);
	volatile uint64_t *p = (volatile uint64_t *)UT_TEST_VA;
	ut_map(L4H_CPTR(f0 + UT_FRAMES - 1), UT_TEST_VA);
	p[0] = 0x1234567890abcdefull;
	CHECK(p[0] == 0x1234567890abcdefull, "frame write", p[0]);

	/* The original is full while the copy exists: it cannot be retyped. */
	err = seL4_Untyped_Retype(orig, UT_FRAME, 0, L4H_CNODE_CPTR, 0, 0, next_slot, 1);
	CHECK(err == seL4_NotEnoughMemory, "retype of the original with a copy err=", err);
	/* A copy that has children cannot be copied again. */
	err = seL4_CNode_Copy(L4H_CNODE_CPTR, next_slot, L4H_ROOT_DEPTH, L4H_CNODE_CPTR, copy_slot,
			      L4H_ROOT_DEPTH, seL4_AllRights);
	CHECK(err != 0, "copy of a copy with children", 0);

	/* Revoking the copy deletes its frames and their mappings. */
	err = seL4_CNode_Revoke(L4H_CNODE_CPTR, copy_slot, L4H_ROOT_DEPTH);
	CHECK(err == 0, "revoke copy err=", err);
	err = ut_unmap(L4H_CPTR(f0 + UT_FRAMES - 1));
	CHECK(err == seL4_InvalidCapability, "frame after revoke err=", err);

	/* The copy has no children: the next retype resets it.  A CNode
	 * that fills the whole block fits again, so the free index went back
	 * to 0; then frames again, which must read as zero. */
	seL4_Word cn = next_slot++;
	err = seL4_Untyped_Retype(copy, seL4_CapTableObject, UT_BITS - seL4_SlotBits, L4H_CNODE_CPTR, 0, 0, cn, 1);
	CHECK(err == 0, "CNode filling the reset copy err=", err);
	err = seL4_CNode_Revoke(L4H_CNODE_CPTR, copy_slot, L4H_ROOT_DEPTH);
	CHECK(err == 0, "revoke copy (CNode) err=", err);
	err = seL4_Untyped_Retype(copy, UT_FRAME, 0, L4H_CNODE_CPTR, 0, 0, f0, UT_FRAMES);
	CHECK(err == 0, "frames again err=", err);
	ut_map(L4H_CPTR(f0 + UT_FRAMES - 1), UT_TEST_VA);
	CHECK(p[0] == 0, "reset memory not zeroed: ", p[0]);

	/* Revoking the original (teardown by l4h-root) deletes the copy and
	 * everything retyped from it. */
	err = seL4_CNode_Revoke(L4H_CNODE_CPTR, orig_slot, L4H_ROOT_DEPTH);
	CHECK(err == 0, "revoke original err=", err);
	err = seL4_Untyped_Retype(copy, UT_FRAME, 0, L4H_CNODE_CPTR, 0, 0, next_slot, 1);
	CHECK(err == seL4_InvalidCapability, "copy after revoke of the original err=", err);
	err = ut_unmap(L4H_CPTR(f0 + UT_FRAMES - 1));
	CHECK(err == seL4_InvalidCapability, "frame after revoke of the original err=", err);
	/* And the original is usable again. */
	err = seL4_Untyped_Retype(orig, UT_FRAME, 0, L4H_CNODE_CPTR, 0, 0, f0, 1);
	CHECK(err == 0, "original after revoke err=", err);
	puts("testguest: Untyped copies: retype/revoke through a copy, reset, teardown by the original: ok\n");
}

/* The RAM the host gives us: boot blocks have frames and no Untyped, the
 * others an Untyped we can retype into frames, revoke and reset. */
static void ram_blocks_test(void)
{
	uint64_t b = bi->boot_blocks + 2;
	seL4_CPtr ut = L4H_BLOCK_CPTR(b);
	int err;

	CHECK(bi->nr_blocks * L4H_BLOCK_SIZE == bi->ram_size, "nr_blocks=", bi->nr_blocks);
	CHECK(bi->boot_blocks > 0 && bi->boot_blocks + 3 <= bi->nr_blocks, "boot_blocks=", bi->boot_blocks);
	puthex("testguest: RAM blocks: ", bi->nr_blocks);
	puthex("testguest: boot blocks: ", bi->boot_blocks);
	err = seL4_Untyped_Retype(L4H_BLOCK_CPTR(0), UT_FRAME, 0, L4H_CNODE_CPTR, 0, 0, next_slot, 1);
	CHECK(err != 0, "boot block has an Untyped", 0);
	/* Frames of a whole block, mapped and written. */
	seL4_Word f0 = next_slot;
	next_slot += 512;
	err = seL4_Untyped_Retype(ut, UT_FRAME, 0, L4H_CNODE_CPTR, 0, 0, f0, 512);
	CHECK(err == 0, "block frames err=", err);
	volatile uint64_t *p = (volatile uint64_t *)UT_TEST_VA;
	ut_map(L4H_CPTR(f0 + 511), UT_TEST_VA);
	p[0] = 0xfeedfacecafebeefull;
	/* Our copy cannot be copied while it has children. */
	err = seL4_CNode_Copy(L4H_CNODE_CPTR, next_slot, L4H_ROOT_DEPTH, L4H_CNODE_CPTR, ut, 64, seL4_AllRights);
	CHECK(err != 0, "copy of a block with children", 0);
	/* Revoke, then the whole block as one CNode (the Untyped was reset),
	 * then frames again, zeroed. */
	err = seL4_CNode_Revoke(L4H_CNODE_CPTR, ut, 64);
	CHECK(err == 0, "block revoke err=", err);
	seL4_Word cn = next_slot++;
	err = seL4_Untyped_Retype(ut, seL4_CapTableObject, L4H_BLOCK_SHIFT - seL4_SlotBits, L4H_CNODE_CPTR, 0, 0, cn, 1);
	CHECK(err == 0, "block CNode err=", err);
	err = seL4_CNode_Revoke(L4H_CNODE_CPTR, ut, 64);
	CHECK(err == 0, "block revoke err=", err);
	err = seL4_Untyped_Retype(ut, UT_FRAME, 0, L4H_CNODE_CPTR, 0, 0, f0, 512);
	CHECK(err == 0, "block frames again err=", err);
	ut_map(L4H_CPTR(f0 + 511), UT_TEST_VA);
	CHECK(p[0] == 0, "reset block not zeroed: ", p[0]);
	err = seL4_CNode_Revoke(L4H_CNODE_CPTR, ut, 64);
	CHECK(err == 0, "block revoke err=", err);
	puts("testguest: RAM blocks: boot frames, Untyped copies retyped, revoked and reset: ok\n");
}

/* ---- the I/O pool, from a hostile driver's point of view
 *
 * Devices may only use the host's I/O pool: l4h-vmm refuses a driver that
 * does not accept VIRTIO_F_ACCESS_PLATFORM, rings outside the pool and
 * requests with any buffer outside it.  Played against the first
 * virtio-blk device (sector 0 holds "l4h-pool-test" from the host).
 */
#define VIRTIO_ID_BLOCK		2
#define VIRTIO_F_VERSION_1	(1ull << 32)
#define VIRTIO_F_ACCESS_PLATFORM (1ull << 33)
#define ST_ACK			1
#define ST_DRIVER		2
#define ST_DRIVER_OK		4
#define ST_FEATURES_OK		8
#define ST_NEEDS_RESET		64
#define VQ_NEXT			1
#define VQ_WRITE		2
#define QN			8

static int blk_dev = -1;

static void *gpa_va(uint64_t gpa)
{
	return (void *)(bi->ram_base_va + gpa);
}

static volatile struct l4h_virtio_regs *blk_regs(void)
{
	return (volatile struct l4h_virtio_regs *)bi->devices[blk_dev].page_va;
}

static uint64_t dev_write(uint64_t op, uint64_t a)
{
	__atomic_thread_fence(__ATOMIC_SEQ_CST);
	seL4_SetMR(0, blk_dev);
	seL4_SetMR(1, op);
	seL4_SetMR(2, a);
	seL4_SetMR(3, 0);
	seL4_Call(L4H_CPTR(L4H_SLOT_HYP_EP), seL4_MessageInfo_new(L4H_HC_DEV_WRITE, 0, 0, 4));
	return seL4_GetMR(0);
}

static uint64_t set_status(uint32_t s)
{
	blk_regs()->status = s;
	return dev_write(L4H_VIRTIO_OP_STATUS, 0);
}

/* Reset, negotiate `features`; returns the status the device left. */
static uint32_t negotiate(uint64_t features)
{
	volatile struct l4h_virtio_regs *r = blk_regs();

	set_status(0);
	CHECK(r->status == 0, "reset status=", r->status);
	set_status(ST_ACK | ST_DRIVER);
	r->driver_features = features;
	dev_write(L4H_VIRTIO_OP_FEATURES, 0);
	set_status(ST_ACK | ST_DRIVER | ST_FEATURES_OK);
	return r->status;
}

/* Queue 0 with its rings at `ring_gpa`: returns the hypercall result. */
static uint64_t setup_queue(uint64_t ring_gpa)
{
	volatile struct l4h_virtq_regs *q = &blk_regs()->queues[0];

	/* Clear the rings where they are mapped (RAM, or all in the pool). */
	if (ring_gpa + 0x400 <= bi->io_pool_gpa + bi->io_pool_size)
		for (uint64_t *p = gpa_va(ring_gpa); p < (uint64_t *)gpa_va(ring_gpa + 0x400); p++)
			*p = 0;
	q->num = QN;
	q->desc = ring_gpa;
	q->driver = ring_gpa + 0x100;
	q->device = ring_gpa + 0x200;
	q->ready = 1;
	return dev_write(L4H_VIRTIO_OP_QUEUE, 0);
}

struct vdesc {
	uint64_t addr;
	uint32_t len;
	uint16_t flags, next;
};

/* Read sector 0 with the header, data and status at the given GPAs (the
 * rings are in the pool at `ring_gpa`).  Returns the used length, or -1 if
 * the device did not return the chain. */
static int64_t blk_read(uint64_t ring_gpa, uint64_t hdr, uint64_t data, uint64_t status)
{
	static uint16_t avail_idx;
	struct vdesc *d = gpa_va(ring_gpa);
	volatile uint16_t *avail = gpa_va(ring_gpa + 0x100);
	volatile uint16_t *used_idx = gpa_va(ring_gpa + 0x200 + 2);
	volatile uint32_t *used = gpa_va(ring_gpa + 0x200 + 4);
	uint16_t before = *used_idx;

	if (hdr >= bi->io_pool_gpa) {
		uint32_t *h = gpa_va(hdr);
		h[0] = 0;	/* VIRTIO_BLK_T_IN */
		h[1] = 0;
		h[2] = h[3] = 0;	/* sector 0 */
	}
	d[0] = (struct vdesc){ hdr, 16, VQ_NEXT, 1 };
	d[1] = (struct vdesc){ data, 512, VQ_NEXT | VQ_WRITE, 2 };
	d[2] = (struct vdesc){ status, 1, VQ_WRITE, 0 };
	avail[2 + avail_idx % QN] = 0;
	__atomic_thread_fence(__ATOMIC_SEQ_CST);
	avail[1] = ++avail_idx;
	__atomic_thread_fence(__ATOMIC_SEQ_CST);
	__atomic_fetch_or(&blk_regs()->kick, 1ull, __ATOMIC_SEQ_CST);
	seL4_Signal(L4H_CPTR2(L4H_SLOT_DEV_CNODE, bi->devices[blk_dev].kick_slot));
	uint64_t end = rdtsc() + bi->tsc_hz;
	while (*used_idx == before) {
		if (rdtsc() > end)
			return -1;
		seL4_Yield();
	}
	__atomic_thread_fence(__ATOMIC_SEQ_CST);
	return used[2 * (before % QN) + 1];
}

static void io_pool_test(void)
{
	uint64_t pool = bi->io_pool_gpa, size = bi->io_pool_size;

	CHECK(size >= (1 << 20) && pool >= bi->ram_size && pool % 4096 == 0, "pool=", pool);
	puthex("testguest: I/O pool at ", pool);
	for (int i = 0; i < (int)bi->nr_devices; i++)
		if (bi->devices[i].type == L4H_DEV_VIRTIO && bi->devices[i].virtio_id == VIRTIO_ID_BLOCK)
			blk_dev = i;
	if (blk_dev < 0) {
		puts("testguest: no virtio-blk device, I/O pool test skipped\n");
		return;
	}
	volatile struct l4h_virtio_regs *r = blk_regs();
	CHECK(r->device_features & VIRTIO_F_ACCESS_PLATFORM, "features=", r->device_features);

	/* A driver without ACCESS_PLATFORM is refused. */
	uint32_t s = negotiate(VIRTIO_F_VERSION_1);
	CHECK(!(s & ST_FEATURES_OK), "FEATURES_OK without ACCESS_PLATFORM, status=", s);

	/* Rings in guest RAM are refused, and so is DRIVER_OK after that. */
	uint64_t ram_ring = (bi->kernel_end_gpa + 0xfff) & ~0xfffull;
	s = negotiate(VIRTIO_F_VERSION_1 | VIRTIO_F_ACCESS_PLATFORM);
	CHECK(s & ST_FEATURES_OK, "FEATURES_OK refused, status=", s);
	CHECK(setup_queue(ram_ring) != 0, "queue in RAM accepted, gpa=", ram_ring);
	CHECK(r->queues[0].ready == 0, "queue in RAM ready=", r->queues[0].ready);
	CHECK(setup_queue(pool + size - 0x100) != 0, "queue across the pool end accepted", 0);
	CHECK(set_status(ST_ACK | ST_DRIVER | ST_FEATURES_OK | ST_DRIVER_OK) != 0, "DRIVER_OK accepted", 0);
	CHECK(!(r->status & ST_DRIVER_OK) && (r->status & ST_NEEDS_RESET), "status=", r->status);

	/* Rings in the pool work. */
	s = negotiate(VIRTIO_F_VERSION_1 | VIRTIO_F_ACCESS_PLATFORM);
	CHECK(s & ST_FEATURES_OK, "FEATURES_OK refused, status=", s);
	CHECK(setup_queue(pool) == 0, "queue in the pool refused", pool);
	CHECK(set_status(ST_ACK | ST_DRIVER | ST_FEATURES_OK | ST_DRIVER_OK) == 0, "DRIVER_OK refused", 0);

	volatile uint8_t *data = gpa_va(pool + 0x1000);
	volatile uint8_t *st = gpa_va(pool + 0x600);
	volatile uint8_t *ram = gpa_va(ram_ring);
	/* A read into the pool. */
	*st = 0xff;
	int64_t n = blk_read(pool, pool + 0x400, pool + 0x1000, pool + 0x600);
	CHECK(n == 513 && *st == 0, "pool read: used=", n);
	CHECK(data[0] == 'l' && data[1] == '4' && data[2] == 'h', "pool read data=", data[0]);
	/* A read into guest RAM is refused: nothing is written there, and the
	 * status byte is untouched. */
	for (int i = 0; i < 1024; i++)
		ram[i] = 0x5a;
	*st = 0xff;
	n = blk_read(pool, pool + 0x400, ram_ring, pool + 0x600);
	CHECK(n == 0 && *st == 0xff && ram[0] == 0x5a && ram[511] == 0x5a, "RAM read: used=", n);
	/* So are a header in RAM, a status byte in RAM, and a buffer that runs
	 * past the end of the pool. */
	n = blk_read(pool, ram_ring, pool + 0x1000, pool + 0x600);
	CHECK(n == 0 && *st == 0xff, "header in RAM: used=", n);
	n = blk_read(pool, pool + 0x400, pool + 0x1000, ram_ring + 600);
	CHECK(n == 0 && ram[600] == 0x5a, "status in RAM: used=", n);
	n = blk_read(pool, pool + 0x400, pool + size - 256, pool + 0x600);
	CHECK(n == 0 && *st == 0xff, "buffer past the pool: used=", n);
	/* The device still works. */
	data[0] = 0;
	n = blk_read(pool, pool + 0x400, pool + 0x1000, pool + 0x600);
	CHECK(n == 513 && *st == 0 && data[0] == 'l', "pool read again: used=", n);
	set_status(0);
	puts("testguest: I/O pool: ACCESS_PLATFORM required, rings and buffers outside the pool refused\n");
}

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
	err = seL4_Untyped_Retype(OBJ_UT, seL4_CapTableObject, 1,
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

	io_pool_test();
	untyped_copy_test(L4H_BLOCK_CPTR(bi->boot_blocks + 1));
	ram_blocks_test();

	puts("TESTGUEST: PASS\n");
	power(L4H_POWER_OFF);
}
