/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * guest_random_words: the guest of hermit-cli/tests/guest_random_adapter.rs.
 *
 * Hermit's guest-random adapter (plan step S4, diff B) is to answer each
 * 8-byte read of the path /dev/hermit/guest-random with one 64-bit
 * `guest.random` word: a seeded one, or one a choice controller picks. The
 * path exists on no filesystem, so without the adapter its open fails with
 * ENOENT, as it does everywhere outside Hermit. This program reads the path
 * directly, or through tools/libvoidstar's fuzz_get_random(), and prints what
 * it got, so that a test can compare words across runs, controllers and
 * processes. It needs no input file of its own.
 *
 * Every word is printed on a line of its own as
 *
 *     word TID VALUE
 *
 * with TID the reading thread's gettid() and VALUE the word as an exact
 * unsigned decimal. A failed call prints its errno's name. Modes (argv[1]):
 *
 *   words N          N reads of 8 bytes.
 *   absent           Only open the path: "open: OK" or "open: ERRNO".
 *   sizes            A read of 4 bytes, then a read of 16 bytes into a buffer
 *                    whose last 8 bytes show whether they were written.
 *   rules            pread, write, the write-mode opens, a nonblocking open
 *                    and poll, each printing its result, then one word.
 *   fork K M         K words; then a forked child reads M words through the
 *                    inherited descriptor and exits; then the parent reads M.
 *   exec K M         K words; then this program execs itself as "words M".
 *   getrandom        getrandom of 16 bytes, one word (or the open's errno),
 *                    then getrandom of 16 bytes again, printed in hex.
 *   dlopen LIB N     N calls of fuzz_get_random() from the library LIB.
 *   reuse LIB FILE   The descriptor-reuse gap: one fuzz_get_random() call,
 *                    close_range(3, ~0U, 0), an open of the regular file FILE
 *                    that takes back the lowest descriptor number, then a
 *                    second call. FILE's first 8 bytes, read through the
 *                    program's own descriptor, are printed before the second
 *                    call, and that descriptor's offset after it.
 *
 * Exit status: 0 when every call the mode makes ran, whatever it returned; 1
 * when the path cannot be opened in a mode that reads it; 2 for a usage error;
 * 3 when a call the mode depends on returned something else (named on
 * stdout).
 */

#define _GNU_SOURCE
#include <dlfcn.h>
#include <errno.h>
#include <fcntl.h>
#include <inttypes.h>
#include <poll.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/random.h>
#include <sys/syscall.h>
#include <sys/types.h>
#include <sys/wait.h>
#include <unistd.h>

#ifndef SYS_close_range
#define SYS_close_range 436
#endif

/* The path the adapter answers; one 8-byte read of it is one word. */
#define GUEST_RANDOM_PATH "/dev/hermit/guest-random"

/* fuzz_get_random's type. */
typedef uint64_t (*random_function)(void);

static const char *errno_name(int error) {
    switch (error) {
    case EACCES:
        return "EACCES";
    case EAGAIN:
        return "EAGAIN";
    case EBADF:
        return "EBADF";
    case EFAULT:
        return "EFAULT";
    case EINVAL:
        return "EINVAL";
    case EIO:
        return "EIO";
    case EISDIR:
        return "EISDIR";
    case ENODEV:
        return "ENODEV";
    case ENOENT:
        return "ENOENT";
    case ENOSYS:
        return "ENOSYS";
    case ENOTDIR:
        return "ENOTDIR";
    case ENXIO:
        return "ENXIO";
    case EPERM:
        return "EPERM";
    case EROFS:
        return "EROFS";
    case ESPIPE:
        return "ESPIPE";
    default:
        break;
    }
    static char other[32];
    snprintf(other, sizeof other, "errno %d", error);
    return other;
}

/* Print a call's result: its count, or its errno's name. */
static void print_result(const char *what, ssize_t result) {
    if (result < 0) {
        printf("%s: %s\n", what, errno_name(errno));
    } else {
        printf("%s: %zd\n", what, result);
    }
}

static void print_hex(const char *label, const unsigned char *bytes,
                      size_t length) {
    printf("%s ", label);
    for (size_t i = 0; i < length; i++) {
        printf("%02x", bytes[i]);
    }
    printf("\n");
}

static void print_word(uint64_t word) {
    long tid = syscall(SYS_gettid);
    printf("word %ld %" PRIu64 "\n", tid, word);
}

/* Open the path for reading, or say why not. */
static int open_path(void) {
    int fd = open(GUEST_RANDOM_PATH, O_RDONLY | O_CLOEXEC);
    if (fd < 0) {
        printf("open: %s\n", errno_name(errno));
    }
    return fd;
}

/* Read and print one word; 0, having said why, if the read did not give 8. */
static int read_word(int fd) {
    uint64_t word;
    ssize_t got = read(fd, &word, sizeof word);
    if (got != (ssize_t)sizeof word) {
        print_result("read 8", got);
        return 0;
    }
    print_word(word);
    return 1;
}

static int read_words(int fd, long count) {
    for (long i = 0; i < count; i++) {
        if (!read_word(fd)) {
            return 0;
        }
    }
    return 1;
}

static long count_arg(const char *text) {
    char *end;
    errno = 0;
    long value = strtol(text, &end, 10);
    if (errno != 0 || end == text || *end != '\0' || value < 0) {
        fprintf(stderr, "guest_random_words: not a count: %s\n", text);
        exit(2);
    }
    return value;
}

static int words_mode(long count) {
    int fd = open_path();
    if (fd < 0) {
        return 1;
    }
    return read_words(fd, count) ? 0 : 3;
}

static int absent_mode(void) {
    int fd = open(GUEST_RANDOM_PATH, O_RDONLY | O_CLOEXEC);
    if (fd < 0) {
        printf("open: %s\n", errno_name(errno));
        return 0;
    }
    printf("open: OK\n");
    close(fd);
    return 0;
}

static int sizes_mode(void) {
    int fd = open_path();
    if (fd < 0) {
        return 1;
    }
    unsigned char buffer[16];
    memset(buffer, 0xa5, sizeof buffer);
    print_result("read 4", read(fd, buffer, 4));
    memset(buffer, 0xa5, sizeof buffer);
    ssize_t got = read(fd, buffer, sizeof buffer);
    print_result("read 16", got);
    if (got >= 8) {
        uint64_t word;
        memcpy(&word, buffer, sizeof word);
        print_word(word);
        int untouched = 1;
        for (size_t i = 8; i < sizeof buffer; i++) {
            untouched &= buffer[i] == 0xa5;
        }
        printf("tail: %s\n", untouched ? "untouched" : "written");
    }
    return 0;
}

static void print_open(const char *what, int flags) {
    int fd = open(GUEST_RANDOM_PATH, flags | O_CLOEXEC);
    if (fd < 0) {
        printf("open %s: %s\n", what, errno_name(errno));
        return;
    }
    printf("open %s: OK\n", what);
    close(fd);
}

/* poll for POLLIN without waiting; print the count and the events set. */
static void print_poll(int fd) {
    static const struct {
        short bit;
        const char *name;
    } names[] = {
        {POLLIN, "POLLIN"},   {POLLPRI, "POLLPRI"}, {POLLOUT, "POLLOUT"},
        {POLLERR, "POLLERR"}, {POLLHUP, "POLLHUP"}, {POLLNVAL, "POLLNVAL"},
    };
    struct pollfd entry = {.fd = fd, .events = POLLIN, .revents = 0};
    int ready = poll(&entry, 1, 0);
    if (ready < 0) {
        printf("poll: %s\n", errno_name(errno));
        return;
    }
    printf("poll: %d", ready);
    const char *separator = " ";
    int rest = entry.revents;
    for (size_t i = 0; i < sizeof names / sizeof names[0]; i++) {
        if (entry.revents & names[i].bit) {
            printf("%s%s", separator, names[i].name);
            separator = "|";
            rest &= ~names[i].bit;
        }
    }
    if (rest != 0) {
        printf("%s0x%x", separator, (unsigned)rest);
    }
    if (entry.revents == 0) {
        printf(" none");
    }
    printf("\n");
}

static int rules_mode(void) {
    int fd = open_path();
    if (fd < 0) {
        return 1;
    }
    unsigned char buffer[8] = {0};
    print_result("pread", pread(fd, buffer, sizeof buffer, 0));
    print_result("write", write(fd, buffer, sizeof buffer));
    print_open("O_WRONLY", O_WRONLY);
    print_open("O_RDWR", O_RDWR);
    print_open("O_RDONLY|O_NONBLOCK", O_RDONLY | O_NONBLOCK);
    print_poll(fd);
    return read_word(fd) ? 0 : 3;
}

static int fork_mode(long before, long each) {
    int fd = open_path();
    if (fd < 0) {
        return 1;
    }
    if (!read_words(fd, before)) {
        return 3;
    }
    fflush(stdout);
    pid_t child = fork();
    if (child < 0) {
        printf("fork: %s\n", errno_name(errno));
        return 3;
    }
    if (child == 0) {
        int ok = read_words(fd, each);
        fflush(stdout);
        _exit(ok ? 0 : 3);
    }
    int status;
    if (waitpid(child, &status, 0) != child) {
        printf("waitpid: %s\n", errno_name(errno));
        return 3;
    }
    if (!WIFEXITED(status) || WEXITSTATUS(status) != 0) {
        printf("child: status 0x%x\n", (unsigned)status);
        return 3;
    }
    return read_words(fd, each) ? 0 : 3;
}

static int exec_mode(const char *self, long before, const char *after) {
    int fd = open_path();
    if (fd < 0) {
        return 1;
    }
    if (!read_words(fd, before)) {
        return 3;
    }
    fflush(stdout);
    execl("/proc/self/exe", self, "words", after, (char *)NULL);
    printf("exec: %s\n", errno_name(errno));
    return 3;
}

static int print_getrandom(void) {
    unsigned char bytes[16];
    ssize_t got = getrandom(bytes, sizeof bytes, 0);
    if (got != (ssize_t)sizeof bytes) {
        print_result("getrandom", got);
        return 0;
    }
    print_hex("getrandom", bytes, sizeof bytes);
    return 1;
}

static int getrandom_mode(void) {
    if (!print_getrandom()) {
        return 3;
    }
    int fd = open_path();
    if (fd >= 0 && !read_word(fd)) {
        return 3;
    }
    return print_getrandom() ? 0 : 3;
}

/* LIB's fuzz_get_random, loaded with dlopen as the SDKs load it. */
static random_function load(const char *library) {
    void *handle = dlopen(library, RTLD_NOW | RTLD_LOCAL);
    if (handle == NULL) {
        printf("dlopen: %s\n", dlerror());
        return NULL;
    }
    void *symbol = dlsym(handle, "fuzz_get_random");
    if (symbol == NULL) {
        printf("dlsym: %s\n", dlerror());
        return NULL;
    }
    return (random_function)symbol;
}

static int dlopen_mode(const char *library, long count) {
    random_function get = load(library);
    if (get == NULL) {
        return 3;
    }
    for (long i = 0; i < count; i++) {
        print_word(get());
    }
    return 0;
}

static int close_from_three(void) {
    if (syscall(SYS_close_range, 3U, ~0U, 0U) != 0) {
        printf("close_range: %s\n", errno_name(errno));
        return 0;
    }
    return 1;
}

static int reuse_mode(const char *library, const char *file) {
    /* Start with descriptors 0, 1 and 2 only. */
    if (!close_from_three()) {
        return 3;
    }
    random_function get = load(library);
    if (get == NULL) {
        return 3;
    }
    /* The lowest free number: the one the library's open is given. */
    int lowest = fcntl(STDOUT_FILENO, F_DUPFD_CLOEXEC, 0);
    if (lowest < 0) {
        printf("fcntl: %s\n", errno_name(errno));
        return 3;
    }
    close(lowest);
    print_word(get());
    if (!close_from_three()) {
        return 3;
    }
    int fd = open(file, O_RDONLY | O_CLOEXEC);
    if (fd < 0) {
        printf("open %s: %s\n", file, errno_name(errno));
        return 3;
    }
    if (fd != lowest) {
        printf("unexpected: the file took descriptor %d, not %d\n", fd,
               lowest);
        return 3;
    }
    printf("file descriptor reused\n");
    unsigned char bytes[8];
    ssize_t got = pread(fd, bytes, sizeof bytes, 0);
    if (got != (ssize_t)sizeof bytes) {
        print_result("file pread", got);
        return 3;
    }
    print_hex("file bytes", bytes, sizeof bytes);
    print_word(get());
    off_t offset = lseek(fd, 0, SEEK_CUR);
    if (offset < 0) {
        printf("lseek: %s\n", errno_name(errno));
        return 3;
    }
    printf("file offset %lld\n", (long long)offset);
    return 0;
}

static int usage(void) {
    fprintf(stderr,
            "usage: guest_random_words words N | absent | sizes | rules | "
            "fork K M | exec K M | getrandom | dlopen LIB N | reuse LIB FILE\n");
    return 2;
}

int main(int argc, char **argv) {
    /* Line-buffered, so nothing is pending at a fork or an exec. */
    setvbuf(stdout, NULL, _IOLBF, 0);
    if (argc < 2) {
        return usage();
    }
    const char *mode = argv[1];
    if (strcmp(mode, "words") == 0 && argc == 3) {
        return words_mode(count_arg(argv[2]));
    }
    if (strcmp(mode, "absent") == 0 && argc == 2) {
        return absent_mode();
    }
    if (strcmp(mode, "sizes") == 0 && argc == 2) {
        return sizes_mode();
    }
    if (strcmp(mode, "rules") == 0 && argc == 2) {
        return rules_mode();
    }
    if (strcmp(mode, "fork") == 0 && argc == 4) {
        return fork_mode(count_arg(argv[2]), count_arg(argv[3]));
    }
    if (strcmp(mode, "exec") == 0 && argc == 4) {
        count_arg(argv[3]);
        return exec_mode(argv[0], count_arg(argv[2]), argv[3]);
    }
    if (strcmp(mode, "getrandom") == 0 && argc == 2) {
        return getrandom_mode();
    }
    if (strcmp(mode, "dlopen") == 0 && argc == 4) {
        return dlopen_mode(argv[2], count_arg(argv[3]));
    }
    if (strcmp(mode, "reuse") == 0 && argc == 4) {
        return reuse_mode(argv[2], argv[3]);
    }
    return usage();
}
