// Micro-benchmarks run inside an l4h guest by tests/run-bench.sh.
#define _GNU_SOURCE
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

static double now(void)
{
	struct timespec ts;
	clock_gettime(CLOCK_MONOTONIC, &ts);
	return ts.tv_sec + ts.tv_nsec * 1e-9;
}

static void report(const char *what, double per, const char *unit)
{
	printf("BENCH: %-22s %10.2f %s\n", what, per, unit);
}

/* CPU time of all vCPUs from /proc/stat, in clock ticks: busy (everything
 * but idle, iowait and steal) and elapsed (everything but steal). */
static void cpu_ticks(unsigned long long *busy, unsigned long long *all)
{
	unsigned long long v[8] = { 0 };
	FILE *f = fopen("/proc/stat", "r");

	*busy = *all = 0;
	if (!f)
		return;
	if (fscanf(f, "cpu %llu %llu %llu %llu %llu %llu %llu %llu", &v[0], &v[1], &v[2], &v[3],
		   &v[4], &v[5], &v[6], &v[7]) == 8) {
		*busy = v[0] + v[1] + v[2] + v[5] + v[6];
		*all = *busy + v[3] + v[4];
	}
	fclose(f);
}

static unsigned long long cpu_busy0, cpu_all0;

static void cpu_start(void)
{
	cpu_ticks(&cpu_busy0, &cpu_all0);
}

/* Share of the guest's CPU time that was busy since cpu_start(). */
static void cpu_report(const char *what)
{
	unsigned long long busy, all;

	cpu_ticks(&busy, &all);
	if (all > cpu_all0)
		report(what, 100.0 * (busy - cpu_busy0) / (all - cpu_all0), "% CPU");
}

static void bench_syscall(void)
{
	int n = 200000;
	double t = now();
	for (int i = 0; i < n; i++)
		syscall(SYS_getpid);
	report("getpid", (now() - t) / n * 1e9, "ns");
}

static void bench_fault(void)
{
	size_t sz = 64 << 20;
	char *p = mmap(NULL, sz, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
	double t = now();
	for (size_t o = 0; o < sz; o += 4096)
		p[o] = 1;
	report("page fault (touch)", (now() - t) / (sz / 4096) * 1e6, "us");
	munmap(p, sz);
}

static void bench_fork(void)
{
	int n = 200;
	double t = now();
	for (int i = 0; i < n; i++) {
		pid_t pid = fork();
		if (pid == 0)
			_exit(0);
		waitpid(pid, NULL, 0);
	}
	report("fork+exit+wait", (now() - t) / n * 1e6, "us");
}

static void bench_pipe(void)
{
	int a[2], b[2], n = 20000;
	char c = 0;
	if (pipe(a) || pipe(b))
		return;
	pid_t pid = fork();
	if (pid == 0) {
		for (int i = 0; i < n; i++) {
			if (read(a[0], &c, 1) != 1 || write(b[1], &c, 1) != 1)
				_exit(1);
		}
		_exit(0);
	}
	double t = now();
	for (int i = 0; i < n; i++) {
		if (write(a[1], &c, 1) != 1 || read(b[0], &c, 1) != 1)
			break;
	}
	report("pipe round trip", (now() - t) / n * 1e6, "us");
	waitpid(pid, NULL, 0);
}

static void bench_disk(const char *dev)
{
	size_t bs = 1 << 20, total = 256 << 20;
	char *buf = aligned_alloc(4096, bs);
	int fd = open(dev, O_RDWR | O_DIRECT);
	if (fd < 0) {
		printf("BENCH: no %s\n", dev);
		return;
	}
	memset(buf, 0x5a, bs);
	cpu_start();
	double t = now();
	for (size_t o = 0; o < total; o += bs)
		if (pwrite(fd, buf, bs, o) != (ssize_t)bs)
			break;
	fsync(fd);
	report("disk write (1 MiB, direct)", total / (now() - t) / 1e6, "MB/s");
	cpu_report("disk write busy");
	cpu_start();
	t = now();
	for (size_t o = 0; o < total; o += bs)
		if (pread(fd, buf, bs, o) != (ssize_t)bs)
			break;
	report("disk read (1 MiB, direct)", total / (now() - t) / 1e6, "MB/s");
	cpu_report("disk read busy");
	int n = 2000;
	t = now();
	for (int i = 0; i < n; i++)
		if (pread(fd, buf, 4096, (size_t)(i * 7919 % 60000) * 4096) != 4096)
			break;
	report("disk 4K read latency", (now() - t) / n * 1e6, "us");
	close(fd);
}

int main(int argc, char **argv)
{
	bench_syscall();
	bench_fault();
	bench_fork();
	bench_pipe();
	if (argc > 1)
		bench_disk(argv[1]);
	return 0;
}
