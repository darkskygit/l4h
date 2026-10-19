/*
 * l4h guest dm-crypt / kernel-mode SIMD test (x86_64 and aarch64).
 *
 * "Accelerated" is AES-NI on x86_64 and the ARMv8 Crypto Extensions on
 * aarch64 (bit-sliced NEON if the CPU lacks them).
 *
 * - /proc/crypto: xts(aes) resolves to the accelerated driver, whose
 *   self-test passed.
 * - Cross-check: data written through an accelerated dm-crypt device reads
 *   back identically through a generic-C dm-crypt device over the same
 *   loop device, and the other way round.
 * - Throughput: O_DIRECT reads and writes through dm-crypt over dm-zero,
 *   generic C vs accelerated.
 * - SIMD stress: one thread per vCPU keeps its own patterns in the vector
 *   registers and FP control state (YMM if AVX is usable, else XMM, MXCSR
 *   and the x87 control word on x86_64; V0-V31 and FPCR on aarch64) and
 *   checks them across syscalls and preemption while other threads keep
 *   dm-crypt busy on every vCPU.
 *
 * Prints "CRYPTOTEST: PASS" or "CRYPTOTEST: FAIL".
 * Usage: cryptotest [stress-seconds] [throughput-MiB]
 */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <pthread.h>
#include <sched.h>
#include <stdatomic.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/stat.h>
#include <sys/sysmacros.h>
#include <time.h>
#include <unistd.h>
#ifdef __x86_64__
#include <cpuid.h>
#else
#include <sys/auxv.h>
#include <asm/hwcap.h>
#endif
#include <linux/dm-ioctl.h>

static int failures;

#define FAIL(...) do { printf("  FAIL: " __VA_ARGS__); printf("\n"); failures++; } while (0)

static double now(void)
{
	struct timespec ts;

	clock_gettime(CLOCK_MONOTONIC, &ts);
	return ts.tv_sec + ts.tv_nsec / 1e9;
}

/* ---- device-mapper through /dev/mapper/control */

static int dm_ioctl(unsigned long cmd, const char *name, const char *target,
		    unsigned long long sectors, const char *params, unsigned int flags,
		    dev_t *dev)
{
	char buf[16384] __attribute__((aligned(8)));
	struct dm_ioctl *io = (struct dm_ioctl *)buf;
	int fd, ret;

	memset(buf, 0, sizeof(buf));
	io->version[0] = DM_VERSION_MAJOR;
	io->version[1] = 0;
	io->version[2] = 0;
	io->data_size = sizeof(buf);
	io->data_start = sizeof(*io);
	io->flags = flags;
	snprintf(io->name, sizeof(io->name), "%s", name);
	if (target) {
		struct dm_target_spec *sp = (struct dm_target_spec *)(buf + sizeof(*io));
		char *p = (char *)(sp + 1);
		size_t len = strlen(params) + 1;

		sp->sector_start = 0;
		sp->length = sectors;
		snprintf(sp->target_type, sizeof(sp->target_type), "%s", target);
		memcpy(p, params, len);
		sp->next = (sizeof(*sp) + len + 7) & ~7UL;
		io->target_count = 1;
	}
	fd = open("/dev/mapper/control", O_RDWR);
	if (fd < 0)
		return -errno;
	ret = ioctl(fd, cmd, io);
	if (ret < 0)
		ret = -errno;
	close(fd);
	if (!ret && dev)
		*dev = (dev_t)io->dev;
	return ret;
}

/* Create a one-target device; returns its /dev/dm-N path in `path`. */
static int dm_create(const char *name, const char *target, unsigned long long sectors,
		     const char *params, char *path, size_t pathlen)
{
	dev_t dev;
	int err;

	err = dm_ioctl(DM_DEV_CREATE, name, NULL, 0, NULL, 0, &dev);
	if (!err)
		err = dm_ioctl(DM_TABLE_LOAD, name, target, sectors, params, 0, NULL);
	if (!err)
		err = dm_ioctl(DM_DEV_SUSPEND, name, NULL, 0, NULL, 0, &dev);
	if (err) {
		printf("  dm %s (%s %s): %s\n", name, target, params, strerror(-err));
		dm_ioctl(DM_DEV_REMOVE, name, NULL, 0, NULL, 0, NULL);
		return err;
	}
	snprintf(path, pathlen, "/dev/dm-%u", minor(dev));
	/* devtmpfs creates the node asynchronously. */
	for (int i = 0; i < 200 && access(path, F_OK); i++)
		usleep(10000);
	return 0;
}

