#define _GNU_SOURCE
#include <errno.h>
#include <linux/futex.h>
#include <stdatomic.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/mman.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

struct shared_state {
	_Atomic uint32_t turn;
	_Atomic uint32_t ready;
};

static int futex_wait(_Atomic uint32_t *word, uint32_t expected)
{
	return syscall(SYS_futex, word, FUTEX_WAIT, expected, NULL, NULL, 0);
}

static int futex_wake(_Atomic uint32_t *word)
{
	return syscall(SYS_futex, word, FUTEX_WAKE, 1, NULL, NULL, 0);
}

static double now(void)
{
	struct timespec ts;
	clock_gettime(CLOCK_MONOTONIC, &ts);
	return (double)ts.tv_sec + (double)ts.tv_nsec / 1e9;
}

static int wait_for(_Atomic uint32_t *word, uint32_t value)
{
	while (atomic_load_explicit(word, memory_order_acquire) != value) {
		if (futex_wait(word, value ^ 1) < 0 && errno != EAGAIN && errno != EINTR)
			return -1;
	}
	return 0;
}

static int run_pingpong(int rounds)
{
	struct shared_state *s = mmap(NULL, sizeof(*s), PROT_READ | PROT_WRITE,
				      MAP_SHARED | MAP_ANONYMOUS, -1, 0);
	pid_t child;
	double start, elapsed;
	int i, status;

	if (s == MAP_FAILED)
		return -1;
	atomic_init(&s->turn, 0);
	atomic_init(&s->ready, 0);
	child = fork();
	if (child < 0)
		return -1;
	if (child == 0) {
		atomic_store_explicit(&s->ready, 1, memory_order_release);
		futex_wake(&s->ready);
		for (i = 0; i < rounds; i++) {
			if (wait_for(&s->turn, 1) < 0)
				_exit(2);
			atomic_store_explicit(&s->turn, 0, memory_order_release);
			futex_wake(&s->turn);
		}
		_exit(0);
	}
	if (wait_for(&s->ready, 1) < 0)
		return -1;
	start = now();
	for (i = 0; i < rounds; i++) {
		atomic_store_explicit(&s->turn, 1, memory_order_release);
		futex_wake(&s->turn);
		if (wait_for(&s->turn, 0) < 0)
			return -1;
	}
	elapsed = now() - start;
	if (waitpid(child, &status, 0) != child || !WIFEXITED(status) || WEXITSTATUS(status))
		return -1;
	printf("FUTEXBENCH: process-shared %.1f us/roundtrip (%d rounds)\n",
	       elapsed * 1e6 / rounds, rounds);
	munmap(s, sizeof(*s));
	return 0;
}

int main(int argc, char **argv)
{
	int rounds = argc > 1 ? atoi(argv[1]) : 1000;
	if (rounds < 1)
		return 2;
	if (run_pingpong(rounds) < 0) {
		perror("process-shared futex ping-pong");
		return 1;
	}
	puts("FUTEXBENCH: done");
	return 0;
}
