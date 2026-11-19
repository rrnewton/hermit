/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * libvoidstar.so for programs run under Hermit.
 *
 * Programs written against the Antithesis SDKs take their randomness from one
 * C function, fuzz_get_random(), in a library named libvoidstar.so that the
 * SDKs load from /usr/lib/libvoidstar.so. This file is Hermit's guest-side
 * adapter with that name. Each fuzz_get_random() call is meant to be one
 * `guest.random` choice point: it reads one 64-bit word from the path
 * /dev/hermit/guest-random, which Hermit is to intercept and answer, so that
 * an outside search tool can pick the word and an ordinary run gets a seeded,
 * repeatable one. The path exists on no filesystem: outside Hermit the open
 * fails, and fuzz_get_random() names the path on stderr and aborts. It never
 * falls back to any other source of randomness.
 *
 * Hermit does not intercept the path yet; that lands separately. Until it
 * does, fuzz_get_random() aborts by name everywhere.
 *
 * The exports are exactly the 14 of the public Antithesis stub
 * (github.com/antithesishq/antithesis_stub, libvoidstar.c at commit
 * 00659dd02d9dfec3ade2f009239ec2addf344249), with the same signatures; they
 * are declared together below. Nothing else is visible.
 *
 *   fuzz_get_random                one word, read from
 *                                  /dev/hermit/guest-random
 *   fuzz_json_data,                append the record to
 *   fuzz_json_data_unfilterable    $ANTITHESIS_OUTPUT_DIR/sdk.jsonl
 *   fuzz_flush                     returns; it is not a receipt
 *   the four __sanitizer_cov_* hooks, init_coverage_module,
 *   notify_coverage, notify_coverage_v2, coverage_lease_generation_addr
 *                                  coverage: no-ops that record nothing
 *                                  and return the stub's inert values
 *   instrumentation_max_abi_version      returns 1
 *   instrumentation_request_abi_version  grants 1
 *
 * ABI version 1. The stub reports 3, the interface generation whose exports
 * and return values it implements. This library reports 1 because it does
 * not implement the coverage contract: its coverage entry points record
 * nothing. What versions 1 and 2 promise to an instrumented binary is not
 * documented by the stub, so granting 1 to a binary that asks for more is
 * unverified. The version rises when coverage capture lands.
 *
 * Build, with any C compiler:
 *
 *     cc -O2 -shared -fPIC -o libvoidstar.so libvoidstar.c
 *
 * Deliver it with Hermit's read-only bind. The SDKs need the fixed path
 * /usr/lib/libvoidstar.so, and in host mode a bind target must already
 * exist, so run under --image, where the target is inside the image:
 *
 *     hermit run --image=REF \
 *         --bind=HOST/libvoidstar.so:/usr/lib/libvoidstar.so:ro -- PROGRAM
 *
 * A harness of your own that loads the library by path can instead bind it
 * under the guest's /tmp, which host mode allows.
 */

#ifndef _POSIX_C_SOURCE
#define _POSIX_C_SOURCE 200809L
#endif

#include <errno.h>
#include <fcntl.h>
#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/types.h>
#include <unistd.h>

/* Everything after the system headers is hidden unless marked an export. */
#pragma GCC visibility push(hidden)

#define VOIDSTAR_EXPORT __attribute__((visibility("default")))

/* The one ABI version this library implements; see the header comment. */
#define HERMIT_VOIDSTAR_ABI_VERSION 1

/* The path Hermit is to answer; one 8-byte read of it is one word. */
#define GUEST_RANDOM_PATH "/dev/hermit/guest-random"

/* The variable naming the directory that receives SDK records. */
#define OUTPUT_DIR_VARIABLE "ANTITHESIS_OUTPUT_DIR"

/* The file in that directory the records are appended to. */
#define OUTPUT_FILE_NAME "sdk.jsonl"

