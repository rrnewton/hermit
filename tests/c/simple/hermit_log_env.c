// Copyright (c) Meta Platforms, Inc. and affiliates.
//
// This source code is licensed under the BSD-style license found in the
// LICENSE file in the root directory of this source tree.

#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

int main(int argc, char** argv) {
  if (argc == 2 && strcmp(argv[1], "exec") == 0) {
    char* exec_argv[] = {argv[0], NULL};
    execv(argv[0], exec_argv);
    perror("execv");
    return 2;
  }

  const char* value = getenv("HERMIT_LOG");
  printf("hermit_log=%s\n", value == NULL ? "<unset>" : value);
  return 0;
}
