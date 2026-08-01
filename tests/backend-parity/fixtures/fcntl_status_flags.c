/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * fcntl(2) F_GETFL/F_SETFL file-status-flag round-trip parity probe.
 *
 * A single process opens one temporary file and exercises the deterministic
 * file-status-flag semantics Detcore's file model must preserve identically on
 * every backend. F_SETFL may change only the settable status flags (here
 * O_NONBLOCK and O_APPEND); it must never alter the access mode or creation
 * flags, and F_GETFL must report exactly the flags in effect:
 *
 *   - F_GETFL on an O_RDWR descriptor reports access mode O_RDWR, O_NONBLOCK
 *     clear.
 *   - F_SETFL O_NONBLOCK sets O_NONBLOCK while preserving the O_RDWR access
 *     mode.
 *   - F_SETFL clearing O_NONBLOCK removes it again.
 *   - F_SETFL O_APPEND makes subsequent writes land at end-of-file regardless
 *     of the current offset.
 *
 * The sequence over an initial file containing "ab" is:
 *   F_GETFL                 -> access O_RDWR, O_NONBLOCK clear
 *   F_SETFL |O_NONBLOCK     -> O_NONBLOCK set, access still O_RDWR
 *   F_SETFL &~O_NONBLOCK    -> O_NONBLOCK clear
 *   F_SETFL O_APPEND; write -> "cd" appended -> "abcd"
 *   reopen/read             -> content "abcd", size 4
 *
 * The four bytes "abcd" checksum to 'a'+'b'+'c'+'d' = 394 and the final size is
 * 4. Only invariants are printed:
 *
 *   fcntl_status_flags size=4 checksum=394 ok=6
 *
 * It is deliberately free of gated concerns: single process, no fork/thread, and
 * no pid, timestamp, cpu-time, or address is observed.
 */

#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <unistd.h>

static void fail(const char *message) {
  fprintf(stderr, "%s: %s\n", message, strerror(errno));
  exit(1);
}

static off_t fd_size(int fd) {
  struct stat st;
  if (fstat(fd, &st) != 0)
    fail("fstat");
  return st.st_size;
}

int main(void) {
  char template[] = "/tmp/fcntl_status_flags_XXXXXX";
  int fd = mkstemp(template);
  if (fd < 0)
    fail("mkstemp");
  if (write(fd, "ab", 2) != 2)
    fail("write ab");

  int ok = 0;

  /* F_GETFL reports the access mode and no O_NONBLOCK. */
  int flags = fcntl(fd, F_GETFL);
  if (flags < 0)
    fail("F_GETFL");
  if ((flags & O_ACCMODE) == O_RDWR)
    ok++;
  if ((flags & O_NONBLOCK) == 0)
    ok++;

  /* F_SETFL sets O_NONBLOCK while preserving the access mode. */
  if (fcntl(fd, F_SETFL, flags | O_NONBLOCK) != 0)
    fail("F_SETFL O_NONBLOCK");
  flags = fcntl(fd, F_GETFL);
  if (flags < 0)
    fail("F_GETFL nonblock");
  if ((flags & O_NONBLOCK) != 0 && (flags & O_ACCMODE) == O_RDWR)
    ok++;

  /* F_SETFL clearing O_NONBLOCK removes it again. */
  if (fcntl(fd, F_SETFL, flags & ~O_NONBLOCK) != 0)
    fail("F_SETFL clear");
  flags = fcntl(fd, F_GETFL);
  if (flags < 0)
    fail("F_GETFL clear");
  if ((flags & O_NONBLOCK) == 0)
    ok++;

  /* F_SETFL O_APPEND forces subsequent writes to end-of-file. */
  if (fcntl(fd, F_SETFL, O_APPEND) != 0)
    fail("F_SETFL O_APPEND");
  flags = fcntl(fd, F_GETFL);
  if (flags < 0)
    fail("F_GETFL append");
  if ((flags & O_APPEND) != 0)
    ok++;
  if (lseek(fd, 0, SEEK_SET) != 0)
    fail("lseek");
  if (write(fd, "cd", 2) != 2)
    fail("write cd");
  if (close(fd) != 0)
    fail("close");

  /* Reopen read-only and confirm the appended content and size. */
  int fin = open(template, O_RDONLY);
  if (fin < 0)
    fail("open final");
  char buf[8];
  off_t final_size = fd_size(fin);
  ssize_t n = pread(fin, buf, sizeof(buf), 0);
  if (n == 4 && memcmp(buf, "abcd", 4) == 0 && final_size == 4)
    ok++;
  long checksum = 0;
  for (ssize_t i = 0; i < n; i++)
    checksum += (unsigned char)buf[i];
  if (close(fin) != 0)
    fail("close final");

  if (unlink(template) != 0)
    fail("unlink");

  printf("fcntl_status_flags size=%ld checksum=%ld ok=%d\n", (long)final_size,
         checksum, ok);
  return 0;
}
