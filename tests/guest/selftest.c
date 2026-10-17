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

struct test {
	const char *name;
	void (*fn)(void);
};

static const struct test tests[] = {
	{ "syscall_regs", test_syscall_regs },
	{ "mmap", test_mmap },
	{ "segv", test_segv },
	{ "fork", test_fork },
	{ "many_forks", test_many_forks },
	{ "signals_fpu", test_signals_fpu },
	{ "timers", test_timers },
	{ "threads", test_threads },
	{ "futex", test_futex },
	{ "files", test_files },
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
