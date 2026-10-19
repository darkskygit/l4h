/*
 * Bulk TCP for tests/run-bench.sh, so that the network is measured and not
 * the tool (busybox nc copies in small chunks and saturates a vCPU at
 * ~8 MB/s):
 *   netbench recv PORT          accept connections, discard in 1 MiB reads
 *   netbench send HOST PORT MIB write MIB MiB of zeros in 256 KiB writes
 */
#include <arpa/inet.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <unistd.h>

static char buf[1 << 20];

int main(int argc, char **argv)
{
	struct sockaddr_in a = { .sin_family = AF_INET };
	int s = socket(AF_INET, SOCK_STREAM, 0), one = 1;

	if (argc == 3 && !strcmp(argv[1], "recv")) {
		a.sin_port = htons(atoi(argv[2]));
		setsockopt(s, SOL_SOCKET, SO_REUSEADDR, &one, sizeof(one));
		if (bind(s, (struct sockaddr *)&a, sizeof(a)) || listen(s, 4)) {
			perror("netbench: listen");
			return 1;
		}
		for (;;) {
			int c = accept(s, NULL, NULL);

			while (read(c, buf, sizeof(buf)) > 0)
				;
			close(c);
		}
	}
	if (argc == 5 && !strcmp(argv[1], "send")) {
		long left = atol(argv[4]) << 20;

		a.sin_port = htons(atoi(argv[3]));
		inet_pton(AF_INET, argv[2], &a.sin_addr);
		if (connect(s, (struct sockaddr *)&a, sizeof(a))) {
			perror("netbench: connect");
			return 1;
		}
		while (left > 0) {
			ssize_t n = write(s, buf, left < 256 << 10 ? left : 256 << 10);

			if (n <= 0) {
				perror("netbench: write");
				return 1;
			}
			left -= n;
		}
		/* Done when the receiver has read everything. */
		shutdown(s, SHUT_WR);
		while (read(s, buf, sizeof(buf)) > 0)
			;
		return 0;
	}
	fprintf(stderr, "usage: netbench recv PORT | netbench send HOST PORT MIB\n");
	return 2;
}
