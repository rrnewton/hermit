/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * A shared library whose constructor changes the in-guest LiteInst runtime's
 * settings before the runtime's own constructor runs: the program links it,
 * and the dynamic loader initializes a program's libraries before the
 * preloaded runtime. IN_GUEST_TRAP_FIXTURE_CONSTRUCTOR selects what it does:
 *
 * - `set`: sets REVERIE_LITEINST_SITE_PATCHING=1;
 * - `unset`: removes REVERIE_LITEINST_SITE_PATCHING;
 * - `set-then-restore`: sets it to 1, and the library's `mprotect`, which
 *   interposes on the C library's, sets it back to 0 at the first call that
 *   changes the vDSO's protection: the runtime makes that call while it
 *   rewrites the vDSO, after it has read 1 and before it constructs its Tool;
 * - `no-stats`: removes REVERIE_LITEINST_STATS_COORDINATOR, so the runtime
 *   collects no statistics;
 * - anything else: changes nothing.
 */

#define _GNU_SOURCE
#include <stdlib.h>
#include <string.h>
#include <sys/auxv.h>
#include <sys/syscall.h>
#include <unistd.h>

static const char* mode = "none";
static int restore_pending = 0;
static int restored = 0;

__attribute__((constructor)) static void change_settings(void) {
  const char* requested = getenv("IN_GUEST_TRAP_FIXTURE_CONSTRUCTOR");
  if (requested == NULL) {
    return;
  }
  if (strcmp(requested, "set") == 0) {
    setenv("REVERIE_LITEINST_SITE_PATCHING", "1", 1);
    mode = "set";
  } else if (strcmp(requested, "unset") == 0) {
    unsetenv("REVERIE_LITEINST_SITE_PATCHING");
    mode = "unset";
  } else if (strcmp(requested, "set-then-restore") == 0) {
    setenv("REVERIE_LITEINST_SITE_PATCHING", "1", 1);
    restore_pending = 1;
    mode = "set-then-restore";
  } else if (strcmp(requested, "no-stats") == 0) {
    unsetenv("REVERIE_LITEINST_STATS_COORDINATOR");
    mode = "no-stats";
  }
}

/* The vDSO is a few pages; the runtime changes the protection of its text. */
static int in_vdso(const void* address) {
  unsigned long vdso = getauxval(AT_SYSINFO_EHDR);
  unsigned long at = (unsigned long)address;
  return vdso != 0 && at >= vdso && at < vdso + 0x10000;
}

int mprotect(void* address, size_t length, int protection) {
  if (restore_pending && in_vdso(address)) {
    restore_pending = 0;
    setenv("REVERIE_LITEINST_SITE_PATCHING", "0", 1);
    restored = 1;
  }
  return (int)syscall(SYS_mprotect, address, length, protection);
}

const char* in_guest_trap_fixture_mode(void) {
  return mode;
}

int in_guest_trap_fixture_restored(void) {
  return restored;
}