static void dm_remove(const char *name)
{
	dm_ioctl(DM_DEV_REMOVE, name, NULL, 0, NULL, 0, NULL);
}

static const char key[] =
	"0123456789abcdef0123456789abcdef00112233445566778899aabbccddeeff"
	"fedcba9876543210fedcba9876543210ffeeddccbbaa99887766554433221100";
#define CIPHER_ACCEL	"aes-xts-plain64"
#define CIPHER_GENERIC	"capi:xts(ecb(aes-generic))-plain64"
#ifdef __x86_64__
#define ACCEL		"AES-NI"
#else
#define ACCEL		"NEON"
#endif

/* Whether `driver` is the accelerated xts(aes) driver expected here. */
static int accel_driver(const char *driver)
{
#ifdef __x86_64__
	return strstr(driver, "aesni") || strstr(driver, "vaes");
#else
	/* The Crypto Extensions driver if the CPU has AES, else bit-sliced NEON. */
	if (getauxval(AT_HWCAP) & HWCAP_AES)
		return !strcmp(driver, "xts-aes-ce");
	return !strcmp(driver, "xts-aes-neonbs");
#endif
}

/* ---- /proc/crypto */

static void test_proc_crypto(void)
{
	FILE *f = fopen("/proc/crypto", "r");
	char line[256], driver[128] = "", best[128] = "";
	int is_xts = 0, prio = 0, best_prio = -1, passed = 0, best_passed = 0;

	if (!f) {
		FAIL("open /proc/crypto");
		return;
	}
	/* Records are separated by blank lines; name comes first. */
	for (;;) {
		char *ok = fgets(line, sizeof(line), f);

		if (!ok || line[0] == '\n') {
			if (is_xts && prio > best_prio) {
				best_prio = prio;
				best_passed = passed;
				snprintf(best, sizeof(best), "%s", driver);
			}
			if (is_xts)
				printf("  xts(aes): driver %s priority %d selftest %s\n",
				       driver, prio, passed ? "passed" : "not passed");
			is_xts = prio = passed = 0;
			driver[0] = 0;
			if (!ok)
				break;
			continue;
		}
		if (!strncmp(line, "name ", 5) && strstr(line, ": xts(aes)\n"))
			is_xts = 1;
		sscanf(line, "driver : %127s", driver);
		sscanf(line, "priority : %d", &prio);
		if (!strncmp(line, "selftest ", 9) && strstr(line, "passed"))
			passed = 1;
	}
	fclose(f);
	if (!accel_driver(best))
		FAIL("xts(aes) resolves to %s, not the expected " ACCEL " driver",
		     best[0] ? best : "nothing");
	else if (!best_passed)
		FAIL("%s self-test did not pass", best);
	else
		printf("  xts(aes) uses %s\n", best);
}

/* ---- cross-check the accelerated driver against generic C over one device */

static int rw_all(int fd, void *buf, size_t len, off_t off, int wr)
{
	size_t done = 0;

	while (done < len) {
		ssize_t n = wr ? pwrite(fd, (char *)buf + done, len - done, off + done)
			       : pread(fd, (char *)buf + done, len - done, off + done);
		if (n <= 0)
			return -1;
		done += n;
	}
	return 0;
}

