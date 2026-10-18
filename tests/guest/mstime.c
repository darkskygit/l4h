/* Print CLOCK_MONOTONIC in milliseconds, for timing shell steps. */
#include <stdio.h>
#include <time.h>

int main(void)
{
	struct timespec ts;

	clock_gettime(CLOCK_MONOTONIC, &ts);
	printf("%lld\n", (long long)ts.tv_sec * 1000 + ts.tv_nsec / 1000000);
	return 0;
}
