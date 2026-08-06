/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * STATICALLY LINKED AGAINST GLIBC -- the realistic high-value shape for a
 * main-ELF patcher.
 *
 * The inline-assembly guest proves a rewriter can patch a site we hand it, and
 * the freestanding `-nostdlib` guest proves it can cope with a binary that has
 * no PLT at all. Neither is a program anyone ships. This one is: it contains no
 * inline assembly, makes ordinary libc calls, and is linked `-static`, so
 * LIBC'S OWN SYSCALL INSTRUCTIONS ARE LINKED INTO THE MAIN ELF.
 *
 * That is the case a rewriter must actually survive: many sites, inside
 * functions we did not author, surrounded by instruction sequences a hand-written
 * stub never produces -- optimized memcpy variants, locale and malloc paths,
 * stdio buffering. A rewriter that handles the five-line stub and falls over
 * here has not been shown to work on real programs.
 *
 * DETERMINISM. Writes a fixed payload to a temporary file, reads it back, folds
 * an order-sensitive checksum over the bytes, and prints fixed text. No clock,
 * pid, environment, randomness, or directory enumeration (enumeration order is
 * host state -- see readdir_order_identity.c). stdout is flushed explicitly so
 * ordering does not depend on libc's exit-time flush.
 */

#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

int main(void) {
    static const char payload[] =
        "static glibc guest: libc syscall sites live in the main ELF\n";
    const size_t n = sizeof(payload) - 1;

    char path[] = "/tmp/static_libc_syscall_sites_XXXXXX";
    int fd = mkstemp(path);
    if (fd < 0) {
        perror("mkstemp");
        return 2;
    }

    unsigned long written = 0;
    for (int i = 0; i < 4; i++) {
        ssize_t w = write(fd, payload, n);
        if (w != (ssize_t)n) {
            perror("write");
            close(fd);
            unlink(path);
            return 2;
        }
        written += (unsigned long)w;
    }

    if (lseek(fd, 0, SEEK_SET) != 0) {
        perror("lseek");
        close(fd);
        unlink(path);
        return 2;
    }

    unsigned long readback = 0;
    unsigned long checksum = 0;
    char buf[64];
    for (;;) {
        ssize_t r = read(fd, buf, sizeof(buf));
        if (r < 0) {
            perror("read");
            close(fd);
            unlink(path);
            return 2;
        }
        if (r == 0) {
            break;
        }
        readback += (unsigned long)r;
        for (ssize_t i = 0; i < r; i++) {
            checksum = checksum * 131u + (unsigned char)buf[i];
        }
    }

    close(fd);
    unlink(path);

    printf("static_libc written=%lu readback=%lu checksum=%lu\n",
           written, readback, checksum);
    fflush(stdout);
    return written == readback ? 0 : 1;
}
