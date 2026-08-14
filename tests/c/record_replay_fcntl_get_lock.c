/*
 * Exercise record/replay of the output buffers and error paths shared by
 * F_GETLK and F_OFD_GETLK. Success must restore the kernel-written l_type;
 * EBADF and EFAULT must not depend on or overwrite the caller's buffer.
 */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

static int query_is_unlocked(int fd, int command) {
    struct flock query;
    memset(&query, 0, sizeof(query));
    query.l_type = F_WRLCK;
    query.l_whence = SEEK_SET;
    query.l_start = 16;
    query.l_len = 8;
    return fcntl(fd, command, &query) == 0 && query.l_type == F_UNLCK;
}

static int null_fails_with_efault(int fd, int command) {
    errno = 0;
    return fcntl(fd, command, NULL) == -1 && errno == EFAULT;
}

static int bad_fd_preserves_buffer(int command) {
    struct flock query;
    struct flock before;
    memset(&query, 0x5a, sizeof(query));
    memcpy(&before, &query, sizeof(query));
    errno = 0;
    return fcntl(-1, command, &query) == -1 && errno == EBADF &&
           memcmp(&query, &before, sizeof(query)) == 0;
}

int main(void) {
    char path[] = "/tmp/hermit-fcntl-get-lock-XXXXXX";
    int fd = mkstemp(path);
    if (fd == -1) {
        return EXIT_FAILURE;
    }
    unlink(path);

    int ok = 0;
    ok += query_is_unlocked(fd, F_GETLK);
    ok += query_is_unlocked(fd, F_OFD_GETLK);
    ok += null_fails_with_efault(fd, F_GETLK);
    ok += null_fails_with_efault(fd, F_OFD_GETLK);
    ok += bad_fd_preserves_buffer(F_GETLK);
    ok += bad_fd_preserves_buffer(F_OFD_GETLK);
    close(fd);

    printf("fcntl-get-lock ok=%d\n", ok);
    return ok == 6 ? EXIT_SUCCESS : EXIT_FAILURE;
}
