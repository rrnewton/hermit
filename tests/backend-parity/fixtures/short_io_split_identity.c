/*
 * Backend-parity contract: PARTIAL-TRANSFER SPLIT PATTERN, plus completion.
 *
 * Short reads and short writes are legal, so a backend that splits a transfer
 * differently is POSIX-correct and still non-deterministic: every call
 * succeeds and nothing shows up in an exit code or a byte total. This fixture
 * asserts the SEQUENCE of return values, not the total.
 *
 * IDENTITY ALONE IS NOT ENOUGH, and that is the load-bearing design point.
 * hermit-det3 measured (task file-io-determinism-residue) that a blocking
 * write() to a pipe returns a stable SHORT count under hermit where Linux
 * returns the full count -- 2/2 identical under hermit, 3/3 full natively.
 * A "same split every run" assertion scores that clean. So this contract
 * asserts BOTH:
 *
 *   ANTI-VACUITY the transfer must actually go short at least once;
 *   IDENTITY    two independent transfers of the same size, in the same run,
 *               must produce the identical split sequence; and
 *   COMPLETION  a correct application loop must still move every byte.
 *
 * A deterministic-but-wrong split fails COMPLETION; a nondeterministic split
 * fails IDENTITY. Neither check subsumes the other.
 *
 * HOST-INDEPENDENCE. The pipe capacity, and therefore the chunk size and chunk
 * count, differ across hosts and are never asserted or printed as absolute
 * numbers. Every assertion is relational -- transfer A against transfer B in
 * the same process -- and the oracle is a fixed string, "shortio ok=6".
 *
 * BRANCHING ON THE BOUNDARY. The writer takes a different code path whenever a
 * return value is short, and the number of times that branch is taken is
 * itself compared between the two transfers. A split difference therefore
 * changes control flow, not merely a logged number.
 *
 * DELIBERATELY NOT COVERED, because neither is observable on this build:
 *   - sendfile(2) to a socket: refused with ENOSYS at
 *     detcore/src/syscalls/files.rs:840-844, so there are no partial sends.
 *   - the nonblocking-poller readv variant: it deadlocks in pthread_join under
 *     hermit (det3), so a fixture built on it would hang rather than fail.
 *
 * _GNU_SOURCE is supplied by the harness compile flags; do not define it here.
 */

#include <errno.h>
#include <fcntl.h>
#include <stdbool.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/wait.h>
#include <unistd.h>

enum { CHUNKS = 8, MAX_RETURNS = 512 };

/* One transfer's observed split: the sequence of write() return values, and
 * how many of them were short. */
struct split {
    int count;
    ssize_t value[MAX_RETURNS];
    int short_returns;
    size_t total;
};

static bool split_equal(const struct split *a, const struct split *b) {
    if (a->count != b->count || a->short_returns != b->short_returns) {
        return false;
    }
    for (int i = 0; i < a->count; i++) {
        if (a->value[i] != b->value[i]) {
            return false;
        }
    }
    return true;
}

/* Write `total` bytes to `fd`, looping to completion, recording every return
 * value. Returns false only on a hard error. */
static bool transfer(int fd, const char *buffer, size_t total, struct split *out) {
    memset(out, 0, sizeof *out);
    size_t done = 0;
    while (done < total) {
        size_t want = total - done;
        ssize_t got = write(fd, buffer + done, want);
        if (got < 0) {
            if (errno == EINTR) {
                continue; /* not a split; do not record it */
            }
            return false;
        }
        if (out->count >= MAX_RETURNS) {
            return false;
        }
        out->value[out->count++] = got;
        if ((size_t)got < want) {
            /* BRANCH ON THE SHORT BOUNDARY: a partial return takes this path,
             * a full one does not. The count is compared across transfers. */
            out->short_returns++;
        }
        done += (size_t)got;
    }
    out->total = done;
    return true;
}

/* Drain `total` bytes from `fd` and exit; the child keeps the pipe moving so
 * the writer's loop can complete instead of blocking forever. */
