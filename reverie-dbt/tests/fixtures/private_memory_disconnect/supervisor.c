/* Copyright (c) Meta Platforms, Inc. and affiliates.
 * Licensed under the BSD-style license in the root LICENSE file. */
#define _GNU_SOURCE
#include "record.h"
#include <errno.h>
#include <fcntl.h>
#include <sched.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/mman.h>
#include <sys/prctl.h>
#include <sys/wait.h>
#include <unistd.h>

static void require(int ok, const char *what) {
    if (!ok) { perror(what); exit(90); }
}

static void await_word(_Atomic uint64_t *record, size_t word, pid_t child) {
    while (atomic_load_explicit(&record[word], memory_order_acquire) == 0) {
        int status;
        require(waitpid(child, &status, WNOHANG) == 0, "launcher ended before handshake");
        sched_yield();
    }
}

int main(int argc, char **argv) {
    require(argc == 6, "arguments: drrun client record guest mode");
    int synchronize = argv[5][0] == 's';
    require(prctl(PR_SET_CHILD_SUBREAPER, 1) == 0, "subreaper");
    int file = open(argv[3], O_RDWR | O_CREAT | O_EXCL, 0600);
    require(file >= 0 && ftruncate(file, 4096) == 0, "fresh shared record");
    _Atomic uint64_t *record = mmap(NULL, 4096, PROT_READ | PROT_WRITE,
                                   MAP_SHARED, file, 0);
    require(record != MAP_FAILED, "record mapping");
    close(file);
    for (size_t i = 0; i < WORDS; ++i) atomic_init(&record[i], 0);
    atomic_store(&record[MAGIC], DISCONNECT_MAGIC);
    int exit_gate[2], witness[2];
    require(pipe(exit_gate) == 0 && pipe2(witness, O_NONBLOCK) == 0, "pipes");
    for (size_t i = 0; i < 2; ++i)
        require(exit_gate[i] != 198 && exit_gate[i] != WITNESS_FD &&
                witness[i] != 198 && witness[i] != WITNESS_FD, "reserved descriptor collision");
    pid_t child = fork();
    require(child >= 0, "fork launcher");
    if (child == 0) {
        require(dup2(exit_gate[0], 198) == 198 &&
                dup2(witness[1], WITNESS_FD) == WITNESS_FD, "inherited descriptors");
        require(fcntl(WITNESS_FD, F_GETFD) == 0 &&
                (fcntl(WITNESS_FD, F_GETFL) & O_NONBLOCK) != 0, "witness visibility");
        close(exit_gate[0]); close(exit_gate[1]); close(witness[0]); close(witness[1]);
        execl(argv[1], argv[1], "-c", argv[2], argv[3], argv[5], "--", argv[4], (char *)NULL);
        _exit(91);
    }
    close(exit_gate[0]); close(witness[1]);
    await_word(record, READY, child);
    atomic_store_explicit(&record[START], 1, memory_order_release);
    await_word(record, ENTERED, child);
    char byte = 'x';
    if (synchronize)
        atomic_store_explicit(&record[SYNC_START], 1, memory_order_release);
    else {
        atomic_store(&record[EXIT_REQUESTED], 1);
        require(write(exit_gate[1], &byte, 1) == 1, "request application exit");
        close(exit_gate[1]);
    }
    for (;;) {
        if (atomic_load_explicit(&record[EARLY], memory_order_acquire) != 0)
            break;
        struct disconnect_witness event = {0};
        ssize_t n = read(witness[0], &event, sizeof(event));
        if (n > 0) {
            require(n == sizeof(event) && event.magic == WITNESS_MAGIC,
                    "complete private SDK witness");
            if (synchronize) {
                require(event.kind == WITNESS_SAFE || event.kind == WITNESS_UNSAFE,
                        "actual safe-point decision witness");
                if (event.tid != atomic_load(&record[OWNER_TID]))
                    continue;
                atomic_store(&record[event.kind == WITNESS_SAFE ? SAFE_OBSERVED :
                                     UNSAFE_OBSERVED], 1);
            } else {
                require(event.kind == WITNESS_WAIT && event.tid == (uint64_t)child &&
                        event.detail > 0, "actual contended-close acquisition witness");
                atomic_store(&record[WAIT_OBSERVED], 1);
            }
            break;
        }
        /* The process can close its descriptor after publishing EARLY between
         * the acquire above and this read. Preserve that real boundary failure. */
        if (n == 0 && atomic_load_explicit(&record[EARLY], memory_order_acquire) != 0)
            break;
        require(n == -1 && (errno == EAGAIN || errno == EINTR), "missing close witness");
        sched_yield();
    }
    /* Either real contention or an already-recorded early destruction permits
     * release. No process callback is needed to release a correct implementation. */
    atomic_store_explicit(&record[RELEASE], 1, memory_order_release);
    if (synchronize) {
        await_word(record, SYNC_DONE, child);
        await_word(record, COMPLETED, child);
        atomic_store(&record[EXIT_REQUESTED], 1);
        require(write(exit_gate[1], &byte, 1) == 1, "request application exit");
        close(exit_gate[1]);
    }
    int status;
    require(waitpid(child, &status, 0) == child, "launcher wait");
    fprintf(stderr, "PRIVATE_MEMORY_DISCONNECT launcher_status=%d\n", status);
    int other_status;
    pid_t other;
    int descendants_ok = 1;
    while ((other = waitpid(-1, &other_status, __WALL)) > 0) {
        descendants_ok &= WIFEXITED(other_status) && WEXITSTATUS(other_status) == 0;
        fprintf(stderr, "PRIVATE_MEMORY_DISCONNECT reaped=%ld raw_status=%d\n",
                (long)other, other_status);
    }
    require(other == -1 && errno == ECHILD, "all children reaped");
    close(witness[0]);
    fprintf(stderr, "PRIVATE_MEMORY_DISCONNECT entered=%llu completed=%llu early=%llu "
            "destroyed=%llu wait=%llu calls=%llu api_returned=%llu late=%llu\n",
            (unsigned long long)atomic_load(&record[ENTERED]),
            (unsigned long long)atomic_load(&record[COMPLETED]),
            (unsigned long long)atomic_load(&record[EARLY]),
            (unsigned long long)atomic_load(&record[DESTROYED]),
            (unsigned long long)atomic_load(&record[WAIT_OBSERVED]),
            (unsigned long long)atomic_load(&record[CALLS]),
            (unsigned long long)atomic_load(&record[API_RETURNED]),
            (unsigned long long)atomic_load(&record[LATE]));
    if (atomic_load(&record[EARLY]) != 0 || atomic_load(&record[COMPLETED]) != 1 ||
        atomic_load(&record[DESTROYED]) != 1 || atomic_load(&record[LATE]) != 0) {
        fprintf(stderr, "ASSERTION FAILED: observer completion precedes process-state destruction\n");
        return 101;
    }
    require(WIFEXITED(status) && WEXITSTATUS(status) == 0, "successful guest exit");
    require(descendants_ok, "successful adopted-client-thread exits");
    if (synchronize) {
        fprintf(stderr, "PRIVATE_MEMORY_DISCONNECT safe=%llu unsafe=%llu held=%llu "
                "resumed=%llu unsuspended=%llu\n",
                (unsigned long long)atomic_load(&record[SAFE_OBSERVED]),
                (unsigned long long)atomic_load(&record[UNSAFE_OBSERVED]),
                (unsigned long long)atomic_load(&record[HELD_WAS_SUSPENDED]),
                (unsigned long long)atomic_load(&record[RESUME_OK]),
                (unsigned long long)atomic_load(&record[UNSUSPENDED]));
        if (atomic_load(&record[SAFE_OBSERVED]) != 0 ||
            atomic_load(&record[HELD_WAS_SUSPENDED]) != 0 ||
            atomic_load(&record[UNSAFE_OBSERVED]) != 1 ||
            atomic_load(&record[RESUME_OK]) != 1) {
            fprintf(stderr, "ASSERTION FAILED: held observer is not a safe suspension point\n");
            return 101;
        }
    } else
        require(atomic_load(&record[WAIT_OBSERVED]) == 1, "actual close contention occurred");
    return 0;
}
