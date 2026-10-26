/* Copyright (c) Meta Platforms, Inc. and affiliates.
 * Licensed under the BSD-style license in the root LICENSE file. */
#define _GNU_SOURCE
#include "record.h"
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/mman.h>
#include <sys/prctl.h>
#include <sys/wait.h>
#include <unistd.h>

static void require(int ok, const char *what) {
    if (!ok) { perror(what); exit(90); }
}

int main(int argc, char **argv) {
    require(argc == 5, "arguments: drrun client record guest");
    require(prctl(PR_SET_CHILD_SUBREAPER, 1) == 0, "subreaper");
    int record_fd = open(argv[3], O_RDWR);
    require(record_fd >= 0, "record open");
    _Atomic uint64_t *record = mmap(NULL, 4096, PROT_READ | PROT_WRITE,
                                   MAP_SHARED, record_fd, 0);
    require(record != MAP_FAILED, "record map");
    close(record_fd);
    int armed[2], release[2], ready[2];
    require(pipe(armed) == 0 && pipe(release) == 0 && pipe(ready) == 0, "pipes");
    pid_t parent = fork();
    require(parent >= 0, "fork launcher");
    if (parent == 0) {
        require(dup2(armed[1], 197) == 197 && dup2(release[0], 198) == 198 &&
                    dup2(ready[0], 199) == 199 && dup2(ready[1], 200) == 200,
                "guest descriptors");
        close(armed[0]); close(armed[1]); close(release[0]); close(release[1]);
        close(ready[0]); close(ready[1]);
        execl(argv[1], argv[1], "-c", argv[2], "shared", argv[3], "--", argv[4],
              "shared", (char *)NULL);
        _exit(91);
    }
    close(armed[1]); close(release[0]); close(ready[0]); close(ready[1]);
    char byte;
    require(read(armed[0], &byte, 1) == 1, "parent armed");
    close(armed[0]);
    int status;
    require(waitpid(parent, &status, 0) == parent && WIFEXITED(status) &&
                WEXITSTATUS(status) == 0, "parent wait status");
    atomic_store(&record[EARLY_BEFORE_RELEASE], atomic_load(&record[EARLY]));
    atomic_store(&record[EXITS_BEFORE_RELEASE], atomic_load(&record[EXIT_EVENTS]));
    atomic_store_explicit(&record[PARENT_READY], 1, memory_order_release);
    require(write(release[1], &byte, 1) == 1, "release surviving child");
    close(release[1]);
    pid_t child = waitpid(-1, &status, 0);
    require(child > 0 && child != parent && WIFEXITED(status) &&
                WEXITSTATUS(status) == 0, "last child wait status");
    require((uint64_t)child == atomic_load(&record[CHILD_PID]), "actual child identity");
    atomic_store(&record[CHILD_DONE], 1);
    errno = 0;
    require(waitpid(-1, &status, WNOHANG) == -1 && errno == ECHILD, "all children reaped");
    fprintf(stderr, "PRIVATE_MEMORY_FORK supervisor parent=0 child=0 reaped=1\n");
    return 0;
}
