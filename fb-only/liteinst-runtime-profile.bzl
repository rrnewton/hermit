# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This source code is dual-licensed under either the MIT license found in the
# LICENSE-MIT file in the root directory of this source tree or the Apache
# License, Version 2.0 found in the LICENSE-APACHE file in the root directory
# of this source tree. You may select, at your option, one of the
# above-listed licenses.

# The internal liteinst-runtime profile, which shared BUCK files load as
# "@fbcode//hermetic_infra/hermit/fb-only:liteinst-runtime-profile.bzl". The
# open-source build loads its stand-in at the same label
# (shim/hermetic_infra/hermit/fb-only/liteinst-runtime-profile.bzl). Keep the
# exported names and the flag list in step with that file.

load("@fbcode_macros//build_defs:fbcode_configured_alias.bzl", "fbcode_configured_alias")

# The reviewed rustc flags of Cargo's liteinst-runtime profile (the root
# Cargo.toml), for the Detcore runtime's whole transitive closure: the release
# flags with debug assertions and overflow checks on. Cargo's lto = false
# passes no -Clto flag and leaves rustc's thin local LTO in effect, so none
# appears here: -Clto=off would be lto = "off", different code generation.
# stage-detcore-liteinst and ci/verify-packaged-detcore.py pin this exact
# list in the receipt each produces or reads.
LITEINST_RUNTIME_RUSTC_FLAGS = [
    "-Copt-level=3",
    "-Cdebuginfo=0",
    "-Cdebug-assertions=yes",
    "-Coverflow-checks=yes",
    "-Ccodegen-units=16",
    "-Cembed-bitcode=no",
    "-Cpanic=unwind",
    "-Cstrip=none",
]

def detcore_liteinst_runtime_alias(name, actual):
    """`actual` configured under the liteinst-runtime profile, as `name`.

    fbcode pins the x86_64 Linux platform plus the opt build mode. That
    alone does not obviously pin assertions, overflow checks, codegen units
    or strip over a Rust closure, nor an unstripped [cdylib] with .symtab:
    whether the flags above reach every crate of the closure is unproven
    until a real fbsource-built ELF is checked against them (D1c-6), and no
    consumer may read this alias as that proof."""
    fbcode_configured_alias(
        name = name,
        actual = actual,
        additional_constraints = ["ovr_config//build_mode/constraints:opt"],
        platform = "ovr_config//platform/linux:x86_64-fbcode",
        visibility = ["PUBLIC"],
    )