static void cross_check(const char *back, unsigned long long sectors)
{
	char a[64], g[64], params[512];
	size_t len = sectors * 512;
	unsigned char *src, *dst;
	int fa, fg;

	snprintf(params, sizeof(params), "%s %s 0 %s 0", CIPHER_ACCEL, key, back);
	if (dm_create("xc-accel", "crypt", sectors, params, a, sizeof(a))) {
		FAIL("create " ACCEL " crypt device");
		return;
	}
	snprintf(params, sizeof(params), "%s %s 0 %s 0", CIPHER_GENERIC, key, back);
	if (dm_create("xc-generic", "crypt", sectors, params, g, sizeof(g))) {
		FAIL("create generic crypt device");
		dm_remove("xc-accel");
		return;
	}
	if (posix_memalign((void **)&src, 4096, len) || posix_memalign((void **)&dst, 4096, len))
		abort();
	fa = open(a, O_RDWR | O_DIRECT);
	fg = open(g, O_RDWR | O_DIRECT);
	for (int dir = 0; dir < 2 && fa >= 0 && fg >= 0; dir++) {
		int wfd = dir ? fg : fa, rfd = dir ? fa : fg;

		for (size_t i = 0; i < len; i++)
			src[i] = (unsigned char)(rand() >> 7);
		memset(dst, 0, len);
		if (rw_all(wfd, src, len, 0, 1) || rw_all(rfd, dst, len, 0, 0)) {
			FAIL("cross-check I/O: %s", strerror(errno));
			break;
		}
		if (memcmp(src, dst, len))
			FAIL("%s-encrypted data differs when decrypted by %s",
			     dir ? "generic" : ACCEL, dir ? ACCEL : "generic");
		else
			printf("  %s -> %s: %zu KiB identical\n", dir ? "generic" : ACCEL,
			       dir ? ACCEL : "generic", len / 1024);
	}
	if (fa < 0 || fg < 0)
		FAIL("open crypt devices");
	close(fa);
	close(fg);
	free(src);
	free(dst);
	dm_remove("xc-generic");
	dm_remove("xc-accel");
}

/* ---- throughput over dm-zero */

struct stream {
	const char *path;
	int wr;
	size_t bytes;
	off_t off;
	int err;
};

static void *stream_run(void *arg)
{
	struct stream *s = arg;
	size_t bs = 1 << 20;
	void *buf;
	int fd = open(s->path, (s->wr ? O_WRONLY : O_RDONLY) | O_DIRECT);

	if (fd < 0 || posix_memalign(&buf, 4096, bs)) {
		s->err = 1;
		return NULL;
	}
	memset(buf, 0x5a, bs);
	for (size_t done = 0; done < s->bytes; done += bs)
		if (rw_all(fd, buf, bs, s->off + done, s->wr)) {
			s->err = 1;
			break;
		}
	free(buf);
	close(fd);
	return NULL;
}

static double throughput(const char *path, int wr, size_t mib, int nstreams)
{
	pthread_t th[64];
	struct stream st[64];
	double t0, t;
	int err = 0;

	t0 = now();
	for (int i = 0; i < nstreams; i++) {
		st[i] = (struct stream){ path, wr, mib << 20, (off_t)i * (mib << 20), 0 };
		pthread_create(&th[i], NULL, stream_run, &st[i]);
	}
	for (int i = 0; i < nstreams; i++) {
		pthread_join(th[i], NULL);
		err |= st[i].err;
	}
	t = now() - t0;
	if (err)
		return -1;
	return (double)nstreams * mib / t;
}

