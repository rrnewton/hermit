#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.
#
# Defends the frozen /etc/group control in hermit-cli/src/bin/hermit/container.rs.
#
# WHAT THE CONTROL DOES. `frozen_group_file()` reads the host /etc/group and, if
# no group carries the kernel overflow GID (65534), APPENDS `nobody:x:65534:`
# before bind-mounting the result read-only over /etc/group. Inside the guest's
# user namespace every unmapped group id collapses to that overflow GID, so
# whether it resolves to a NAME or prints as a NUMBER is decided entirely by
# whether that entry exists.
#
# WHY IT MATTERS BEYOND THIS CELL. Without the entry, `ls -l`, `id` and
# `stat -c %G` print `65534` instead of `nobody` -- on every host whose
# /etc/group lacks the group. That silently changes the stdout of any other cell
# that lists a file, so the blast radius is much wider than the control's size.
#
# ⚠️ FAILABILITY IS ESTABLISHED BY MECHANISM, NOT BY AN OBSERVED RED. There is no
# flag to drop the group mount, so NOBODY HAS WATCHED THIS CELL FAIL. What is
# measured, on devbig014 at the time of writing: kernel overflowgid is 65534,
# the host /etc/group contains no entry for it, and getgrgid(65534) raises
# KeyError on the host -- so the name below can only come from the synthesised
# entry. That is an argument plus a host measurement, not a negative control.
# Treat it as an honest cell with a stated limit; if a way to disable the mount
# appears, run this with it off and record the red.
set -euo pipefail

# One line: resolve the group of a file the guest did not create, which in the
# user namespace is owned by the unmapped-and-therefore-overflow GID.
printf 'OVERFLOW_GID_GROUP=%s\n' "$(stat -c '%G' /etc/hostname)"
