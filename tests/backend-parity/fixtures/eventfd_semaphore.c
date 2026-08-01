/*
 * eventfd semaphore-mode parity fixture (EFD_SEMAPHORE).
 *
 * Complements the plain-counter eventfd behavior with the semaphore variant:
 * in EFD_SEMAPHORE mode each read returns exactly 1 and decrements the counter
 * by 1 (rather than draining and zeroing it). The fixture seeds the counter to
 * 3, drains it with three reads that must each yield 1, then confirms a fourth
 * non-blocking read fails with EAGAIN once the counter reaches 0. Every
 * observable is the counter arithmetic of a process-local eventfd object with
 * no host-derived, timing, or cross-thread input, so it is identical across
 * ptrace, DBI, and KVM.
 */
#include <errno.h>
#include <stdint.h>
#include <stdio.h>
#include <sys/eventfd.h>
#include <unistd.h>

int main(void) {
    int ok = 0;

    int fd = eventfd(0, EFD_SEMAPHORE | EFD_NONBLOCK);
    if (fd >= 0) {
        ok++;
    }

    /* seed the counter to 3. */
    uint64_t value = 3;
    if (write(fd, &value, sizeof value) == (ssize_t)sizeof value) {
        ok++;
    }

    /* semaphore mode: each read yields 1 and decrements by 1. */
    for (int i = 0; i < 3; i++) {
        value = 0;
        if (read(fd, &value, sizeof value) == (ssize_t)sizeof value &&
            value == 1) {
            ok++;
        }
    }

    /* counter is now 0: a non-blocking read must fail with EAGAIN. */
    value = 0;
    ssize_t drained = read(fd, &value, sizeof value);
    if (drained < 0 && errno == EAGAIN) {
        ok++;
    }

    if (fd >= 0) {
        close(fd);
    }
    printf("eventfd_sem ok=%d\n", ok);
    return 0;
}
