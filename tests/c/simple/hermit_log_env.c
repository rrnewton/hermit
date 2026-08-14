// Copyright (c) Meta Platforms, Inc. and affiliates.
//
// This source code is licensed under the BSD-style license found in the
// LICENSE file in the root directory of this source tree.

#include <stdio.h>
#include <stdlib.h>

int main(void) {
  const char* value = getenv("HERMIT_LOG");
  printf("hermit_log=%s\n", value == NULL ? "<unset>" : value);
  return 0;
}