static void bench(size_t mib, int ncpu)
{
	/* Optional entries name one driver and are skipped if it is absent. */
	static const struct { const char *name, *cipher; int optional; } c[] = {
		{ "generic", CIPHER_GENERIC, 0 },
#ifdef __x86_64__
		{ "aesni-sse", "capi:xts-aes-aesni-plain64", 1 },
		{ "aesni-avx", "capi:xts-aes-aesni-avx-plain64", 1 },
		{ "vaes-avx2", "capi:xts-aes-vaes-avx2-plain64", 1 },
#else
		{ "neonbs", "capi:xts-aes-neonbs-plain64", 1 },
		{ "aes-ce", "capi:xts-aes-ce-plain64", 1 },
#endif
		{ "default", CIPHER_ACCEL, 0 },
	};
	unsigned long long sectors = 64ULL << 21;	/* 64 GiB of zeroes */
	char zero[64], dev[64], params[512];
	int streams[2] = { 1, ncpu };

	if (dm_create("bench-zero", "zero", sectors, "", zero, sizeof(zero))) {
		FAIL("create dm-zero");
		return;
	}
	for (size_t i = 0; i < sizeof(c) / sizeof(c[0]); i++) {
		snprintf(params, sizeof(params), "%s %s 0 %s 0", c[i].cipher, key, zero);
		if (dm_create("bench-crypt", "crypt", sectors, params, dev, sizeof(dev))) {
			if (!c[i].optional)
				FAIL("create %s crypt device", c[i].name);
			continue;
		}
		for (int s = 0; s < (ncpu > 1 ? 2 : 1); s++) {
			double w = throughput(dev, 1, mib, streams[s]);
			double r = throughput(dev, 0, mib, streams[s]);

			if (w < 0 || r < 0)
				FAIL("%s I/O", c[i].name);
			printf("  BENCH %-9s %2d stream(s): write %7.1f MB/s  read %7.1f MB/s\n",
			       c[i].name, streams[s], w * 1.048576, r * 1.048576);
		}
		dm_remove("bench-crypt");
	}
	dm_remove("bench-zero");
}

/* ---- SIMD register stress */

static int have_avx;
static atomic_int stop;
static atomic_long simd_checks, simd_errors;

#ifdef __x86_64__
#define NREGS		16	/* vector registers, each in a 32-byte slot */
#define REG_NAME	(have_avx ? "ymm" : "xmm")
#define REG_WIDTH	(have_avx ? 32 : 16)

/*
 * Fill the vector registers from pat (16 x 32 bytes), make `nsys` getppid
 * syscalls and spin `spin` iterations, then store the registers to out.
 * With avx == 0 only the low 16 bytes (XMM) of each slot are used.
 */
static void simd_round(const unsigned char *pat, unsigned char *out, int avx, int nsys, int spin)
{
#define REGS(op, base) \
	op " 0*32(%" base "), %%" R "0\n\t" op " 1*32(%" base "), %%" R "1\n\t" \
	op " 2*32(%" base "), %%" R "2\n\t" op " 3*32(%" base "), %%" R "3\n\t" \
	op " 4*32(%" base "), %%" R "4\n\t" op " 5*32(%" base "), %%" R "5\n\t" \
	op " 6*32(%" base "), %%" R "6\n\t" op " 7*32(%" base "), %%" R "7\n\t" \
	op " 8*32(%" base "), %%" R "8\n\t" op " 9*32(%" base "), %%" R "9\n\t" \
	op " 10*32(%" base "), %%" R "10\n\t" op " 11*32(%" base "), %%" R "11\n\t" \
	op " 12*32(%" base "), %%" R "12\n\t" op " 13*32(%" base "), %%" R "13\n\t" \
	op " 14*32(%" base "), %%" R "14\n\t" op " 15*32(%" base "), %%" R "15\n\t"
#define STORE(op, base) \
	op " %%" R "0, 0*32(%" base ")\n\t" op " %%" R "1, 1*32(%" base ")\n\t" \
	op " %%" R "2, 2*32(%" base ")\n\t" op " %%" R "3, 3*32(%" base ")\n\t" \
	op " %%" R "4, 4*32(%" base ")\n\t" op " %%" R "5, 5*32(%" base ")\n\t" \
	op " %%" R "6, 6*32(%" base ")\n\t" op " %%" R "7, 7*32(%" base ")\n\t" \
	op " %%" R "8, 8*32(%" base ")\n\t" op " %%" R "9, 9*32(%" base ")\n\t" \
	op " %%" R "10, 10*32(%" base ")\n\t" op " %%" R "11, 11*32(%" base ")\n\t" \
	op " %%" R "12, 12*32(%" base ")\n\t" op " %%" R "13, 13*32(%" base ")\n\t" \
	op " %%" R "14, 14*32(%" base ")\n\t" op " %%" R "15, 15*32(%" base ")\n\t"
#define BODY \
	"mov %[nsys], %%r8d\n\t" \
	"1: test %%r8d, %%r8d\n\t" \
	"jz 2f\n\t" \
	"mov $110, %%eax\n\t"	/* getppid */ \
	"syscall\n\t" \
	"dec %%r8d\n\t" \
	"jmp 1b\n\t" \
	"2: mov %[spin], %%r8d\n\t" \
	"3: test %%r8d, %%r8d\n\t" \
	"jz 4f\n\t" \
	"pause\n\t" \
	"dec %%r8d\n\t" \
	"jmp 3b\n\t" \
	"4:\n\t"
#define CLOBBERS "rax", "rcx", "rdx", "rsi", "rdi", "r8", "r9", "r10", "r11", "memory", \
	"xmm0", "xmm1", "xmm2", "xmm3", "xmm4", "xmm5", "xmm6", "xmm7", \
	"xmm8", "xmm9", "xmm10", "xmm11", "xmm12", "xmm13", "xmm14", "xmm15"
	if (avx) {
#define R "ymm"
		asm volatile(REGS("vmovdqu", "[pat]") BODY STORE("vmovdqu", "[out]")
			     :: [pat] "r"(pat), [out] "r"(out), [nsys] "r"(nsys), [spin] "r"(spin)
			     : CLOBBERS);
		asm volatile("vzeroupper" ::: "memory");
#undef R
	} else {
#define R "xmm"
		asm volatile(REGS("movdqu", "[pat]") BODY STORE("movdqu", "[out]")
			     :: [pat] "r"(pat), [out] "r"(out), [nsys] "r"(nsys), [spin] "r"(spin)
			     : CLOBBERS);
#undef R
	}
}