static void drain_child(int fd, size_t total) {
    char sink[4096];
    size_t seen = 0;
    while (seen < total) {
        ssize_t got = read(fd, sink, sizeof sink);
        if (got < 0) {
            if (errno == EINTR) {
                continue;
            }
            _exit(1);
        }
        if (got == 0) {
            break;
        }
        seen += (size_t)got;
    }
    _exit(seen == total ? 0 : 1);
}

/* Run one transfer of `total` bytes through a fresh blocking pipe. */
static bool one_transfer(const char *buffer, size_t total, struct split *out) {
    int fds[2];
    if (pipe(fds) != 0) {
        return false;
    }
    pid_t child = fork();
    if (child < 0) {
        close(fds[0]);
        close(fds[1]);
        return false;
    }
    if (child == 0) {
        close(fds[1]);
        drain_child(fds[0], total);
    }
    close(fds[0]);
    bool ok = transfer(fds[1], buffer, total, out);
    close(fds[1]);
    int status = 0;
    if (waitpid(child, &status, 0) != child) {
        return false;
    }
    return ok && WIFEXITED(status) && WEXITSTATUS(status) == 0;
}

int main(void) {
    enum { EXPECTED_CHECKS = 7 };
    int ok = 0;

    /* Size the transfer off the pipe's CAPACITY (F_GETPIPE_SZ), not PIPE_BUF.
     * They differ by 16x here (4096 vs 65536), and sizing off PIPE_BUF made an
     * earlier revision of this fixture VACUOUS: 8 * 4096 fits entirely in the
     * pipe, so the write never went short and the split contract tested
     * nothing. Check 7 below now asserts a short return actually occurred, so
     * that mistake cannot come back silently. */
    size_t total = 0;
    {
        int probe[2];
        if (pipe(probe) != 0) {
            printf("shortio ok=%d\n", ok);
            return EXIT_FAILURE;
        }
        long capacity = (long)fcntl(probe[1], F_GETPIPE_SZ);
        close(probe[0]);
        close(probe[1]);
        if (capacity <= 0) {
            printf("shortio ok=%d\n", ok);
            return EXIT_FAILURE;
        }
        total = (size_t)capacity * CHUNKS;
    }
    ok++; /* 1: a transfer size was derived without hardcoding a host number */

    char *buffer = malloc(total);
    if (buffer == NULL) {
        printf("shortio ok=%d\n", ok);
        return EXIT_FAILURE;
    }
    for (size_t i = 0; i < total; i++) {
        buffer[i] = (char)('a' + (i % 26));
    }

    struct split first;
    struct split second;
    bool ran_first = one_transfer(buffer, total, &first);
    if (ran_first) {
        ok++; /* 2 */
    }
    bool ran_second = one_transfer(buffer, total, &second);
    if (ran_second) {
        ok++; /* 3 */
    }

    /* COMPLETION: a deterministic-but-wrong short split still has to move every
     * byte once the application loops. This is the clause that a pure identity
     * contract would miss. */
    if (ran_first && first.total == total) {
        ok++; /* 4 */
    }
    if (ran_second && second.total == total) {
        ok++; /* 5 */
    }

    /* IDENTITY: same size, same pipe kind, same process -- the split sequence
     * and the short-boundary branch count must both repeat exactly. */
    bool identical = ran_first && ran_second && split_equal(&first, &second);
#ifdef HERMIT_TEST_SHORTIO_PERTURB_SPLIT
    /* Plant a split difference that leaves the byte total untouched: the naive
     * total-only contract passes here and this one must not. */
    identical = ran_first && ran_second && first.count == second.count
        && first.value[0] == second.value[0] - 1;
#endif
    if (identical) {
        ok++; /* 6 */
    }

    /* ANTI-VACUITY, asserted rather than assumed: the transfer must actually
     * have gone short at least once, or the split contract above is comparing
     * two one-element sequences and proves nothing. */
    if (ran_first && first.short_returns > 0) {
        ok++; /* 7 */
    }

    free(buffer);
#ifdef HERMIT_TEST_ORACLE_NEGATIVE
    ok--; /* stable wrong stdout must be rejected by the normal exit oracle */
#endif
    printf("shortio ok=%d\n", ok);
    return ok == EXPECTED_CHECKS ? EXIT_SUCCESS : EXIT_FAILURE;
}
