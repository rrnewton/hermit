/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * Asks stdout to change each file status flag whose behavior Detcore does NOT
 * implement for inherited stdio -- O_ASYNC, O_DIRECT, O_NOATIME -- and requires
 * each request to be refused with EOPNOTSUPP AND the flag to be left alone.
 *
 * Both halves are the point. A refusal that returns the wrong errno is one
 * defect; a refusal that reports success and reflects the flag through F_GETFL
 * without implementing it is the defect this refusal exists to prevent, because
 * the guest would then make decisions from a state that does not exist. Testing
 * only "the call failed" would accept the first, and testing only the flag would
 * accept a refusal that returned, say, EINVAL.
 *
 * The bit is TOGGLED rather than set, so the request is a real change whatever
 * the caller's descriptor started with. `fcntl(F_SETFL)` ignores a request that
 * changes nothing, and a fixture that merely re-set an already-set flag would
 * pass against no refusal at all.
 *
 * O_APPEND and O_NONBLOCK are deliberately NOT covered here: their behavior IS
 * implemented for inherited stdio, so they are the qualifying cases and belong
 * to stdio_status_flag_containment.c and the nonblocking fixtures.
 */

#define _GNU_SOURCE

#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <unistd.h>

struct unsupported_flag {
    const char *name;
    int bit;
};

int main(void) {
    const struct unsupported_flag flags[] = {
        {"O_ASYNC", O_ASYNC},
        {"O_DIRECT", O_DIRECT},
        {"O_NOATIME", O_NOATIME},
    };
    int failures = 0;

    for (unsigned i = 0; i < sizeof(flags) / sizeof(flags[0]); i++) {
        int before = fcntl(STDOUT_FILENO, F_GETFL);
        if (before < 0) {
            perror("fcntl(F_GETFL) before");
            return 1;
        }

        errno = 0;
        int result = fcntl(STDOUT_FILENO, F_SETFL, before ^ flags[i].bit);
        int setfl_errno = errno;

        int after = fcntl(STDOUT_FILENO, F_GETFL);
        if (after < 0) {
            perror("fcntl(F_GETFL) after");
            return 1;
        }

        int refused = result == -1 && setfl_errno == EOPNOTSUPP;
        int unchanged = (after & flags[i].bit) == (before & flags[i].bit);

        dprintf(
            STDERR_FILENO,
            "%s result=%d errno=%d refused=%d bit_before=%d bit_after=%d unchanged=%d\n",
            flags[i].name,
            result,
            setfl_errno,
            refused,
            (before & flags[i].bit) != 0,
            (after & flags[i].bit) != 0,
            unchanged);

        if (!refused || !unchanged) {
            failures++;
        }
    }

    return failures == 0 ? 0 : 1;
}
