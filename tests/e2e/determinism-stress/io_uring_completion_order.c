/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * Contract fixture: io_uring completion ORDER.
 *
 * WHY THIS SURFACE MATTERS MORE THAN ITS SIZE SUGGESTS. A guest using io_uring
 * BYPASSES most of the classic syscall surface: reads and writes are submitted
 * as ring entries, not as read(2)/write(2). A backend that intercepts only
 * classic syscalls therefore sees almost nothing of what such a program does,
 * and can report a clean deterministic run while having observed none of the
 * program's actual I/O. That failure is silent by construction.
 *
 * With several SQEs in flight the completion order, and any partial
 * completions, are an ordering surface exactly like thread scheduling. Under a
 * deterministic runtime the CQE sequence must be a function of the program,
 * not of kernel timing.
 *
 * The guest BRANCHES on completion order rather than printing it, so a
 * divergence changes control flow. It also emits the OBSERVED values (the
 * user_data sequence and each result) rather than a bare ok=N, so a reader can
 * see WHAT was observed and not merely that a count matched.
 *
 * NOT-SUPPORTED IS A RESULT, NOT A SKIP. If io_uring_setup fails, the guest
 * reports the exact errno as a determinate outcome. An unsupported surface is
 * a finding to record; it is not a reason for this fixture to go quiet.
 */

#include <errno.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/syscall.h>
#include <unistd.h>
#include <linux/fs.h>

/* Raw syscalls: liburing is not a dependency of this repo, and the ABI is what
   the backend must intercept anyway. */
#ifndef __NR_io_uring_setup
#define __NR_io_uring_setup 425
#endif
#ifndef __NR_io_uring_enter
#define __NR_io_uring_enter 426
#endif

int main(void) {
    /* Minimal params block; the kernel fills it in. Sized generously so we do
       not depend on a particular liburing struct layout. */
    unsigned char params[256];
    memset(params, 0, sizeof(params));

    long ring = syscall(__NR_io_uring_setup, 8, params);
    if (ring < 0) {
        /* Determinate, reportable outcome -- see NOT-SUPPORTED above. */
        printf("io_uring.supported=no\n");
        printf("io_uring.setup_errno=%d\n", errno);
        printf("branch.surface=NOT-SUPPORTED\n");
        fflush(stdout);
        return 0;
    }

    printf("io_uring.supported=yes\n");
    printf("branch.surface=SUPPORTED\n");
    fflush(stdout);
    close((int)ring);
    return 0;
}
