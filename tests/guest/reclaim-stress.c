// SPDX-License-Identifier: GPL-2.0
/* Four checking threads, 128 MiB total, 75% read-only hot-set accesses.
 * Run in a 64 MiB memcg with swap; see tests/run-reclaimtest.sh. */
#define _GNU_SOURCE
#include <errno.h>
#include <pthread.h>
#include <stdatomic.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/resource.h>
#include <time.h>
#include <unistd.h>

enum { THREADS = 4, PAGES = 32768, HOT = 8192, STRIDE = 4096 / sizeof(uint64_t) };
static volatile uint64_t *mem;
static atomic_int stop, bad;
static uint64_t operations[THREADS];

static int swap_counts(unsigned long long *in, unsigned long long *out)
{
	FILE *f = fopen("/proc/vmstat", "r");
	char name[80];
	unsigned long long value;
	int found = 0;

	if (!f)
		return -1;
	while (fscanf(f, "%79s %llu", name, &value) == 2) {
		if (!strcmp(name, "pswpin")) {
			*in = value;
			found++;
		} else if (!strcmp(name, "pswpout")) {
			*out = value;
			found++;
		}
	}
	fclose(f);
	return found == 2 ? 0 : -1;
}

static uint64_t random_next(uint64_t *state)
{
	uint64_t x = *state;

	x ^= x << 13;
	x ^= x >> 7;
	x ^= x << 17;
	return *state = x;
}

static void *worker(void *arg)
{
	long id = (long)arg;
	uint64_t state = id + 1, ops = 0;
	unsigned long base = id * (PAGES / THREADS);

	while (!atomic_load_explicit(&stop, memory_order_relaxed)) {
		uint64_t r = random_next(&state);
		int read_only = (r & 3) != 0;
		unsigned long page = base + (read_only ? (r >> 2) % (HOT / THREADS) :
			HOT / THREADS + (r >> 2) % ((PAGES - HOT) / THREADS));
		volatile uint64_t *p = mem + page * STRIDE;

		if (p[0] != page || p[2] != ~p[1]) {
			atomic_store(&bad, 1);
			atomic_store(&stop, 1);
			break;
		}
		if (!read_only) {
			p[1] = r;
			p[2] = ~r;
		}
		ops++;
	}
	operations[id] = ops;
	return NULL;
}

int main(int argc, char **argv)
{
	int seconds = argc > 1 ? atoi(argv[1]) : 120;
	pthread_t threads[THREADS];
	struct rusage before, after;
	struct timespec start, end;
	uint64_t total = 0;
	unsigned long long swapin0, swapout0, swapin1, swapout1;

	setvbuf(stdout, NULL, _IONBF, 0);
	if (seconds < 1)
		return 1;
	mem = mmap(NULL, (size_t)PAGES * 4096, PROT_READ | PROT_WRITE,
		   MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
	if (mem == MAP_FAILED)
		return perror("mmap"), 1;
	for (unsigned long page = 0; page < PAGES; page++) {
		mem[page * STRIDE] = page;
		mem[page * STRIDE + 1] = page;
		mem[page * STRIDE + 2] = ~page;
	}
	for (long i = 0; i < THREADS; i++)
		if (mprotect((void *)(mem + i * (PAGES / THREADS) * STRIDE),
			     (HOT / THREADS) * 4096, PROT_READ))
			return perror("mprotect"), 1;
	printf("RECLAIM: initialized 128 MiB, 32 MiB read-only hot set, %d seconds\n", seconds);
	if (swap_counts(&swapin0, &swapout0))
		return 1;
	getrusage(RUSAGE_SELF, &before);
	clock_gettime(CLOCK_MONOTONIC, &start);
	for (long i = 0; i < THREADS; i++)
		if (pthread_create(&threads[i], NULL, worker, (void *)i))
			return 1;
	for (int i = 0; i < seconds && !atomic_load(&stop); i++)
		sleep(1);
	atomic_store(&stop, 1);
	for (int i = 0; i < THREADS; i++) {
		pthread_join(threads[i], NULL);
		total += operations[i];
	}
	clock_gettime(CLOCK_MONOTONIC, &end);
	getrusage(RUSAGE_SELF, &after);
	if (swap_counts(&swapin1, &swapout1))
		return 1;
	printf("RECLAIM: operations=%llu seconds=%.3f minor=%ld major=%ld swapin=%llu swapout=%llu\n",
	       (unsigned long long)total,
	       end.tv_sec - start.tv_sec + (end.tv_nsec - start.tv_nsec) / 1e9,
	       after.ru_minflt - before.ru_minflt, after.ru_majflt - before.ru_majflt,
	       swapin1 - swapin0, swapout1 - swapout0);
	printf("RECLAIM: %s\n", atomic_load(&bad) ? "FAIL (data corruption)" : "PASS");
	return atomic_load(&bad) != 0;
}