/*
 * Set (check == 0) or check this thread's FP control state: MXCSR rounds
 * toward zero or up, the x87 control word sets 53-bit precision and
 * varies its rounding.  Returns 0 if the check finds it changed.
 */
static int fp_control(long id, int check, char *what, size_t len)
{
	unsigned int mxcsr = 0x7f80 ^ ((id & 1) << 13), m;
	unsigned short fcw = 0x027f | ((id & 1) << 10), c;

	if (!check) {
		asm volatile("ldmxcsr %0" :: "m"(mxcsr));
		asm volatile("fldcw %0" :: "m"(fcw));
		return 1;
	}
	asm volatile("stmxcsr %0" : "=m"(m));
	asm volatile("fnstcw %0" : "=m"(c));
	snprintf(what, len, "mxcsr %#x fcw %#x", m, c);
	return (m & ~0x3fU) == (mxcsr & ~0x3fU) && c == fcw;
}

static int avx_usable(void)
{
	unsigned int a, b, c, d, lo, hi;

	if (!__get_cpuid(1, &a, &b, &c, &d) || !(c & (1u << 27)) || !(c & (1u << 28)))
		return 0;
	asm volatile("xgetbv" : "=a"(lo), "=d"(hi) : "c"(0));
	return (lo & 6) == 6;
}
#else
#define NREGS		32
#define REG_NAME	"v"
#define REG_WIDTH	16

