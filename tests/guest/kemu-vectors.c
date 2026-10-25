// SPDX-License-Identifier: GPL-2.0
/* Exercise live JIT vectors in separate seL4 user TCBs and signal frames. */
#define _GNU_SOURCE
#include <assert.h>
#include <pthread.h>
#include <signal.h>
#include <stdatomic.h>
#include <stdint.h>
#include <stdio.h>
#include <time.h>
#include <ucontext.h>
#include <asm/sigcontext.h>

#define THREADS 4
#define ITERATIONS 200000

static atomic_uint done;
static _Thread_local volatile sig_atomic_t loop_signals, bad_frame;
static _Thread_local uint64_t increment;

extern const char vector_loop_begin[], vector_loop_end[];
extern void vector_loop(uint64_t v[8][2], unsigned count);
__asm__(
	".text\n.globl vector_loop\n.type vector_loop,%function\nvector_loop:\n"
	"ldp q0,q1,[x0]\nldp q2,q3,[x0,#32]\n"
	"ldp q4,q5,[x0,#64]\nldp q6,q7,[x0,#96]\n"
	".globl vector_loop_begin\nvector_loop_begin:\n"
	".irp r,0,1,2,3,4,5,6\n"
	"add v\\r\\().2d,v\\r\\().2d,v7.2d\n.endr\n"
	"subs w1,w1,#1\nb.ne vector_loop_begin\n"
	".globl vector_loop_end\nvector_loop_end:\n"
	"stp q0,q1,[x0]\nstp q2,q3,[x0,#32]\n"
	"stp q4,q5,[x0,#64]\nstp q6,q7,[x0,#96]\nret\n"
	".size vector_loop,.-vector_loop\n"
);

static void handler(int sig, siginfo_t *si, void *ptr)
{
	ucontext_t *uc = ptr;

	(void)sig;
	(void)si;
	if (uc->uc_mcontext.pc >= (uintptr_t)vector_loop_begin &&
	    uc->uc_mcontext.pc < (uintptr_t)vector_loop_end) {
		struct fpsimd_context *fp = (void *)uc->uc_mcontext.__reserved;

		loop_signals++;
		if (fp->head.magic != FPSIMD_MAGIC ||
		    (uint64_t)fp->vregs[7] != increment ||
		    (uint64_t)(fp->vregs[7] >> 64) != increment)
			bad_frame = 1;
		for (unsigned r = 0; r < 7; r++)
			if ((uint64_t)(fp->vregs[r] >> 64) != (uint64_t)fp->vregs[r] + 1)
				bad_frame = 1;
	}
	/* sigreturn must discard the handler's vector state. */
	__asm__ volatile(
		".irp r,0,1,2,3,4,5,6,7\n"
		"movi v\\r\\().16b,#0xa5\n.endr\n"
		::: "v0", "v1", "v2", "v3", "v4", "v5", "v6", "v7");
}

static void *worker(void *arg)
{
	uintptr_t id = (uintptr_t)arg;
	uint64_t v[8][2];
	unsigned rounds = 0;

	increment = id + 1;
	for (unsigned r = 0; r < 7; r++) {
		v[r][0] = 1000 * id + 2 * r;
		v[r][1] = v[r][0] + 1;
	}
	v[7][0] = v[7][1] = increment;
	/* Retry short loops if signals only arrived outside the live region. */
	do {
		/* Tagged SIMD loads/stores also exercise kemu's #GP recovery. */
		vector_loop((void *)((uintptr_t)v | ((0xa0ul + id) << 56)), ITERATIONS);
		rounds++;
	} while (!loop_signals && rounds < 32);
	assert(loop_signals && !bad_frame);
	for (unsigned r = 0; r < 7; r++) {
		uint64_t expected = 1000 * id + 2 * r + increment * ITERATIONS * rounds;

		assert(v[r][0] == expected && v[r][1] == expected + 1);
	}
	assert(v[7][0] == increment && v[7][1] == increment);
	printf("PASS kemu-vectors thread %lu (%d live signals)\n",
	       (unsigned long)id, loop_signals);
	atomic_fetch_or(&done, 1u << id);
	return NULL;
}

int main(void)
{
	pthread_t threads[THREADS];
	struct sigaction sa = { .sa_sigaction = handler, .sa_flags = SA_SIGINFO };
	const struct timespec pause = { .tv_nsec = 100000 };

	sigemptyset(&sa.sa_mask);
	assert(!sigaction(SIGUSR1, &sa, NULL));
	for (uintptr_t i = 0; i < THREADS; i++)
		assert(!pthread_create(&threads[i], NULL, worker, (void *)i));
	while (atomic_load(&done) != (1u << THREADS) - 1) {
		for (unsigned i = 0; i < THREADS; i++)
			if (!(atomic_load(&done) & (1u << i)))
				assert(!pthread_kill(threads[i], SIGUSR1));
		nanosleep(&pause, NULL);
	}
	for (unsigned i = 0; i < THREADS; i++)
		assert(!pthread_join(threads[i], NULL));
	puts("PASS kemu-vectors");
	return 0;
}
