/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * Reads stderr's file status flags, sets O_APPEND, and reads them back,
 * reporting only whether the guest itself saw the bit change.
 *
 * This separates the two halves of a containment claim that are easy to
 * conflate. `fcntl(F_SETFL)` mutates the open file DESCRIPTION, which Hermit
 * may be sharing with the process that invoked it, so a fix that simply
 * dropped the guest's request would make the supervisor's descriptor look
 * correct while silently breaking the guest. The printed line is the guest's
 * OWN view; the test that runs this program separately checks the supervisor's
 * descriptor. Both must hold: the guest sees its change, and nobody else does.
 *
 * Only the O_APPEND bit is printed, not the raw flag word, because the rest of
 * the word legitimately depends on what the caller redirected stderr to.
 */

#include <fcntl.h>
#include <stdio.h>
#include <unistd.h>

int main(void) {
    int before = fcntl(STDERR_FILENO, F_GETFL);
    if (before < 0) {
        perror("fcntl(F_GETFL) before");
        return 1;
    }
    if (fcntl(STDERR_FILENO, F_SETFL, before | O_APPEND) < 0) {
        perror("fcntl(F_SETFL, O_APPEND)");
        return 1;
    }
    int after = fcntl(STDERR_FILENO, F_GETFL);
    if (after < 0) {
        perror("fcntl(F_GETFL) after");
        return 1;
    }
    printf(
        "append_before=%d append_after=%d\n",
        (before & O_APPEND) != 0,
        (after & O_APPEND) != 0);
    return 0;
}
