/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * Sets O_NONBLOCK on stderr and THEN sets O_APPEND, and reports what the guest
 * itself ends up seeing.
 *
 * The order is the whole point. O_NONBLOCK is the one settable flag that cannot
 * be modeled -- Detcore keeps a separate physical view of it and panics when the
 * two disagree -- so it is forwarded to the supervisor's descriptor. A revision
 * that handled this by reverting the WHOLE call whenever O_NONBLOCK was involved
 * latched: once the model carried the flag, every later F_SETFL and F_GETFL on
 * that description reverted too, and the O_APPEND escape the containment exists
 * to prevent came straight back. Measured 0x8001 -> 0x8c01 on the supervisor's
 * descriptor, both backends.
 *
 * Neither call alone catches that. A guest that sets only O_APPEND stays on the
 * contained path; a guest that sets only O_NONBLOCK has nothing left to leak.
 * It takes the pair, in this order.
 */

#define _GNU_SOURCE

#include <fcntl.h>
#include <stdio.h>
#include <unistd.h>

int main(void) {
    int flags = fcntl(STDERR_FILENO, F_GETFL);
    if (flags < 0) {
        perror("fcntl(F_GETFL)");
        return 1;
    }
    /* Forwarded to the supervisor by design; see the F_SETFL arm in detcore. */
    if (fcntl(STDERR_FILENO, F_SETFL, flags | O_NONBLOCK) < 0) {
        perror("fcntl(F_SETFL, O_NONBLOCK)");
        return 1;
    }
    /* Must remain contained even though the description is now nonblocking. */
    if (fcntl(STDERR_FILENO, F_SETFL, flags | O_NONBLOCK | O_APPEND) < 0) {
        perror("fcntl(F_SETFL, O_NONBLOCK|O_APPEND)");
        return 1;
    }
    int seen = fcntl(STDERR_FILENO, F_GETFL);
    if (seen < 0) {
        perror("fcntl(F_GETFL) after");
        return 1;
    }
    printf(
        "guest_append=%d guest_nonblock=%d\n",
        (seen & O_APPEND) != 0,
        (seen & O_NONBLOCK) != 0);
    return 0;
}
