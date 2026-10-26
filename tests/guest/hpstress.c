// SPDX-License-Identifier: GPL-2.0
/*
 * Memory workload for tests/run-hotplugstress.sh, while the host plugs
 * and unplugs the guest's memory: threads that map, fill, check and unmap
 * anonymous memory sized to what is available, and a long-lived region
 * that is checked over and over, whose pages unplugging migrates.  Every
 * word holds a function of its address and a generation, so a page that
 * moved with stale or foreign contents shows.  SIGUSR1 prints the counts;
 * SIGUSR2 stops the churning threads, leaving the long-lived region.
 */
#define _GNU_SOURCE
#include <pthread.h>
#include <signal.h>
#include <stdatomic.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <unistd.h>

enum { THREADS = 3, KEEP_MIB = 48, WORDS_PER_PAGE = 4096 / sizeof(uint64_t) };

static atomic_ulong rounds, checks, bad, maps_failed;
static atomic_int quiet;

static uint64_t pattern(const volatile uint64_t *p, uint64_t gen)
{
	uint64_t x = (uintptr_t)p ^ (gen * 0x9e3779b97f4a7c15ull);

	return x ^ (x >> 29);
}

/* Fill one word per cache line: enough to catch a wrong page. */
static void fill(volatile uint64_t *m, size_t words, uint64_t gen)
{
	for (size_t i = 0; i < words; i += 8)
		m[i] = pattern(&m[i], gen);
}

static int check(volatile uint64_t *m, size_t words, uint64_t gen)
{
	for (size_t i = 0; i < words; i += 8) {
		if (m[i] != pattern(&m[i], gen)) {
			printf("HPSTRESS: CORRUPT at %p: %#llx, want %#llx\n", (void *)&m[i],
			       (unsigned long long)m[i], (unsigned long long)pattern(&m[i], gen));
			fflush(stdout);
			return 1;
		}
	}
	return 0;
}

/* MemAvailable in bytes. */
static size_t available(void)
{
	FILE *f = fopen("/proc/meminfo", "r");
	char line[128];
	unsigned long kb = 0;

	while (f && fgets(line, sizeof(line), f))
		if (sscanf(line, "MemAvailable: %lu kB", &kb) == 1)
			break;
	if (f)
		fclose(f);
	return (size_t)kb << 10;
}

static void *churn(void *arg)
{
	uint64_t gen = (uintptr_t)arg * 1000003;

	while (!quiet) {
		/* A share of what is available, so that unplugging can still
		 * find room and the OOM killer stays away. */
		size_t len = (available() / 3 / THREADS) & ~(size_t)0xfffff;

		if (len < (4u << 20))
			len = 4u << 20;
		volatile uint64_t *m = mmap(NULL, len, PROT_READ | PROT_WRITE,
					    MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
		if (m == MAP_FAILED) {
			maps_failed++;
			usleep(100000);
			continue;
		}
		gen++;
		fill(m, len / 8, gen);
		usleep(200000);
		bad += check(m, len / 8, gen);
		munmap((void *)m, len);
		rounds++;
	}
	return NULL;
}

static void calm(int sig)
{
	(void)sig;
	quiet = 1;
}

static void report(int sig)
{
	char buf[160];
	int n = snprintf(buf, sizeof(buf), "HPSTRESS: rounds %lu checks %lu bad %lu mapfail %lu\n",
			 rounds, checks, bad, maps_failed);

	(void)sig;
	write(1, buf, n);
}

int main(void)
{
	size_t words = ((size_t)KEEP_MIB << 20) / 8;
	volatile uint64_t *keep = mmap(NULL, words * 8, PROT_READ | PROT_WRITE,
				       MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
	pthread_t t;

	if (keep == MAP_FAILED) {
		puts("HPSTRESS: CORRUPT: no memory for the kept region");
		return 1;
	}
	signal(SIGUSR1, report);
	signal(SIGUSR2, calm);
	fill(keep, words, 1);
	for (uintptr_t i = 0; i < THREADS; i++)
		pthread_create(&t, NULL, churn, (void *)(i + 1));
	puts("HPSTRESS: running");
	fflush(stdout);
	for (;;) {
		bad += check(keep, words, 1);
		checks++;
		usleep(100000);
	}
}
