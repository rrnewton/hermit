#define _GNU_SOURCE

#include <errno.h>
#include <inttypes.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/syscall.h>
#include <time.h>
#include <unistd.h>

enum { ITERATIONS = 100000 };

int main(int argc, char **argv) {
    uint64_t iterations = ITERATIONS;
    if (argc == 2) {
        char *end = NULL;
        errno = 0;
        uintmax_t parsed = strtoumax(argv[1], &end, 10);
        if (errno != 0 || end == argv[1] || *end != '\0') {
            fprintf(stderr, "invalid iteration count: %s\n", argv[1]);
            return 2;
        }
        iterations = parsed;
    } else if (argc != 1) {
        fprintf(stderr, "usage: %s [iterations]\n", argv[0]);
        return 2;
    }

    uint64_t completed = 0;

    for (uint64_t iteration = 0; iteration < iterations; ++iteration) {
        long result;
        if ((iteration & 1U) == 0) {
            result = syscall(SYS_getpid);
        } else {
            struct timespec now;
            result = syscall(SYS_clock_gettime, CLOCK_MONOTONIC, &now);
        }
        if (result < 0) {
            perror("syscall");
            return errno == 0 ? 1 : errno;
        }
        ++completed;
    }

    printf("%" PRIu64 "\n", completed);
    return 0;
}
