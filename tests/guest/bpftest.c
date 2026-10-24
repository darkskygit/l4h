// SPDX-License-Identifier: GPL-2.0
/*
 * The BPF JIT in an l4h guest, through the bpf() system call: verified
 * XDP and tc programs are JIT-compiled and call helpers, a direct tail call
 * is patched in and out of running code, and XDP (generic) and tcx
 * programs see the packets of a ping on lo.  Run by tests/run-bpftest.sh.
 */
#define _GNU_SOURCE
#include <errno.h>
#include <linux/bpf.h>
#include <net/if.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/syscall.h>
#include <unistd.h>

#ifndef XDP_FLAGS_SKB_MODE
#define XDP_FLAGS_SKB_MODE (1U << 1)
#endif

#define INSN(c, d, s, o, i) \
	((struct bpf_insn){ .code = (c), .dst_reg = (d), .src_reg = (s), .off = (o), .imm = (i) })
#define MOV64_IMM(d, i)		INSN(BPF_ALU64 | BPF_MOV | BPF_K, d, 0, 0, i)
#define MOV64_REG(d, s)		INSN(BPF_ALU64 | BPF_MOV | BPF_X, d, s, 0, 0)
#define ADD64_IMM(d, i)		INSN(BPF_ALU64 | BPF_ADD | BPF_K, d, 0, 0, i)
#define ST_W(d, o, i)		INSN(BPF_ST | BPF_MEM | BPF_W, d, 0, o, i)
#define LD_MAP_FD(d, fd)	INSN(BPF_LD | BPF_DW | BPF_IMM, d, BPF_PSEUDO_MAP_FD, 0, fd), \
				INSN(0, 0, 0, 0, 0)
#define JEQ_IMM(d, i, o)	INSN(BPF_JMP | BPF_JEQ | BPF_K, d, 0, o, i)
#define ATOMIC_ADD64(d, s, o)	INSN(BPF_STX | BPF_ATOMIC | BPF_DW, d, s, o, BPF_ADD)
#define CALL(f)			INSN(BPF_JMP | BPF_CALL, 0, 0, 0, f)
#define EXIT()			INSN(BPF_JMP | BPF_EXIT, 0, 0, 0, 0)

static int failures;

#define CHECK(cond, ...) do {						\
	if (!(cond)) {							\
		printf("BPFTEST: FAIL " __VA_ARGS__);			\
		printf(" (line %d, errno %d %s)\n", __LINE__, errno,	\
		       strerror(errno));				\
		failures++;						\
	}								\
} while (0)

static long sys_bpf(int cmd, union bpf_attr *attr)
{
	return syscall(__NR_bpf, cmd, attr, sizeof(*attr));
}

static char log_buf[65536];

static int prog_load(int type, const struct bpf_insn *insns, int n)
{
	union bpf_attr a = {
		.prog_type = type,
		.insns = (uintptr_t)insns,
		.insn_cnt = n,
		.license = (uintptr_t)"GPL",
		.log_buf = (uintptr_t)log_buf,
		.log_size = sizeof(log_buf),
		.log_level = 1,
	};
	int fd = sys_bpf(BPF_PROG_LOAD, &a);

	if (fd < 0)
		printf("verifier log:\n%s\n", log_buf);
	return fd;
}

static int map_create(int type, int value_size, int entries)
{
	union bpf_attr a = {
		.map_type = type,
		.key_size = 4,
		.value_size = value_size,
		.max_entries = entries,
	};

	return sys_bpf(BPF_MAP_CREATE, &a);
}

static int map_update(int fd, uint32_t key, const void *value)
{
	union bpf_attr a = {
		.map_fd = fd,
		.key = (uintptr_t)&key,
		.value = (uintptr_t)value,
	};

	return sys_bpf(BPF_MAP_UPDATE_ELEM, &a);
}

static int map_delete(int fd, uint32_t key)
{
	union bpf_attr a = { .map_fd = fd, .key = (uintptr_t)&key };

	return sys_bpf(BPF_MAP_DELETE_ELEM, &a);
}

static uint64_t counter(int fd)
{
	uint32_t key = 0;
	uint64_t v = ~0ull;
	union bpf_attr a = {
		.map_fd = fd,
		.key = (uintptr_t)&key,
		.value = (uintptr_t)&v,
	};

	if (sys_bpf(BPF_MAP_LOOKUP_ELEM, &a))
		return ~0ull;
	return v;
}

/* JIT-compiled: the kernel reports native code for it. */
static int jited(int fd)
{
	struct bpf_prog_info info = {};
	union bpf_attr a = {
		.info.bpf_fd = fd,
		.info.info_len = sizeof(info),
		.info.info = (uintptr_t)&info,
	};

	if (sys_bpf(BPF_OBJ_GET_INFO_BY_FD, &a))
		return 0;
	return info.jited_prog_len > 0;
}

static int test_run(int fd, uint32_t *retval)
{
	unsigned char pkt[64] = { [12] = 0x08, [13] = 0x00 };
	union bpf_attr a = {
		.test.prog_fd = fd,
		.test.data_in = (uintptr_t)pkt,
		.test.data_size_in = sizeof(pkt),
		.test.repeat = 1000,
	};
	int err = sys_bpf(BPF_PROG_TEST_RUN, &a);

	*retval = a.test.retval;
	return err;
}

