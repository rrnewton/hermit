/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

#define _GNU_SOURCE
#include <stdio.h>
#include <stdlib.h>
#include <sys/types.h>
#include <unistd.h>

int main(void) {
  if (getuid() != geteuid() || getgid() != getegid()) {
    fputs("real/effective credential mismatch\n", stderr);
    return 1;
  }

  int group_count = getgroups(0, NULL);
  if (group_count < 0) {
    perror("getgroups count");
    return 1;
  }
  gid_t *groups =
      calloc((size_t)(group_count > 0 ? group_count : 1), sizeof(*groups));
  if (groups == NULL || getgroups(group_count, groups) != group_count) {
    perror("getgroups list");
    free(groups);
    return 1;
  }
  free(groups);

  pid_t process_group = getpgid(0);
  pid_t process_group_alias = getpgrp();
  if (process_group < 0 || process_group != process_group_alias) {
    fprintf(stderr, "getpgid/getpgrp mismatch: %ld/%ld\n", (long)process_group,
            (long)process_group_alias);
    return 1;
  }
  if (getsid(0) < 0) {
    perror("getsid");
    return 1;
  }

  printf("pid=%ld\n", (long)getpid());
  return 0;
}
