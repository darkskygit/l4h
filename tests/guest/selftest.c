/*
 * l4h guest self-test: exercises the Linux user ABI as implemented by the
 * l4h guest kernel.  Built as a static x86_64 binary and run inside a
 * guest; prints "SELFTEST: PASS" or "SELFTEST: FAIL <test>".
 */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <pthread.h>
#include <sched.h>
#include <setjmp.h>
#include <signal.h>
#include <stdatomic.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/personality.h>
#include <sys/syscall.h>
#include <sys/time.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>
#include <linux/futex.h>

static int failures;

#define CHECK(cond, ...) do { \
	if (!(cond)) { \
		printf("  FAIL %s:%d: ", __func__, __LINE__); \
		printf(__VA_ARGS__); \
		printf("\n"); \
		failures++; \
		return; \
	} \
} while (0)

static double now(void)
{
	struct timespec ts;

	clock_gettime(CLOCK_MONOTONIC, &ts);
	return ts.tv_sec + ts.tv_nsec / 1e9;
}

/* ---- syscalls and registers */
static void test_syscall_regs(void)
{
	/* getpid through the raw syscall path with all argument registers
	 * holding sentinel values. */
	long r;
	register long r10 asm("r10") = 0x1010101010101010;
	register long r8 asm("r8") = 0x0808080808080808;
	register long r9 asm("r9") = 0x0909090909090909;
	long rbx_before = 0x5555aaaa5555aaaa, rbx_after;

	asm volatile("movq %[b], %%rbx\n\tsyscall\n\tmovq %%rbx, %[a]"
		     : "=a"(r), [a] "=r"(rbx_after), "+r"(r10), "+r"(r8), "+r"(r9)
		     : "0"((long)SYS_getpid), "D"(-1L), "S"(-2L), "d"(-3L), [b] "r"(rbx_before)
		     : "rcx", "r11", "rbx", "memory");
	CHECK(r == getpid(), "getpid returned %ld", r);
	CHECK(rbx_after == rbx_before, "rbx clobbered");
	CHECK(r10 == 0x1010101010101010 && r8 == 0x0808080808080808 && r9 == 0x0909090909090909,
	      "argument registers clobbered");
}

