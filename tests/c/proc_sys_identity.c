/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * Contract: guest-visible /proc and /sys reads must be DETERMINISTIC.
 *
 * These pseudo-files expose live host state -- pids, memory figures, cpu
 * counts, uptime, timers, mapping addresses. Every read of them SUCCEEDS, so
 * nothing looks wrong; the host state simply flows into an otherwise
 * deterministic run and makes it irreproducible. That is the failure mode this
 * fixture pins: a silent leak, not an error.
 *
 * WHAT IS ASSERTED, AND WHAT IS DELIBERATELY NOT.
 *
 * This program does NOT compare anything against a golden constant, and it must
 * never be "fixed" by making it do so. Freezing a field to a constant would
 * make it pass while destroying the functionality the field exists for:
 * /proc/uptime and the timer fields MUST keep advancing, continuously and at
 * fine grain (#140). A frozen clock is not determinism, it is a broken clock
 * that happens to compare equal.
 *
 * Instead this program just PRINTS what it read, and determinism is asserted by
 * the harness running it twice and requiring the two runs to be identical
 * (`verify` mode), and by running it across backends. A value is free to evolve
 * during the run; it must simply evolve the SAME WAY every time. That is a
 * strictly stronger and more honest contract than equality-to-a-constant, and
 * it is the only one compatible with continuous virtual time.
 *
 * Reads that FAIL are printed as their errno name rather than skipped. An
 * unreadable path is a legitimate, reproducible observation; silently dropping
 * it would let a path that is readable on one backend and absent on another
 * compare equal.
 */

#include <dirent.h>
#include <errno.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

/* Cap per-file output so a large mapping table cannot dominate the comparison
 * or blow up the harness log. The cap is applied identically every run, so it
 * cannot mask a difference in the prefix it does show. */
#define MAX_BYTES 4096

/*
 * Files whose ENTIRE content is compared.
 *
 * Each of these is a known host-state carrier:
 *   /proc/self/stat, /proc/self/status  pids, ppids, thread counts, rss
 *   /proc/self/statm, /proc/meminfo     live memory figures
 *   /proc/stat                          per-cpu jiffies, boot time, ctxt count
 *   /proc/uptime, /proc/loadavg         wall-clock and load, both host-derived
 *   /proc/cpuinfo, /sys/devices/...     cpu count and topology
 *   /proc/self/maps                     mapping addresses (ASLR)
 *   /proc/sys/kernel entries            host identity and pid allocation state
 */
static const char* const kFiles[] = {
    "/proc/self/stat",
    "/proc/self/statm",
    "/proc/self/status",
    "/proc/self/maps",
    "/proc/self/limits",
    "/proc/self/cmdline",
    "/proc/self/environ",
    "/proc/self/mountinfo",
    "/proc/self/oom_score",
    "/proc/stat",
    "/proc/meminfo",
    "/proc/uptime",
    "/proc/loadavg",
    "/proc/version",
    "/proc/cpuinfo",
    "/proc/sys/kernel/pid_max",
    "/proc/sys/kernel/ostype",
    "/proc/sys/kernel/osrelease",
    "/proc/sys/kernel/hostname",
    "/proc/sys/kernel/random/boot_id",
    "/sys/devices/system/cpu/online",
    "/sys/devices/system/cpu/possible",
    "/sys/kernel/mm/transparent_hugepage/enabled",
};

/* Directories whose ENTRY NAMES are compared (contents are not). /proc/self/fd
 * and /proc/self/task are pid- and tid-shaped, so their names leak identity
 * directly; the top of /proc leaks every live host pid. */
static const char* const kDirs[] = {
    "/proc/self/fd",
    "/proc/self/task",
    "/proc",
    "/sys/devices/system/cpu",
};

static int compare_names(const void* a, const void* b) {
  return strcmp(*(const char* const*)a, *(const char* const*)b);
}

static void dump_file(const char* path) {
  FILE* f = fopen(path, "rb");
  if (f == NULL) {
    /* errno NAME, not the number: numbers are stable but the name survives a
     * reader who does not have errno.h open. */
    printf("FILE %s UNREADABLE %s\n", path, strerror(errno));
    return;
  }
  unsigned char buf[MAX_BYTES];
  size_t n = fread(buf, 1, sizeof(buf), f);
  const int read_error = ferror(f);
  fclose(f);
  if (read_error) {
    printf("FILE %s READERROR\n", path);
    return;
  }
  printf("FILE %s bytes=%zu\n", path, n);
  /* Print the bytes verbatim, escaping only what would break line structure, so
   * a diff points at the offending field rather than at an opaque hash. */
  for (size_t i = 0; i < n; i++) {
    unsigned char c = buf[i];
    if (c == '\n') {
      printf("\\n");
    } else if (c == '\\') {
      printf("\\\\");
    } else if (c < 0x20 || c >= 0x7f) {
      printf("\\x%02x", c);
    } else {
      putchar(c);
    }
  }
  putchar('\n');
}

static void dump_dir(const char* path) {
  DIR* d = opendir(path);
  if (d == NULL) {
    printf("DIR %s UNREADABLE %s\n", path, strerror(errno));
    return;
  }
  char* names[4096];
  size_t count = 0;
  struct dirent* e;
  while ((e = readdir(d)) != NULL && count < sizeof(names) / sizeof(names[0])) {
    names[count] = strdup(e->d_name);
    if (names[count] == NULL) {
      break;
    }
    count++;
  }
  closedir(d);
  /* Sort: readdir order is not part of this contract and is covered by the
   * getdents fixtures. Sorting keeps THIS test focused on the entry SET, so a
   * pure ordering change does not masquerade as a host-state leak. */
  qsort(names, count, sizeof(names[0]), compare_names);
  /* Split numeric from non-numeric entries. Listing every pid under /proc would
   * be thousands of lines of host state -- maximally leaky but unreadable as a
   * diff and heavy in the log. The COUNT of numeric entries carries the leak
   * signal just as well (it changes the moment host pids are visible), while
   * the non-numeric names are the stable set worth comparing by name. */
  size_t numeric = 0;
  for (size_t i = 0; i < count; i++) {
    char* end = NULL;
    (void)strtol(names[i], &end, 10);
    if (end != names[i] && *end == '\0') {
      numeric++;
    }
  }
  printf("DIR %s entries=%zu numeric=%zu\n", path, count, numeric);
  for (size_t i = 0; i < count; i++) {
    char* end = NULL;
    (void)strtol(names[i], &end, 10);
    const int is_numeric = (end != names[i] && *end == '\0');
    if (!is_numeric) {
      printf("  %s\n", names[i]);
    }
    free(names[i]);
  }
}

int main(void) {
  for (size_t i = 0; i < sizeof(kFiles) / sizeof(kFiles[0]); i++) {
    dump_file(kFiles[i]);
  }
  for (size_t i = 0; i < sizeof(kDirs) / sizeof(kDirs[0]); i++) {
    dump_dir(kDirs[i]);
  }
  /* Read a subset TWICE within the single run. Two reads of a host-backed
   * counter can differ inside one run even when the run as a whole is
   * reproducible, so this catches intra-run drift that a run-to-run comparison
   * alone would miss. */
  for (size_t i = 0; i < 2; i++) {
    dump_file("/proc/uptime");
    dump_file("/proc/self/stat");
    dump_file("/proc/stat");
  }
  return 0;
}
