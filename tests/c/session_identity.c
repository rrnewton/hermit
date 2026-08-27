/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * Pins Linux session and process-group behavior under Detcore's model.
 *
 * handle_setsid and handle_setpgid inject the Linux mutation and mirror a
 * successful result into Detcore's process model. getpgid/getpgrp read that
 * model, including the initial group and the setpgid transition. getsid remains
 * a kernel query, so this fixture does not claim complete session-identity
 * virtualization.
 */

/* getsid and getpgid need the POSIX feature-test macro; the harness compiles
 * with -Werror=implicit-function-declaration and a stricter -std than a bare
 * `gcc file.c`, so an implicit declaration here is a build error rather than
 * a warning. */
#define _GNU_SOURCE

#include <errno.h>
#include <stdio.h>
#include <unistd.h>

int main(void) {
    printf("before sid=%d pgid=%d\n", getsid(0), getpgid(0));

    /* Exercises handle_setpgid: the kernel validates, Detcore mirrors. */
    int pg = setpgid(0, 0);
    printf("setpgid rc=%d errno=%d then pgid=%d\n", pg, pg == 0 ? 0 : errno,
           getpgid(0));

    /*
     * Exercises handle_setsid. This is EXPECTED TO FAIL with EPERM: the
     * setpgid above makes this process a group leader, and Linux refuses
     * setsid from a group leader. The refusal is the pinned observation --
     * a success here would mean the group-leader precondition changed.
     */
    int sd = setsid();
    printf("setsid rc=%d errno=%d then sid=%d pgid=%d\n", sd < 0 ? -1 : 0,
           sd < 0 ? errno : 0, getsid(0), getpgid(0));

    return 0;
}
