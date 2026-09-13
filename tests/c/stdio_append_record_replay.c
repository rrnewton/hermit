#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <sys/syscall.h>
#include <sys/uio.h>
#include <unistd.h>

#ifndef RWF_NOAPPEND
#define RWF_NOAPPEND 0x20
#endif

/* Proposed fixture only: compiled and run only inside the official test node. */
static int check(long result, long expected, int step) {
    if (result == expected) return 0;
    fprintf(stderr, "append-record-replay step=%d result=%ld errno=%d expected=%ld\n",
            step, result, errno, expected);
    return 70 + step;
}

int main(void) {
    int flags = fcntl(STDOUT_FILENO, F_GETFL);
    if (flags < 0 || fcntl(STDOUT_FILENO, F_SETFL, flags | O_APPEND) < 0) return 64;
    int logical = fcntl(STDOUT_FILENO, F_GETFL);
    if (logical < 0 || !(logical & O_APPEND)) return 65;
    struct iovec ordinary[] = {{.iov_base = "V", .iov_len = 1},
                              {.iov_base = "v", .iov_len = 1}};
    struct iovec positioned[] = {{.iov_base = "Q", .iov_len = 1},
                                {.iov_base = "q", .iov_len = 1}};
    struct iovec noappend = {.iov_base = "n", .iov_len = 1};
    int failed;
    if ((failed = check(syscall(SYS_write, STDOUT_FILENO, "W", 1), 1, 1))) return failed;
    if ((failed = check(syscall(SYS_writev, STDOUT_FILENO, ordinary, 2), 2, 2))) return failed;
    if ((failed = check(syscall(SYS_pwrite64, STDOUT_FILENO, "P", 1, 2), 1, 3))) return failed;
    if ((failed = check(syscall(SYS_pwritev, STDOUT_FILENO, positioned, 2, 1, 0), 2, 4))) return failed;
    if ((failed = check(syscall(SYS_pwritev2, STDOUT_FILENO, &noappend, 1,
                                0, 0, RWF_NOAPPEND), 1, 5))) return failed;
    if ((failed = check(syscall(SYS_write, STDOUT_FILENO, NULL, 0), 0, 6))) return failed;
    return 37;
}