/* As on x86_64, for V0-V31 (16 bytes each, in 32-byte slots). */
static void simd_round(const unsigned char *pat, unsigned char *out, int avx, int nsys, int spin)
{
#define LD(n) "ld1 {v" #n ".16b}, [x9]\n\tadd x9, x9, #32\n\t"
#define ST(n) "st1 {v" #n ".16b}, [x9]\n\tadd x9, x9, #32\n\t"
#define ALL(m) m(0) m(1) m(2) m(3) m(4) m(5) m(6) m(7) m(8) m(9) m(10) m(11) m(12) \
	m(13) m(14) m(15) m(16) m(17) m(18) m(19) m(20) m(21) m(22) m(23) m(24) \
	m(25) m(26) m(27) m(28) m(29) m(30) m(31)
	(void)avx;
	asm volatile("mov x9, %[pat]\n\t" ALL(LD)
		     "mov x10, %[nsys]\n\t"
		     "1: cbz x10, 2f\n\t"
		     "mov x8, #173\n\t"	/* getppid */
		     "svc #0\n\t"
		     "sub x10, x10, #1\n\t"
		     "b 1b\n\t"
		     "2: mov x10, %[spin]\n\t"
		     "3: cbz x10, 4f\n\t"
		     "yield\n\t"
		     "sub x10, x10, #1\n\t"
		     "b 3b\n\t"
		     "4: mov x9, %[out]\n\t" ALL(ST)
		     :: [pat] "r"(pat), [out] "r"(out), [nsys] "r"((long)nsys),
			[spin] "r"((long)spin)
		     : "x0", "x8", "x9", "x10", "memory",
		       "v0", "v1", "v2", "v3", "v4", "v5", "v6", "v7", "v8", "v9", "v10",
		       "v11", "v12", "v13", "v14", "v15", "v16", "v17", "v18", "v19", "v20",
		       "v21", "v22", "v23", "v24", "v25", "v26", "v27", "v28", "v29", "v30",
		       "v31");
}

/* As on x86_64, for FPCR: rounding toward +infinity, or toward zero with
 * flush-to-zero. */
static int fp_control(long id, int check, char *what, size_t len)
{
	unsigned long want = (id & 1) ? (3UL << 22) | (1UL << 24) : 1UL << 22, fpcr;

	if (!check) {
		asm volatile("msr fpcr, %0" :: "r"(want));
		return 1;
	}
	asm volatile("mrs %0, fpcr" : "=r"(fpcr));
	snprintf(what, len, "fpcr %#lx", fpcr);
	return fpcr == want;
}

static int avx_usable(void)
{
	return 0;
}
#endif

static void *simd_checker(void *arg)
{
	long id = (long)arg;
	unsigned char pat[NREGS * 32] __attribute__((aligned(32)));
	unsigned char out[NREGS * 32] __attribute__((aligned(32)));
	uint64_t x = 0x9e3779b97f4a7c15ULL * (id + 1);
	long checks = 0, errors = 0;
	char what[64];

	fp_control(id, 0, NULL, 0);
	for (int round = 0; !atomic_load_explicit(&stop, memory_order_relaxed); round++) {
		for (size_t i = 0; i < sizeof(pat); i += 8) {
			x ^= x << 13;
			x ^= x >> 7;
			x ^= x << 17;
			memcpy(pat + i, &x, 8);
		}
		memset(out, 0, sizeof(out));
		simd_round(pat, out, have_avx, round & 1 ? 1000 : 0, round & 1 ? 0 : 50000);
		for (int r = 0; r < NREGS; r++)
			if (memcmp(pat + r * 32, out + r * 32, REG_WIDTH)) {
				if (errors < 5)
					printf("  thread %ld round %d: %s%d corrupted\n", id, round,
					       REG_NAME, r);
				errors++;
			}
		if (!fp_control(id, 1, what, sizeof(what))) {
			if (errors < 5)
				printf("  thread %ld round %d: %s\n", id, round, what);
			errors++;
		}
		checks++;
	}
	atomic_fetch_add(&simd_checks, checks);
	atomic_fetch_add(&simd_errors, errors);
	return NULL;
}

struct loader {
	const char *path;
	int id;
	long bytes;
};

static void *crypt_loader(void *arg)
{
	struct loader *l = arg;
	size_t bs = 256 << 10;
	unsigned char *buf;
	int fd = open(l->path, O_RDWR | O_DIRECT);

	if (fd < 0 || posix_memalign((void **)&buf, 4096, bs))
		return NULL;
	for (unsigned long i = 0; !atomic_load_explicit(&stop, memory_order_relaxed); i++) {
		off_t off = (off_t)((i * 7 + l->id * 13) % 16) * bs;

		memset(buf, (int)(i + l->id), bs);
		if (rw_all(fd, buf, bs, off, i & 1))
			break;
		l->bytes += bs;
	}
	free(buf);
	close(fd);
	return NULL;
}