/* Count calls in the array at map_fd[0] (helper call, atomic add). */
static int counting_prog(int type, int map_fd, int ret)
{
	struct bpf_insn p[] = {
		ST_W(BPF_REG_10, -4, 0),
		MOV64_REG(BPF_REG_2, BPF_REG_10),
		ADD64_IMM(BPF_REG_2, -4),
		LD_MAP_FD(BPF_REG_1, map_fd),
		CALL(BPF_FUNC_map_lookup_elem),
		JEQ_IMM(BPF_REG_0, 0, 2),
		MOV64_IMM(BPF_REG_1, 1),
		ATOMIC_ADD64(BPF_REG_0, BPF_REG_1, 0),
		MOV64_IMM(BPF_REG_0, ret),
		EXIT(),
	};

	return prog_load(type, p, sizeof(p) / sizeof(p[0]));
}

static void test_progs(void)
{
	int map = map_create(BPF_MAP_TYPE_ARRAY, 8, 1);
	int types[] = { BPF_PROG_TYPE_XDP, BPF_PROG_TYPE_SCHED_CLS };
	uint32_t ret;

	CHECK(map >= 0, "map create");
	for (int i = 0; i < 2; i++) {
		int fd = counting_prog(types[i], map, 7);
		uint64_t before = counter(map);

		CHECK(fd >= 0, "load prog type %d", types[i]);
		if (fd < 0)
			continue;
		CHECK(jited(fd), "prog type %d not JIT-compiled", types[i]);
		CHECK(test_run(fd, &ret) == 0 && ret == 7, "run prog type %d: %u", types[i], ret);
		CHECK(counter(map) == before + 1000, "prog type %d counted %llu", types[i],
		      (unsigned long long)(counter(map) - before));
		close(fd);
	}
	close(map);
	printf("BPFTEST: progs done\n");
}

/*
 * A tail call with a constant index is a direct jump that the kernel
 * patches when the program array changes.
 */
static void test_tail_call(void)
{
	int pa = map_create(BPF_MAP_TYPE_PROG_ARRAY, 4, 1);
	struct bpf_insn callee[] = { MOV64_IMM(BPF_REG_0, 42), EXIT() };
	struct bpf_insn caller[] = {
		LD_MAP_FD(BPF_REG_2, pa),
		MOV64_IMM(BPF_REG_3, 0),
		CALL(BPF_FUNC_tail_call),
		MOV64_IMM(BPF_REG_0, 1),
		EXIT(),
	};
	int b = prog_load(BPF_PROG_TYPE_XDP, callee, 2);
	int a = prog_load(BPF_PROG_TYPE_XDP, caller, sizeof(caller) / sizeof(caller[0]));
	uint32_t ret;

	CHECK(pa >= 0 && a >= 0 && b >= 0, "tail call setup");
	if (pa < 0 || a < 0 || b < 0)
		return;
	CHECK(jited(a) && jited(b), "tail call progs not JIT-compiled");
	for (int i = 0; i < 100; i++) {
		CHECK(test_run(a, &ret) == 0 && ret == 1, "empty slot: %u", ret);
		CHECK(map_update(pa, 0, &b) == 0, "prog array update");
		CHECK(test_run(a, &ret) == 0 && ret == 42, "tail call: %u", ret);
		CHECK(map_delete(pa, 0) == 0, "prog array delete");
	}
	close(a);
	close(b);
	close(pa);
	printf("BPFTEST: tail call done\n");
}

static int link_create(int prog, int ifindex, int type, int flags)
{
	union bpf_attr a = {
		.link_create.prog_fd = prog,
		.link_create.target_ifindex = ifindex,
		.link_create.attach_type = type,
		.link_create.flags = flags,
	};

	return sys_bpf(BPF_LINK_CREATE, &a);
}

/* XDP (generic) and tcx ingress on lo, which a ping goes through. */
static void test_attach(void)
{
	int lo = if_nametoindex("lo");
	int xmap = map_create(BPF_MAP_TYPE_ARRAY, 8, 1);
	int tmap = map_create(BPF_MAP_TYPE_ARRAY, 8, 1);
	int xdp = counting_prog(BPF_PROG_TYPE_XDP, xmap, 2 /* XDP_PASS */);
	int tc = counting_prog(BPF_PROG_TYPE_SCHED_CLS, tmap, -1 /* TCX_NEXT */);
	int xl, tl;

	CHECK(lo > 0 && xdp >= 0 && tc >= 0, "attach setup");
	if (lo <= 0 || xdp < 0 || tc < 0)
		return;
	xl = link_create(xdp, lo, BPF_XDP, XDP_FLAGS_SKB_MODE);
	CHECK(xl >= 0, "XDP attach");
	tl = link_create(tc, lo, BPF_TCX_INGRESS, 0);
	CHECK(tl >= 0, "tcx attach");
	CHECK(system("ping -c 5 -i 0.2 -W 1 127.0.0.1 >/dev/null") == 0, "ping");
	/* Echo requests and replies both arrive on lo. */
	CHECK(counter(xmap) >= 10, "XDP saw %llu packets", (unsigned long long)counter(xmap));
	CHECK(counter(tmap) >= 10, "tcx saw %llu packets", (unsigned long long)counter(tmap));
	close(xl);
	close(tl);
	close(xdp);
	close(tc);
	printf("BPFTEST: attach done\n");
}

int main(void)
{
	printf("BPFTEST: start\n");
	test_progs();
	test_tail_call();
	test_attach();
	if (failures)
		printf("BPFTEST: FAIL (%d)\n", failures);
	else
		printf("BPFTEST: PASS\n");
	return !!failures;
}
