/* SPDX-License-Identifier: GPL-2.0-only */
/* Console kicks and RTC hypercalls compete for the same VMM. Keep them on
 * different guest vCPUs so a blocked hypercall cannot stop the producer. */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <linux/rtc.h>
#include <sched.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/wait.h>
#include <unistd.h>

static pid_t writer = -1;

static void fail(const char *why)
{
	if (writer > 0)
		kill(writer, SIGKILL);
	fprintf(stderr, "CONSOLETEST: FAIL %s\n", why);
	exit(1);
}

static void timed_out(int sig)
{
	static const char msg[] = "CONSOLETEST: FAIL hypercall timeout\n";
	(void)sig;
	if (writer > 0)
		kill(writer, SIGKILL);
	(void)write(STDERR_FILENO, msg, sizeof(msg) - 1);
	_exit(1);
}

static void pin(int cpu)
{
	cpu_set_t mask;
	CPU_ZERO(&mask);
	CPU_SET(cpu, &mask);
	if (sched_setaffinity(0, sizeof(mask), &mask))
		fail("CPU affinity");
}

int main(void)
{
	int fd, status;
	pin(2);
	writer = fork();
	if (writer < 0)
		fail("fork");
	if (!writer) {
		char buf[1024];
		pin(1);
		fd = open("/dev/kmsg", O_WRONLY);
		if (fd < 0)
			fail("open /dev/kmsg");
		memset(buf, 'x', sizeof(buf));
		memcpy(buf, "<6>console flood ", 17);
		buf[sizeof(buf) - 1] = '\n';
		for (;;)
			if (write(fd, buf, sizeof(buf)) < 0 && errno != EAGAIN)
				fail("console write");
	}
	signal(SIGALRM, timed_out);
	alarm(30);
	sleep(1);
	fd = open("/dev/rtc0", O_RDONLY);
	if (fd < 0)
		fail("open RTC");
	for (int i = 0; i < 20; i++) {
		struct rtc_time tm;
		if (ioctl(fd, RTC_RD_TIME, &tm))
			fail("RTC hypercall");
	}
	if (waitpid(writer, &status, WNOHANG) != 0)
		fail("console producer exited early");
	kill(writer, SIGKILL);
	waitpid(writer, &status, 0);
	writer = -1;
	alarm(0);
	sleep(1);
	puts("CONSOLETEST: PASS 20 RTC hypercalls under continuous output");
	return 0;
}