static void simd_stress(int seconds, int ncpu, const char *back)
{
	unsigned long long sectors = 8192;	/* 4 MiB */
	char zdev[64], ldev[64], params[512];
	pthread_t chk[64], ld[64];
	struct loader lds[64];
	int nld = 0;
	long total = 0;

	if (dm_create("st-zero", "zero", sectors, "", zdev, sizeof(zdev))) {
		FAIL("create dm-zero");
		return;
	}
	snprintf(params, sizeof(params), "%s %s 0 %s 0", CIPHER_ACCEL, key, zdev);
	if (dm_create("st-crypt-zero", "crypt", sectors, params, zdev, sizeof(zdev))) {
		FAIL("create crypt over dm-zero");
		return;
	}
	/* Over the loop device, decryption can also run from I/O completion. */
	snprintf(params, sizeof(params), "%s %s 0 %s 0 2 no_read_workqueue no_write_workqueue",
		 CIPHER_ACCEL, key, back);
	if (dm_create("st-crypt-loop", "crypt", sectors, params, ldev, sizeof(ldev))) {
		FAIL("create crypt over loop");
		return;
	}

	atomic_store(&stop, 0);
	for (long i = 0; i < ncpu; i++)
		pthread_create(&chk[i], NULL, simd_checker, (void *)i);
	for (int i = 0; i < ncpu && nld < 64; i++) {
		for (int k = 0; k < 2; k++) {
			lds[nld] = (struct loader){ k ? ldev : zdev, nld, 0 };
			pthread_create(&ld[nld], NULL, crypt_loader, &lds[nld]);
			nld++;
		}
	}
	sleep(seconds);
	atomic_store(&stop, 1);
	for (int i = 0; i < ncpu; i++)
		pthread_join(chk[i], NULL);
	for (int i = 0; i < nld; i++) {
		pthread_join(ld[i], NULL);
		total += lds[i].bytes;
	}
	printf("  SIMD stress: %d checker threads (%s), %d dm-crypt threads, %ds: "
	       "%ld checks, %ld errors, %ld MiB through dm-crypt\n",
	       ncpu, NREGS == 32 ? "V" : have_avx ? "YMM" : "XMM", nld, seconds, atomic_load(&simd_checks),
	       atomic_load(&simd_errors), total >> 20);
	if (atomic_load(&simd_errors))
		FAIL("user SIMD state corrupted");
	if (!atomic_load(&simd_checks) || total == 0)
		FAIL("stress made no progress");
	dm_remove("st-crypt-loop");
	dm_remove("st-crypt-zero");
	dm_remove("st-zero");
}

int main(int argc, char **argv)
{
	int seconds = argc > 1 ? atoi(argv[1]) : 10;
	size_t mib = argc > 2 ? strtoul(argv[2], NULL, 0) : 512;
	int ncpu = sysconf(_SC_NPROCESSORS_ONLN);
	const char *back = argc > 3 ? argv[3] : "/dev/loop0";

	setvbuf(stdout, NULL, _IOLBF, 0);
	if (ncpu > 32)
		ncpu = 32;
	have_avx = avx_usable();
#ifdef __x86_64__
	printf("CRYPTOTEST: start, %d vCPU(s), AVX %s\n", ncpu, have_avx ? "yes" : "no");
#else
	printf("CRYPTOTEST: start, %d vCPU(s), AES %s\n", ncpu,
	       getauxval(AT_HWCAP) & HWCAP_AES ? "yes" : "no");
#endif
	printf("proc_crypto:\n");
	test_proc_crypto();
	printf("cross_check:\n");
	cross_check(back, 8192);
	printf("bench:\n");
	bench(mib, ncpu);
	printf("simd_stress:\n");
	simd_stress(seconds, ncpu, back);
	printf(failures ? "CRYPTOTEST: FAIL (%d)\n" : "CRYPTOTEST: PASS\n", failures);
	return failures != 0;
}