/* ---- memory */
static void test_mmap(void)
{
	size_t len = 16 << 20;
	unsigned char *p = mmap(NULL, len, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
	size_t i;

	CHECK(p != MAP_FAILED, "mmap: %s", strerror(errno));
	for (i = 0; i < len; i += 4096)
		p[i] = (unsigned char)(i >> 12);
	for (i = 0; i < len; i += 4096)
		CHECK(p[i] == (unsigned char)(i >> 12), "mismatch at %zu", i);
	CHECK(mprotect(p, len, PROT_READ) == 0, "mprotect");
	CHECK(p[4096] == 1, "read after mprotect");
	CHECK(munmap(p, len) == 0, "munmap");
}

/* Kernel copies to and from user memory honour its protection. */
static void test_uaccess_prot(void)
{
	unsigned char *p = mmap(NULL, 2 * 4096, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
	char buf[16];
	int fds[2];
	ssize_t r;

	CHECK(p != MAP_FAILED, "mmap: %s", strerror(errno));
	CHECK(pipe(fds) == 0, "pipe: %s", strerror(errno));
	memcpy(p, "protected data", 15);
	CHECK(mprotect(p, 4096, PROT_NONE) == 0, "mprotect: %s", strerror(errno));
	errno = 0;
	r = write(fds[1], p, 15);
	CHECK(r == -1 && errno == EFAULT, "write from a PROT_NONE page: %zd (%s)", r, strerror(errno));
	/* Never touched before PROT_NONE. */
	CHECK(mprotect(p + 4096, 4096, PROT_NONE) == 0, "mprotect: %s", strerror(errno));
	errno = 0;
	r = write(fds[1], p + 4096, 15);
	CHECK(r == -1 && errno == EFAULT, "write from an untouched PROT_NONE page: %zd (%s)", r,
	      strerror(errno));
	/* Read-only: the kernel cannot write into it either. */
	CHECK(mprotect(p, 4096, PROT_READ) == 0, "mprotect: %s", strerror(errno));
	CHECK(write(fds[1], "0123456789abcde", 15) == 15, "write: %s", strerror(errno));
	errno = 0;
	r = read(fds[0], p, 15);
	CHECK(r == -1 && errno == EFAULT, "read into a PROT_READ page: %zd (%s)", r, strerror(errno));
	CHECK(memcmp(p, "protected data", 15) == 0, "PROT_READ page changed");
	CHECK(write(fds[1], p, 15) == 15, "write from a PROT_READ page: %s", strerror(errno));
	r = read(fds[0], buf, sizeof(buf));
	CHECK(r == 16, "read back: %zd", r);
	CHECK(memcmp(buf, "0123456789abcde", 15) == 0, "pipe data");
	close(fds[0]);
	close(fds[1]);
	munmap(p, 2 * 4096);
}

/*
 * Kernel copies race with another thread dropping the pages (and other
 * threads reusing freed pages): a copy sees the buffer's data or zeroes
 * (MADV_DONTNEED), never another page's, and does not fail (an unlocked
 * walk losing the race returned EFAULT).
 */
static unsigned char *volatile race_buf;
static atomic_int race_stop;

static void *uaccess_dropper(void *arg)
{
	while (!atomic_load(&race_stop)) {
		memset(race_buf, 0xa5, 4 * 4096);
		madvise(race_buf, 4 * 4096, MADV_DONTNEED);
	}
	return NULL;
}

static void *uaccess_reuser(void *arg)
{
	while (!atomic_load(&race_stop)) {
		unsigned char *q = mmap(NULL, 16 * 4096, PROT_READ | PROT_WRITE,
					MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);

		if (q == MAP_FAILED)
			continue;
		memset(q, 0x5a, 16 * 4096);
		munmap(q, 16 * 4096);
	}
	return NULL;
}

static void test_uaccess_race(void)
{
	static unsigned char out[4 * 4096];
	pthread_t dropper, reuser;
	int fds[2], i, bad = 0;
	size_t j;

	race_buf = mmap(NULL, 4 * 4096, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
	CHECK(race_buf != MAP_FAILED, "mmap: %s", strerror(errno));
	CHECK(pipe(fds) == 0, "pipe: %s", strerror(errno));
	atomic_store(&race_stop, 0);
	pthread_create(&dropper, NULL, uaccess_dropper, NULL);
	pthread_create(&reuser, NULL, uaccess_reuser, NULL);
	for (i = 0; i < 2000 && !bad; i++) {
		ssize_t n = write(fds[1], race_buf, sizeof(out)), got = 0;

		if (n <= 0) {
			bad = -1;
			break;
		}
		while (got < n) {
			ssize_t m = read(fds[0], out + got, n - got);

			if (m <= 0)
				break;
			got += m;
		}
		for (j = 0; j < (size_t)got; j++)
			if (out[j] != 0xa5 && out[j] != 0)
				bad = out[j];
	}
	atomic_store(&race_stop, 1);
	pthread_join(dropper, NULL);
	pthread_join(reuser, NULL);
	close(fds[0]);
	close(fds[1]);
	munmap(race_buf, 4 * 4096);
	CHECK(bad == 0, "copy from user saw byte %#x (write: %s)", bad, bad < 0 ? strerror(errno) : "ok");
}

static sigjmp_buf segv_jmp;
static volatile void *segv_addr;

static void segv_handler(int sig, siginfo_t *si, void *ctx)
{
	segv_addr = si->si_addr;
	siglongjmp(segv_jmp, 1);
}

static void test_segv(void)
{
	struct sigaction sa = { .sa_sigaction = segv_handler, .sa_flags = SA_SIGINFO };
	unsigned char *p = mmap(NULL, 4096, PROT_READ, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);

	sigaction(SIGSEGV, &sa, NULL);
	if (sigsetjmp(segv_jmp, 1) == 0) {
		*(volatile int *)0x10 = 1;
		CHECK(0, "no fault on NULL write");
	}
	CHECK(segv_addr == (void *)0x10, "si_addr %p", segv_addr);
	if (sigsetjmp(segv_jmp, 1) == 0) {
		p[100] = 1;
		CHECK(0, "no fault on read-only write");
	}
	CHECK(segv_addr == p + 100, "si_addr %p (expected %p)", segv_addr, p + 100);
	signal(SIGSEGV, SIG_DFL);
	munmap(p, 4096);
}

/* Data pages are not executable; mprotect(PROT_EXEC) makes them so. */
static void test_nx(void)
{
	struct sigaction sa = { .sa_sigaction = segv_handler, .sa_flags = SA_SIGINFO };
	unsigned char *p = mmap(NULL, 4096, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
	int (*fn)(void) = (int (*)(void))p;

	CHECK(p != MAP_FAILED, "mmap: %s", strerror(errno));
	/* mov $42, %eax; ret */
	memcpy(p, "\xb8\x2a\x00\x00\x00\xc3", 6);
	sigaction(SIGSEGV, &sa, NULL);
	segv_addr = NULL;
	if (sigsetjmp(segv_jmp, 1) == 0) {
		fn();
		CHECK(0, "executed from a PROT_READ|PROT_WRITE page (personality %#x)", personality(0xffffffff));
	}
	CHECK(segv_addr == p, "si_addr %p (expected %p)", segv_addr, p);
	CHECK(mprotect(p, 4096, PROT_READ | PROT_EXEC) == 0, "mprotect: %s", strerror(errno));
	CHECK(fn() == 42, "PROT_EXEC page did not run");
	CHECK(mprotect(p, 4096, PROT_READ) == 0, "mprotect: %s", strerror(errno));
	segv_addr = NULL;
	if (sigsetjmp(segv_jmp, 1) == 0) {
		fn();
		CHECK(0, "executed after PROT_EXEC was removed");
	}
	CHECK(segv_addr == p, "si_addr %p (expected %p)", segv_addr, p);
	signal(SIGSEGV, SIG_DFL);
	munmap(p, 4096);
}

/* ---- fork, exec, wait, pipes, COW */
static void test_fork(void)
{
	int fds[2], status;
	char buf[64] = { 0 };
	volatile int cow = 1;
	pid_t pid;

	CHECK(pipe(fds) == 0, "pipe");
	pid = fork();
	CHECK(pid >= 0, "fork: %s", strerror(errno));
	if (pid == 0) {
		cow = 2;
		dprintf(fds[1], "child %d cow=%d", getpid(), cow);
		_exit(42);
	}
	CHECK(waitpid(pid, &status, 0) == pid, "waitpid");
	CHECK(WIFEXITED(status) && WEXITSTATUS(status) == 42, "exit status %x", status);
	CHECK(read(fds[0], buf, sizeof(buf) - 1) > 0, "read");
	CHECK(strstr(buf, "cow=2") != NULL, "child wrote '%s'", buf);
	CHECK(cow == 1, "COW broken: parent sees %d", cow);
	close(fds[0]);
	close(fds[1]);

	pid = fork();
	if (pid == 0) {
		execl("/bin/true", "true", NULL);
		_exit(99);
	}
	CHECK(waitpid(pid, &status, 0) == pid && WIFEXITED(status) && WEXITSTATUS(status) == 0,
	      "exec /bin/true: status %x", status);
}

static void test_many_forks(void)
{
	int i, status;

	for (i = 0; i < 200; i++) {
		pid_t pid = fork();

		CHECK(pid >= 0, "fork %d: %s", i, strerror(errno));
		if (pid == 0)
			_exit(i & 0x7f);
		CHECK(waitpid(pid, &status, 0) == pid && WEXITSTATUS(status) == (i & 0x7f),
		      "fork %d status", i);
	}
}

/* ---- signals and FPU state */
static volatile int sig_count;
static volatile double sig_fpu;

static void usr1_handler(int sig)
{
	/* Clobber FPU/SSE state in the handler; it must be restored. */
	volatile double x = 3.0;
	int i;

	for (i = 0; i < 100; i++)
		x = x * 1.0001 + 0.5;
	sig_fpu = x;
	sig_count++;
}

static void test_signals_fpu(void)
{
	double acc = 1.0;
	int i;

	signal(SIGUSR1, usr1_handler);
	for (i = 0; i < 1000; i++) {
		acc = acc * 1.000001 + 0.000001;
		if (i % 100 == 0)
			kill(getpid(), SIGUSR1);
	}
	CHECK(sig_count == 10, "handler ran %d times", sig_count);
	{
		double ref = 1.0;

		for (i = 0; i < 1000; i++)
			ref = ref * 1.000001 + 0.000001;
		CHECK(acc == ref, "FPU state corrupted across signals: %.17g vs %.17g", acc, ref);
	}
	signal(SIGUSR1, SIG_DFL);
}

static void alarm_handler(int sig)
{
	sig_count = 1000;
}

static void test_timers(void)
{
	struct timespec req = { 0, 50 * 1000 * 1000 };
	double t0 = now(), t1;

	nanosleep(&req, NULL);
	t1 = now();
	CHECK(t1 - t0 >= 0.049 && t1 - t0 < 0.5, "nanosleep(50ms) took %.3f s", t1 - t0);

	sig_count = 0;
	signal(SIGALRM, alarm_handler);
	struct itimerval it = { .it_value = { 0, 20000 } };
	setitimer(ITIMER_REAL, &it, NULL);
	t0 = now();
	while (sig_count != 1000 && now() - t0 < 2.0)
		;
	CHECK(sig_count == 1000, "SIGALRM did not interrupt a busy loop");
}

/* ---- threads, futexes, TLS */
static __thread int tls_value;
static atomic_int counter;
static double thread_results[8];

static void *thread_fn(void *arg)
{
	long id = (long)arg;
	double x = id;
	int i;

	tls_value = (int)id * 10;
	for (i = 0; i < 2000000; i++) {
		x = x * 0.999999 + 1.0;
		if ((i & 1023) == 0)
			atomic_fetch_add(&counter, 1);
	}
	sched_yield();
	thread_results[id] = x + (tls_value == id * 10 ? 0 : 1e9);
	return NULL;
}

static void test_threads(void)
{
	pthread_t th[8];
	long i;
	int n = 8;

	atomic_store(&counter, 0);
	for (i = 0; i < n; i++)
		CHECK(pthread_create(&th[i], NULL, thread_fn, (void *)i) == 0, "pthread_create");
	for (i = 0; i < n; i++)
		pthread_join(th[i], NULL);
	CHECK(atomic_load(&counter) == n * (2000000 / 1024 + 1), "counter %d", atomic_load(&counter));
	for (i = 0; i < n; i++) {
		double ref = i;
		int k;

		for (k = 0; k < 2000000; k++)
			ref = ref * 0.999999 + 1.0;
		CHECK(thread_results[i] == ref, "thread %ld FPU/TLS result %.17g vs %.17g", i,
		      thread_results[i], ref);
	}
}

static atomic_int futex_word;

static void *futex_waker(void *arg)
{
	usleep(10000);
	atomic_store(&futex_word, 1);
	syscall(SYS_futex, &futex_word, FUTEX_WAKE, 1, NULL, NULL, 0);
	return NULL;
}

static void test_futex(void)
{
	pthread_t t;

	atomic_store(&futex_word, 0);
	pthread_create(&t, NULL, futex_waker, NULL);
	while (atomic_load(&futex_word) == 0)
		syscall(SYS_futex, &futex_word, FUTEX_WAIT, 0, NULL, NULL, 0);
	pthread_join(t, NULL);
	CHECK(atomic_load(&futex_word) == 1, "futex");
}

static void test_files(void)
{
	char buf[4096];
	int fd = open("/tmp/selftest", O_CREAT | O_RDWR | O_TRUNC, 0644);
	int i;

	CHECK(fd >= 0, "open: %s", strerror(errno));
	memset(buf, 'x', sizeof(buf));
	for (i = 0; i < 256; i++)
		CHECK(write(fd, buf, sizeof(buf)) == sizeof(buf), "write");
	CHECK(lseek(fd, 0, SEEK_SET) == 0, "lseek");
	for (i = 0; i < 256; i++)
		CHECK(read(fd, buf, sizeof(buf)) == sizeof(buf) && buf[17] == 'x', "read");
	close(fd);
	unlink("/tmp/selftest");
}

/* ---- SMP stress: concurrent faults, mmap/munmap and forks */
static atomic_int stress_errors;

static void *mm_stress_fn(void *arg)
{
	long id = (long)arg;
	int round;

	for (round = 0; round < 200; round++) {
		size_t len = (1 + (round % 7)) * 65536;
		volatile unsigned char *p = mmap(NULL, len, PROT_READ | PROT_WRITE,
						 MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
		size_t i;

		if (p == MAP_FAILED) {
			atomic_fetch_add(&stress_errors, 1);
			continue;
		}
		for (i = 0; i < len; i += 4096)
			p[i] = (unsigned char)(id + i);
		for (i = 0; i < len; i += 4096)
			if (p[i] != (unsigned char)(id + i))
				atomic_fetch_add(&stress_errors, 1);
		if (round & 1)
			mprotect((void *)p, len, PROT_READ);
		munmap((void *)p, len);
	}
	return NULL;
}

static void test_smp_mm_stress(void)
{
	pthread_t th[8];
	long i;

	atomic_store(&stress_errors, 0);
	for (i = 0; i < 8; i++)
		pthread_create(&th[i], NULL, mm_stress_fn, (void *)i);
	for (i = 0; i < 8; i++)
		pthread_join(th[i], NULL);
	CHECK(atomic_load(&stress_errors) == 0, "%d errors", atomic_load(&stress_errors));
}

/* A use-after-munmap from another thread must fault, not read stale data. */
static volatile unsigned char *shared_page;
static atomic_int phase;

static void *unmap_reader(void *arg)
{
	struct sigaction sa = { .sa_sigaction = segv_handler, .sa_flags = SA_SIGINFO };

	sigaction(SIGSEGV, &sa, NULL);
	while (atomic_load(&phase) == 0)
		(void)shared_page[0];
	atomic_store(&phase, 2);
	while (atomic_load(&phase) != 3)
		;
	if (sigsetjmp(segv_jmp, 1) == 0) {
		(void)shared_page[0];
		atomic_store(&phase, 5);	/* no fault: stale mapping */
	} else {
		atomic_store(&phase, 4);
	}
	return NULL;
}

static void test_smp_unmap_visibility(void)
{
	pthread_t t;

	shared_page = mmap(NULL, 4096, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
	shared_page[0] = 7;
	atomic_store(&phase, 0);
	pthread_create(&t, NULL, unmap_reader, NULL);
	usleep(10000);
	atomic_store(&phase, 1);
	while (atomic_load(&phase) != 2)
		;
	munmap((void *)shared_page, 4096);
	atomic_store(&phase, 3);
	pthread_join(t, NULL);
	signal(SIGSEGV, SIG_DFL);
	CHECK(atomic_load(&phase) == 4, "reader saw a stale mapping after munmap");
}

static void *fork_storm_fn(void *arg)
{
	int i, status;

	for (i = 0; i < 50; i++) {
		pid_t pid = fork();

		if (pid == 0) {
			volatile char buf[65536];

			buf[0] = 1;
			buf[65535] = buf[0];
			_exit(3);
		}
		if (pid < 0 || waitpid(pid, &status, 0) != pid || WEXITSTATUS(status) != 3)
			atomic_fetch_add(&stress_errors, 1);
	}
	return NULL;
}

static void test_smp_fork_storm(void)
{
	pthread_t th[4];
	long i;

	atomic_store(&stress_errors, 0);
	for (i = 0; i < 4; i++)
		pthread_create(&th[i], NULL, fork_storm_fn, NULL);
	for (i = 0; i < 4; i++)
		pthread_join(th[i], NULL);
	CHECK(atomic_load(&stress_errors) == 0, "%d errors", atomic_load(&stress_errors));
}

/* More live processes than the guest has seL4 VSpaces (64): sleeping ones
 * lose theirs and get one back when they run again. */
static void test_vspace_evict(void)
{
	enum { N = 96 };
	int fds[N][2], i, status, ok = 1;
	pid_t pids[N];

	for (i = 0; i < N; i++) {
		CHECK(pipe(fds[i]) == 0, "pipe: %s", strerror(errno));
		pids[i] = fork();
		CHECK(pids[i] >= 0, "fork %d: %s", i, strerror(errno));
		if (pids[i] == 0) {
			char c;
			volatile int touch[1024];

			touch[i] = i;
			if (read(fds[i][0], &c, 1) != 1)
				_exit(1);
			_exit(touch[i] == i && c == 'x' ? 0 : 2);
		}
	}
	/* Wake them in turn: each one's VSpace was probably given away. */
	for (i = 0; i < N; i++) {
		if (write(fds[i][1], "x", 1) != 1)
			ok = 0;
		if (waitpid(pids[i], &status, 0) != pids[i] || !WIFEXITED(status) || WEXITSTATUS(status))
			ok = 0;
		close(fds[i][0]);
		close(fds[i][1]);
	}
	CHECK(ok, "a child failed");
}

/* FUTEX_WAKE_OP updates the second word atomically, while user threads on
 * other vCPUs update it with atomic instructions. */
static atomic_int wake_op_word;
static atomic_int wake_op_running;

static void *wake_op_fn(void *arg)
{
	int i, dummy = 0;

	for (i = 0; i < 20000; i++)
		syscall(SYS_futex, &dummy, FUTEX_WAKE_OP_PRIVATE, 0, NULL, &wake_op_word,
			FUTEX_OP(FUTEX_OP_ADD, 1, FUTEX_OP_CMP_EQ, 0));
	atomic_fetch_sub(&wake_op_running, 1);
	return NULL;
}

static void *wake_op_user_fn(void *arg)
{
	long n = 0;

	while (atomic_load(&wake_op_running)) {
		atomic_fetch_add(&wake_op_word, 1);
		n++;
	}
	return (void *)n;
}

static void test_futex_wake_op(void)
{
	pthread_t th[4];
	long total = 2 * 20000;
	void *n;
	int i;

	atomic_store(&wake_op_word, 0);
	atomic_store(&wake_op_running, 2);
	for (i = 0; i < 2; i++)
		pthread_create(&th[i], NULL, wake_op_fn, NULL);
	for (i = 2; i < 4; i++)
		pthread_create(&th[i], NULL, wake_op_user_fn, NULL);
	for (i = 0; i < 4; i++) {
		pthread_join(th[i], &n);
		if (i >= 2)
			total += (long)n;
	}
	CHECK(atomic_load(&wake_op_word) == (int)total, "counter %d, expected %ld (updates lost)",
	      atomic_load(&wake_op_word), total);
}

/* sched_getcpu() (the vDSO) agrees with the CPU a thread is pinned to. */
static void test_getcpu(void)
{
	long n = sysconf(_SC_NPROCESSORS_ONLN), cpu;
	cpu_set_t set, old;

	sched_getaffinity(0, sizeof(old), &old);
	for (cpu = 0; cpu < n; cpu++) {
		unsigned int c = ~0u;
		int vdso;

		CPU_ZERO(&set);
		CPU_SET(cpu, &set);
		sched_setaffinity(0, sizeof(set), &set);
		vdso = sched_getcpu();
		syscall(SYS_getcpu, &c, NULL, NULL);
		/* Unpin before checking: later tests must not run pinned. */
		sched_setaffinity(0, sizeof(old), &old);
		CHECK(c == cpu, "getcpu syscall %u on CPU %ld", c, cpu);
		CHECK(vdso == cpu, "sched_getcpu %d on CPU %ld", vdso, cpu);
	}
}

struct test {
	const char *name;
	void (*fn)(void);
};

static const struct test tests[] = {
	{ "syscall_regs", test_syscall_regs },
	{ "mmap", test_mmap },
	{ "segv", test_segv },
	{ "nx", test_nx },
	{ "uaccess_prot", test_uaccess_prot },
	{ "fork", test_fork },
	{ "many_forks", test_many_forks },
	{ "signals_fpu", test_signals_fpu },
	{ "timers", test_timers },
	{ "threads", test_threads },
	{ "futex", test_futex },
	{ "futex_wake_op", test_futex_wake_op },
	{ "getcpu", test_getcpu },
	{ "files", test_files },
	{ "smp_mm_stress", test_smp_mm_stress },
	{ "smp_unmap_vis", test_smp_unmap_visibility },
	{ "uaccess_race", test_uaccess_race },
	{ "smp_fork_storm", test_smp_fork_storm },
	{ "vspace_evict", test_vspace_evict },
};

int main(int argc, char **argv)
{
	unsigned i;

	setvbuf(stdout, NULL, _IONBF, 0);
	printf("SELFTEST: start (%ld CPUs online)\n", sysconf(_SC_NPROCESSORS_ONLN));
	for (i = 0; i < sizeof(tests) / sizeof(tests[0]); i++) {
		int before = failures;
		double t0 = now();

		tests[i].fn();
		printf("  %-14s %s (%.3f s)\n", tests[i].name, failures == before ? "ok" : "FAILED",
		       now() - t0);
	}
	if (failures)
		printf("SELFTEST: FAIL (%d)\n", failures);
	else
		printf("SELFTEST: PASS\n");
	return failures != 0;
}