#ifdef __cplusplus
extern "C" {
#endif

/* The 14 exports, with the stub's signatures. */
VOIDSTAR_EXPORT uint64_t fuzz_get_random(void);
VOIDSTAR_EXPORT void fuzz_json_data(const char *message, size_t length);
VOIDSTAR_EXPORT void fuzz_json_data_unfilterable(const char *message,
                                                 size_t length);
VOIDSTAR_EXPORT void fuzz_flush(void);
VOIDSTAR_EXPORT void __sanitizer_cov_trace_pc_guard_init(uint32_t *start,
                                                         uint32_t *stop);
VOIDSTAR_EXPORT void __sanitizer_cov_trace_pc_guard_internal(uint32_t *guard,
                                                             uint64_t edge);
VOIDSTAR_EXPORT void __sanitizer_cov_trace_pc_guard(uint32_t *guard);
VOIDSTAR_EXPORT void __sanitizer_cov_pcs_init(const uintptr_t *pcs_beg,
                                              const uintptr_t *pcs_end);
VOIDSTAR_EXPORT size_t init_coverage_module(size_t edge_count,
                                            const char *symbol_file_name);
VOIDSTAR_EXPORT bool notify_coverage(size_t edge_plus_module_id);
VOIDSTAR_EXPORT uint64_t notify_coverage_v2(size_t edge_plus_module_id,
                                            uint64_t hit_since_last_call);
VOIDSTAR_EXPORT const uint64_t *coverage_lease_generation_addr(void);
VOIDSTAR_EXPORT uint64_t instrumentation_max_abi_version(void);
VOIDSTAR_EXPORT uint64_t instrumentation_request_abi_version(uint64_t requested);

/* Write one line to stderr; if that fails there is nowhere else to say so. */
static void say(const char *line) {
    if (write(STDERR_FILENO, line, strlen(line)) < 0) {
        /* Nothing more can be reported. */
    }
}

/* Say on stderr why no word can be read, and end the process. */
static void random_unavailable(const char *reason) __attribute__((noreturn));
static void random_unavailable(const char *reason) {
    char line[256];
    snprintf(line, sizeof line,
             "hermit libvoidstar: " GUEST_RANDOM_PATH ": %s\n", reason);
    say(line);
    abort();
}

/* The descriptor of GUEST_RANDOM_PATH, or -1 until the first call opens it. */
static int random_fd = -1;

/*
 * The descriptor every call reads, opened by the process's first call. A
 * forked child inherits it. If two threads make their first calls at once,
 * each may open the path, and the one that loses the exchange closes its
 * descriptor, so the process keeps one. Unlike pthread_once, this guard
 * needs no libpthread, which glibc before 2.34 keeps out of libc and the
 * build command above does not link, and a fork cannot leave it half-done.
 */
static int random_descriptor(void) {
    int fd = __atomic_load_n(&random_fd, __ATOMIC_ACQUIRE);
    if (fd >= 0) {
        return fd;
    }
    int opened = open(GUEST_RANDOM_PATH, O_RDONLY | O_CLOEXEC);
    if (opened < 0) {
        random_unavailable(strerror(errno));
    }
    int expected = -1;
    if (__atomic_compare_exchange_n(&random_fd, &expected, opened, false,
                                    __ATOMIC_ACQ_REL, __ATOMIC_ACQUIRE)) {
        return opened;
    }
    close(opened);
    return expected;
}

/* One word: exactly one 8-byte read of GUEST_RANDOM_PATH. */
uint64_t fuzz_get_random(void) {
    uint64_t word;
    ssize_t got = read(random_descriptor(), &word, sizeof word);
    if (got < 0) {
        random_unavailable(strerror(errno));
    }
    if (got != (ssize_t)sizeof word) {
        char reason[48];
        snprintf(reason, sizeof reason, "short read %zd", got);
        random_unavailable(reason);
    }
    return word;
}

/* Set once the process has said that records are dropped. */
static int said_dropped;

/* Set once the process has reported a failure to append a record. */
static int said_append_failed;

/* True for the first caller only, across threads. */
static bool first_time(int *said) {
    return __atomic_exchange_n(said, 1, __ATOMIC_RELAXED) == 0;
}

/* Report, once per process, why a record could not be appended. */
static void append_failed(const char *dir, const char *reason) {
    if (!first_time(&said_append_failed)) {
        return;
    }
    char line[512];
    snprintf(line, sizeof line,
             "hermit libvoidstar: %s/" OUTPUT_FILE_NAME
             ": %s; the record is lost, and later failures are not reported\n",
             dir, reason);
    say(line);
}

/*
 * Append one record to $ANTITHESIS_OUTPUT_DIR/sdk.jsonl: its bytes unchanged,
 * plus a newline if it does not end with one, in a single write(2), so that
 * records from several writers never interleave. With the variable unset or
 * empty the record is dropped, and the process says so once. A failure is
 * reported once and never ends the process.
 */
static void append_record(const char *message, size_t length) {
    const char *dir = getenv(OUTPUT_DIR_VARIABLE);
    if (dir == NULL || dir[0] == '\0') {
        if (first_time(&said_dropped)) {
            say("hermit libvoidstar: " OUTPUT_DIR_VARIABLE
                " unset; SDK records are dropped\n");
        }
        return;
    }

    size_t path_size = strlen(dir) + sizeof "/" OUTPUT_FILE_NAME;
    char *path = (char *)malloc(path_size);
    bool newline = length > 0 && message[length - 1] == '\n';
    size_t total = newline ? length : length + 1;
    char *line = newline ? NULL : (char *)malloc(total);
    if (path == NULL || (!newline && line == NULL)) {
        append_failed(dir, strerror(ENOMEM));
        free(path);
        free(line);
        return;
    }
    snprintf(path, path_size, "%s/" OUTPUT_FILE_NAME, dir);
    if (!newline) {
        if (length > 0) {
            memcpy(line, message, length);
        }
        line[length] = '\n';
    }

    int fd = open(path, O_WRONLY | O_APPEND | O_CREAT | O_CLOEXEC, 0644);
    if (fd < 0) {
        append_failed(dir, strerror(errno));
    } else {
        ssize_t wrote = write(fd, newline ? message : line, total);
        if (wrote < 0) {
            append_failed(dir, strerror(errno));
        } else if ((size_t)wrote != total) {
            char reason[64];
            snprintf(reason, sizeof reason, "short write %zd of %zu", wrote,
                     total);
            append_failed(dir, reason);
        }
        close(fd);
    }
    free(path);
    free(line);
}

void fuzz_json_data(const char *message, size_t length) {
    append_record(message, length);
}

void fuzz_json_data_unfilterable(const char *message, size_t length) {
    append_record(message, length);
}

/*
 * Records are written before fuzz_json_data returns. Returning from here
 * confirms nothing: it is not a receipt that any record reached a reader.
 */
void fuzz_flush(void) {}

/*
 * Coverage. Nothing below records anything; each function returns the value
 * the stub returns. The guards stay as the program left them (zeroed), as in
 * the stub.
 */
void __sanitizer_cov_trace_pc_guard_init(uint32_t *start, uint32_t *stop) {
    (void)start;
    (void)stop;
}

void __sanitizer_cov_trace_pc_guard_internal(uint32_t *guard, uint64_t edge) {
    (void)guard;
    (void)edge;
}

void __sanitizer_cov_trace_pc_guard(uint32_t *guard) {
    (void)guard;
}

void __sanitizer_cov_pcs_init(const uintptr_t *pcs_beg,
                              const uintptr_t *pcs_end) {
    (void)pcs_beg;
    (void)pcs_end;
}

size_t init_coverage_module(size_t edge_count, const char *symbol_file_name) {
    (void)edge_count;
    (void)symbol_file_name;
    return (size_t)1 << 32;
}

bool notify_coverage(size_t edge_plus_module_id) {
    (void)edge_plus_module_id;
    return false;
}

uint64_t notify_coverage_v2(size_t edge_plus_module_id,
                            uint64_t hit_since_last_call) {
    (void)edge_plus_module_id;
    (void)hit_since_last_call;
    return (UINT64_C(0xFFFFF) << 20) | UINT64_C(0xFFFFF);
}

const uint64_t *coverage_lease_generation_addr(void) {
    static const uint64_t generation = 0;
    return &generation;
}

uint64_t instrumentation_max_abi_version(void) {
    return HERMIT_VOIDSTAR_ABI_VERSION;
}

/*
 * Every request, 0 included, is granted the one version there is, so the
 * grant is the same on every call and is never 0.
 */
uint64_t instrumentation_request_abi_version(uint64_t requested) {
    (void)requested;
    return HERMIT_VOIDSTAR_ABI_VERSION;
}

#ifdef __cplusplus
}
#endif

#pragma GCC visibility pop
