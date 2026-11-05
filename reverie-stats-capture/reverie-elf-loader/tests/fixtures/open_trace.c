/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/* Test-only x86-64 Linux libc-symbol instrumentation. This does not observe
 * kernel-internal exec opens, glibc-private opens, or direct caller syscalls.
 * Fixture pathnames are stable valid strings, bounded by Linux PATH_MAX.
 * Each record uses a transient raw-syscall FD, closed before the hook returns.
 */
#define _GNU_SOURCE
#define _LARGEFILE64_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdarg.h>
#include <stdatomic.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <unistd.h>

#define MAX_TRACE_RECORDS 4096
static _Atomic unsigned long long attempted;
static _Atomic unsigned long long logged;
static _Atomic unsigned long long failures;

/* The caller saves/restores errno around this raw logging operation. */
static int write_record(const char *destination, const char *line, size_t length)
{
    long trace_fd = syscall(SYS_openat, AT_FDCWD, destination,
                            O_WRONLY | O_APPEND | O_CREAT | O_CLOEXEC, 0600);
    if (trace_fd < 0)
        return 0;
    size_t offset = 0;
    while (offset < length) {
        long count = syscall(SYS_write, trace_fd, line + offset, length - offset);
        if (count < 0 && errno == EINTR)
            continue;
        if (count <= 0)
            break;
        offset += (size_t)count;
    }
    long closed = syscall(SYS_close, trace_fd);
    return offset == length && closed == 0;
}

static void record(const char *operation, int dirfd, unsigned int flags,
                   long result, int operation_errno, const char *path)
{
    int saved_errno = errno;
    const char *destination = getenv("REVERIE_LB_OPEN_TRACE");
    if (!destination || !*destination) {
        errno = saved_errno;
        return;
    }
    if (atomic_fetch_add_explicit(&attempted, 1, memory_order_relaxed) >= MAX_TRACE_RECORDS) {
        atomic_fetch_add_explicit(&failures, 1, memory_order_relaxed);
        errno = saved_errno;
        return;
    }
    /* Never inspect a caller pointer after its syscall reported EFAULT. */
    if (result == -1 && operation_errno == EFAULT)
        path = "<fault>";
    if (!path)
        path = "<null>";
    char line[8192];
    int length = snprintf(line, sizeof(line), "%s\t%d\t%u\t%ld\t%d\t%.4096s\n",
                          operation, dirfd, flags, result, operation_errno, path);
    if (length < 0 || (size_t)length >= sizeof(line)) {
        atomic_fetch_add_explicit(&failures, 1, memory_order_relaxed);
        errno = saved_errno;
        return;
    }
    if (write_record(destination, line, (size_t)length))
        atomic_fetch_add_explicit(&logged, 1, memory_order_relaxed);
    else
        atomic_fetch_add_explicit(&failures, 1, memory_order_relaxed);
    errno = saved_errno;
}

__attribute__((constructor)) static void initialized(void)
{
    record("initialized", AT_FDCWD, 0, 0, 0, "<init>");
}

/* Required terminal evidence for normal, bounded test-process termination.
 * Missing, partial, nonterminal or failed footers cannot prove completeness.
 */
__attribute__((destructor)) static void completed(void)
{
    int saved_errno = errno;
    const char *destination = getenv("REVERIE_LB_OPEN_TRACE");
    if (destination && *destination) {
        unsigned long long total = atomic_load_explicit(&attempted, memory_order_relaxed);
        unsigned long long written = atomic_load_explicit(&logged, memory_order_relaxed);
        unsigned long long lost = atomic_load_explicit(&failures, memory_order_relaxed);
        int complete = total == written && lost == 0 && total < MAX_TRACE_RECORDS;
        char footer[256];
        int length = snprintf(footer, sizeof(footer), "footer\t%llu\t%llu\t%llu\t%d\n",
                              total, written, lost, complete);
        if (length > 0 && (size_t)length < sizeof(footer))
            (void)write_record(destination, footer, (size_t)length);
    }
    errno = saved_errno;
}

static int open_call(const char *operation, int dirfd, const char *path,
                     int flags, mode_t mode)
{
    long result = syscall(SYS_openat, dirfd, path, flags, mode);
    int saved_errno = errno;
    record(operation, dirfd, (unsigned int)flags, result,
           result == -1 ? saved_errno : 0, path);
    errno = saved_errno;
    return (int)result;
}

static int needs_mode(int flags)
{
    return (flags & O_CREAT) || ((flags & O_TMPFILE) == O_TMPFILE);
}

int open(const char *path, int flags, ...)
{
    mode_t mode = 0;
    if (needs_mode(flags)) {
        va_list arguments;
        va_start(arguments, flags);
        mode = va_arg(arguments, mode_t);
        va_end(arguments);
    }
    return open_call("open", AT_FDCWD, path, flags, mode);
}

int open64(const char *path, int flags, ...)
{
    mode_t mode = 0;
    if (needs_mode(flags)) {
        va_list arguments;
        va_start(arguments, flags);
        mode = va_arg(arguments, mode_t);
        va_end(arguments);
    }
    return open_call("open64", AT_FDCWD, path, flags, mode);
}

int openat(int dirfd, const char *path, int flags, ...)
{
    mode_t mode = 0;
    if (needs_mode(flags)) {
        va_list arguments;
        va_start(arguments, flags);
        mode = va_arg(arguments, mode_t);
        va_end(arguments);
    }
    return open_call("openat", dirfd, path, flags, mode);
}

int openat64(int dirfd, const char *path, int flags, ...)
{
    mode_t mode = 0;
    if (needs_mode(flags)) {
        va_list arguments;
        va_start(arguments, flags);
        mode = va_arg(arguments, mode_t);
        va_end(arguments);
    }
    return open_call("openat64", dirfd, path, flags, mode);
}

int statx(int dirfd, const char *path, int flags, unsigned int mask,
          struct statx *buffer)
{
    long result = syscall(SYS_statx, dirfd, path, flags, mask, buffer);
    int saved_errno = errno;
    record("statx", dirfd, (unsigned int)flags, result,
           result == -1 ? saved_errno : 0, path);
    errno = saved_errno;
    return (int)result;
}
