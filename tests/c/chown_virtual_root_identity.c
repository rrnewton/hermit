/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * Regression guest for the determinized `chown` family (#1849).
 *
 * Detcore fixes the guest identity at root, so an ownership change must report
 * the answer a real root gets -- success, for any uid -- instead of the errno
 * of whatever host identity the backend happens to run under. But root
 * privilege waives only the ownership PERMISSION check. It does not waive
 * pathname, descriptor, or flag errors: a real root's
 * `chown("/does/not/exist", 0, 0)` still fails with ENOENT.
 *
 * This guest asserts BOTH halves, because each one alone is satisfied by a
 * defect:
 *
 *   - an unconditional `Ok(0)` satisfies the permission half and turns every
 *     error case into a silent success;
 *   - a plain pass-through satisfies the error half and reintroduces the
 *     host-dependent EPERM/EINVAL the determinization exists to remove.
 *
 * Only an implementation that emulates the identity half while forwarding the
 * argument half passes all of the checks below.
 *
 * Success is printing "chown-virtual-root-identity-ok" and exiting 0. Every
 * failure prints the exact call, what was expected, and what was observed.
 */

#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/types.h>
#include <unistd.h>

/* A uid that is deliberately NOT the caller and NOT root: under a one-uid user
 * namespace this is the value that used to fail with EINVAL, and with no user
 * namespace at all the whole family used to fail with EPERM. */
#define FOREIGN_UID 1000
#define FOREIGN_GID 1000

static int failures = 0;

static const char* errno_name(int err) {
  const char* name = strerrorname_np(err);
  return name ? name : "?";
}

/* The identity half: a virtual root's ownership change must SUCCEED. */
static void expect_ok(const char* what, int rc) {
  if (rc == 0) {
    printf("ok       %-42s rc=0\n", what);
    return;
  }
  printf("FAIL     %-42s expected rc=0, got rc=%d errno=%s\n", what, rc, errno_name(errno));
  failures++;
}

/* The argument half: an error that has nothing to do with identity must still
 * reach the guest, with the errno Linux would have produced. */
static void expect_errno(const char* what, int rc, int wanted) {
  if (rc == 0) {
    printf("FAIL     %-42s expected %s, got rc=0 (error swallowed)\n", what, errno_name(wanted));
    failures++;
    return;
  }
  if (errno != wanted) {
    printf("FAIL     %-42s expected %s, got %s\n", what, errno_name(wanted), errno_name(errno));
    failures++;
    return;
  }
  printf("ok       %-42s rc=-1 errno=%s\n", what, errno_name(wanted));
}

int main(void) {
  const char* path = "chown_virtual_root_target";
  const char* link_path = "chown_virtual_root_symlink";
  const char* missing = "chown_virtual_root_no_such_file";

  unlink(path);
  unlink(link_path);

  int fd = open(path, O_CREAT | O_WRONLY | O_TRUNC, 0644);
  if (fd < 0) {
    perror("open");
    return 1;
  }
  if (symlink(path, link_path) != 0) {
    perror("symlink");
    return 1;
  }

  /* ---- Identity half: these all failed before #1849. ---- */
  expect_ok("chown(file, FOREIGN)", chown(path, FOREIGN_UID, FOREIGN_GID));
  expect_ok("chown(file, 0, 0)", chown(path, 0, 0));
  expect_ok("fchown(fd, FOREIGN)", fchown(fd, FOREIGN_UID, FOREIGN_GID));
  expect_ok("lchown(symlink, FOREIGN)", lchown(link_path, FOREIGN_UID, FOREIGN_GID));
  expect_ok("fchownat(AT_FDCWD, file, FOREIGN)",
            (int)syscall(SYS_fchownat, AT_FDCWD, path, FOREIGN_UID, FOREIGN_GID, 0));
  /* -1 means "leave unchanged"; a real root gets 0 and so must the guest. */
  expect_ok("chown(file, -1, -1)", chown(path, (uid_t)-1, (gid_t)-1));

  /* ---- Argument half: none of these is an identity error, so a real root
   * still receives every one of them. An unconditional success swallows the
   * lot -- that is the regression this half exists to catch. ---- */
  errno = 0;
  expect_errno("chown(MISSING)", chown(missing, 0, 0), ENOENT);
  errno = 0;
  expect_errno("lchown(MISSING)", lchown(missing, 0, 0), ENOENT);
  errno = 0;
  expect_errno("fchownat(AT_FDCWD, MISSING)",
               (int)syscall(SYS_fchownat, AT_FDCWD, missing, 0, 0, 0), ENOENT);
  errno = 0;
  expect_errno("fchown(-1)", fchown(-1, 0, 0), EBADF);
  errno = 0;
  expect_errno("fchown(9999)", fchown(9999, 0, 0), EBADF);
  errno = 0;
  /* A regular file used as a directory component. */
  expect_errno("chown(file/child)", chown("chown_virtual_root_target/child", 0, 0), ENOTDIR);
  errno = 0;
  /* An unrecognised AT_* flag is rejected before anything else happens. */
  expect_errno("fchownat(bad flags)",
               (int)syscall(SYS_fchownat, AT_FDCWD, path, 0, 0, 0x4000), EINVAL);

  /* ---- Host ownership must be untouched: this is a no-op, not a forwarded
   * chown. Detcore does not model per-file ownership, so the read-back shows
   * the unchanged owner even though the calls above reported success. That
   * divergence is the documented boundary; assert it so a future change that
   * starts really mutating ownership cannot pass silently. ---- */
  struct stat st;
  if (stat(path, &st) != 0) {
    perror("stat");
    return 1;
  }
  if (st.st_uid == FOREIGN_UID && st.st_gid == FOREIGN_GID) {
    printf("FAIL     %-42s host ownership was actually changed to %u:%u\n", "stat(file) read-back",
           (unsigned)st.st_uid, (unsigned)st.st_gid);
    failures++;
  } else {
    printf("ok       %-42s owner unchanged (%u:%u), as documented\n", "stat(file) read-back",
           (unsigned)st.st_uid, (unsigned)st.st_gid);
  }

  close(fd);
  unlink(link_path);
  unlink(path);

  if (failures != 0) {
    printf("chown-virtual-root-identity-FAILED %d\n", failures);
    return 1;
  }
  printf("chown-virtual-root-identity-ok\n");
  return 0;
}
