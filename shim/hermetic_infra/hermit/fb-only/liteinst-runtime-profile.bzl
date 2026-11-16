# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This source code is dual-licensed under either the MIT license found in the
# LICENSE-MIT file in the root directory of this source tree or the Apache
# License, Version 2.0 found in the LICENSE-APACHE file in the root directory
# of this source tree. You may select, at your option, one of the
# above-listed licenses.

# Open-source stand-in for the internal profile at this path, which shared
# BUCK files may load as
# "@fbcode//hermetic_infra/hermit/fb-only:liteinst-runtime-profile.bzl"
# (the fbcode cell is this shim here).
#
# The canonical home of the two liteinst-runtime select names is the
# constraints stand-in in this same cell; this file loads and re-exports
# them for any loader that still names the profile, so the values have
# exactly one definition in this tree. Nothing passes these flags through
# the toolchain in this tree: under the shim liteinst-runtime platform
# the target-level select on these names is the only delivery path, and
# outside it the select defaults to no added flags. On this tree (no
# liteinst-runtime profile file before this change) the file carries only
# these exports; the internal file's remaining profile contents arrive
# with the LiteInst stack they belong to.

load(
    "//hermetic_infra/hermit/constraints:liteinst-runtime-flags.bzl",
    _FLAGS_CONSTRAINT = "LITEINST_RUNTIME_FLAGS_CONSTRAINT",
    _SELECT_RUSTC_FLAGS = "LITEINST_RUNTIME_SELECT_RUSTC_FLAGS",
)

LITEINST_RUNTIME_FLAGS_CONSTRAINT = _FLAGS_CONSTRAINT
LITEINST_RUNTIME_SELECT_RUSTC_FLAGS = _SELECT_RUSTC_FLAGS
