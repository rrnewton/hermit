load("@fbcode//hermetic_infra/hermit/fb-only/contract_checks:backend_abstraction.bzl", "MEMBER_MANIFESTS")
load("@fbcode//hermetic_infra/hermit/fb-only:liteinst-runtime-profile.bzl", "detcore_liteinst_runtime_alias")
load("@fbcode//hermetic_infra/hermit/fb-only:release.bzl", "HERMIT_REVERIE_PIN")
load("@fbcode//hermetic_infra/hermit/hermit-cli:integration_tests_data.bzl", "EXTERNAL_SRCS")
load("@fbcode//hermetic_infra/hermit_import:excluded_paths.bzl", "excluded_paths_test")
load("@fbcode_macros//build_defs:export_files.bzl", "buck_export_file", "export_file")
load("@fbcode_macros//build_defs:native_rules.bzl", "buck_filegroup", "buck_genrule")
load("@fbsource//tools/build_defs:rust_binary.bzl", "rust_binary")
load("@fbsource//tools/build_defs:rust_library.bzl", "rust_library")

oncall("hermit")

# Request :hermit-inguest-engine[cdylib] explicitly. The default is a check
# output; [shared]/[dylib] is not this engine's C ABI artifact.
rust_library(
    name = "hermit-inguest-engine",
    srcs = [
        "inguest-engine/build.rs",
        "inguest-engine/src/install.rs",
        "inguest-engine/src/lib.rs",
        "inguest-engine/src/loader_settings.rs",
        "inguest-engine/src/loader_state.rs",
        "inguest-engine/src/native/abi.rs",
        "inguest-engine/src/native/auxv.rs",
        "inguest-engine/src/native/bootstrap.rs",
        "inguest-engine/src/native/config.rs",
        "inguest-engine/src/native/copied.rs",
        "inguest-engine/src/native/copied_action.rs",
        "inguest-engine/src/native/copied_policy.rs",
        "inguest-engine/src/native/diagnostics.rs",
        "inguest-engine/src/native/entry.rs",
        "inguest-engine/src/native/first_instruction_observer.rs",
        "inguest-engine/src/native/image.rs",
        "inguest-engine/src/native/lifecycle.rs",
        "inguest-engine/src/native/log_record.rs",
        "inguest-engine/src/native/mod.rs",
        "inguest-engine/src/native/owner.rs",
        "inguest-engine/src/native/policy.rs",
        "inguest-engine/src/native/prelude.rs",
        "inguest-engine/src/observation.rs",
        "inguest-engine/src/runtime.rs",
    ],
    # Its Cargo manifest is the open-source one (inguest-engine/Cargo.toml);
    # fbsource builds this engine with Buck only.
    autocargo = {"ignore_rule": True},
    crate = "hermit_inguest_engine",
    crate_root = "inguest-engine/src/lib.rs",
    doctests = False,
    env = {"NIX_DONT_SET_RPATH_x86_64_unknown_linux_gnu": "1"},
    features = [],
    linker_flags = ["-Wl,-u,_Unwind_RaiseException"],
    # glibc_compat.rs lives in the //common package, so it cannot be a srcs
    # path here. Map it to the same virtual path the #[path] module in
    # inguest-engine/src/lib.rs resolves (../../common/inguest/...), so both
    # builders compile identical sources.
    mapped_srcs = {
        "//hermetic_infra/hermit/common:glibc_compat.rs": "common/inguest/glibc_compat.rs",
    },
    preferred_linkage = "any",
    # This engine supports GNU Linux x86-64 only. Keep the Cargo build.rs
    # static unwinder / no build-root RUNPATH policy in the native output.
    rustc_flags = ["-lstatic:-bundle=gcc_eh"],
    unittests = False,
    deps = [
        "fbsource//third-party/rust:libc",
        "fbsource//third-party/rust:rand",
        "fbsource//third-party/rust:tracing",
        "//hermetic_infra/hermit/detcore:detcore",
        "//hermetic_infra/reverie:reverie",
        # The reverie-inguest feature variant that reverie-liteinst links, so
        # the engine and reverie-liteinst share one reverie_inguest crate.
        "//hermetic_infra/reverie:reverie-inguest-liteinst",  # @manual name:reverie_inguest
        "//hermetic_infra/reverie:reverie-liteinst",
        "//hermetic_infra/reverie:reverie-rpc-transport",
    ],
)

# Landing order: the LiteInst targets in this package and in hermit-cli
# depend on Reverie's reverie-liteinst-sitepatch / reverie-liteinst-preload
# targets (D1a). In fbsource they are direct fbcode deps; on the GitHub
# side they require D1a landed in Reverie plus the export pin flow that
# advances this tree's Reverie pin. Do not read their presence here as that
# flow having run.
#
# Request :detcore-liteinst[cdylib] explicitly. The default is a check
# output; [shared]/[dylib] is not this runtime's C ABI artifact.
rust_library(
    name = "detcore-liteinst",
    srcs = glob(["detcore-liteinst/src/**/*.rs"]) + ["detcore-liteinst/build.rs"],
    # Its Cargo manifest is the open-source one (detcore-liteinst/Cargo.toml);
    # fbsource builds this runtime with Buck only.
    autocargo = {"ignore_rule": True},
    crate = "detcore_liteinst",
    crate_root = "detcore-liteinst/src/lib.rs",
    doctests = False,
    env = {"NIX_DONT_SET_RPATH_x86_64_unknown_linux_gnu": "1"},
    features = [],
    linker_flags = ["-Wl,-u,_Unwind_RaiseException"],
    # glibc_compat.rs lives in the //common package, so it cannot be a srcs
    # path here. Map it to the same virtual path the #[path] module in
    # detcore-liteinst/src/lib.rs resolves (../../common/inguest/...), so both
    # builders compile identical sources.
    mapped_srcs = {
        "//hermetic_infra/hermit/common:glibc_compat.rs": "common/inguest/glibc_compat.rs",
    },
    preferred_linkage = "any",
    # This runtime supports GNU Linux x86-64 only. Keep the Cargo build.rs
    # static unwinder / no build-root RUNPATH policy in the native output.
    rustc_flags = ["-lstatic:-bundle=gcc_eh"],
    target_compatible_with = [
        "ovr_config//cpu:x86_64",
        "ovr_config//os:linux",
    ],
    unittests = False,
    deps = [
        "fbsource//third-party/rust:libc",
        "//hermetic_infra/hermit/detcore:detcore",
        "//hermetic_infra/reverie:reverie",
        # The reverie-inguest feature variant that reverie-liteinst links, so
        # the runtime and reverie-liteinst share one reverie_inguest crate.
        "//hermetic_infra/reverie:reverie-inguest-liteinst",  # @manual name:reverie_inguest
        "//hermetic_infra/reverie:reverie-liteinst-sitepatch",  # @manual name:reverie_liteinst
        # No reverie-rpc-transport here: in Cargo it is optional, selected
        # only by the exec-private-init feature, and this target builds the
        # ordinary runtime with features = [].
    ],
)

# :detcore-liteinst under the liteinst-runtime profile (fb-only/
# liteinst-runtime-profile.bzl), for the staging genrule below. Request
# :detcore-liteinst-liteinst-runtime[cdylib] explicitly, as with
# :detcore-liteinst. The open-source build pins the shim liteinst-runtime
# platform, which applies the profile's exact flags to the whole closure;
# fbcode pins its x86_64 Linux platform plus opt, whose flag propagation
# over the closure is unproven until a real fbsource-built ELF is checked
# (see the profile file). No default target references this alias.
detcore_liteinst_runtime_alias(
    name = "detcore-liteinst-liteinst-runtime",
    actual = ":detcore-liteinst",
)

# The Cargo finalizer that qualifies the ordinary Detcore preload and
# writes its hermit-packaged-detcore/v1 receipt, as a Buck binary so the
# staging genrule below can run the same logic over the Buck-built
# runtime. hermit-install has no BUCK package of its own; like
# :detcore-liteinst, this target lives in the root package under the
# autocargo ignore_rule pattern (its Cargo manifest stays the
# open-source one). The binary derives the Hermit root from
# CARGO_MANIFEST_DIR exactly as under Cargo and resolves its source
# authority from that tree's manifests -- authority the fbsource import
# does not carry (the import drops every Cargo.toml), so there it
# refuses by name until such an authority exists in its environment.
rust_binary(
    name = "stage-detcore-liteinst",
    srcs = ["hermit-install/src/bin/stage-detcore-liteinst.rs"],
    autocargo = {"ignore_rule": True},
    crate_root = "hermit-install/src/bin/stage-detcore-liteinst.rs",
    # The binary include!s the pin reader by its repository-relative path
    # (../../../hermit-cli/reverie_pin.rs) -- map it to that same virtual
    # path, as :detcore-liteinst maps glibc_compat.rs.
    mapped_srcs = {
        "//hermetic_infra/hermit/hermit-cli:reverie_pin.rs": "hermit-cli/reverie_pin.rs",
    },
    # It qualifies the x86-64 runtime; x86-64 only.
    target_compatible_with = ["ovr_config//cpu:x86_64"],
    deps = [
        "fbsource//third-party/rust:reverie-liteinst-artifact",
        "fbsource//third-party/rust:serde_json",
        "fbsource//third-party/rust:toml",
    ],
)

# The Buck staging of the Detcore runtime is an UNQUALIFIED layout: this
# genrule runs the unchanged Cargo finalizer (with --prebuilt) over the
# liteinst-runtime-configured :detcore-liteinst-liteinst-runtime[cdylib],
# and the finalizer stages those bytes and writes the receipt only if
# they are byte-identical to the qualified fixed liteinst-runtime build.
# Buck-profile bytes are not, so this target refuses by name ("prebuilt
# runtime differs from the qualified fixed liteinst-runtime build") until
# the Buck recipe produces the qualified bytes. No receipt is fabricated
# and core verification (ci/verify-packaged-detcore.py,
# scripts/build-buck-release.rs) is unchanged: without this genrule's
# output an installation is missing rsrcs/libdetcore_liteinst.package.json
# and is refused there.
buck_genrule(
    name = "detcore-liteinst-staged",
    out = "detcore-liteinst-staged",
    cmd = "mkdir -p $OUT/rsrcs $OUT/target/install_pkg/rsrcs" +
        " && $(exe :stage-detcore-liteinst) --source-copy $OUT/target --prebuilt $(location :detcore-liteinst-liteinst-runtime[cdylib])" +
        " && cp $OUT/target/install_pkg/rsrcs/libdetcore_liteinst.so $OUT/rsrcs/libdetcore_liteinst.so" +
        " && cp $OUT/target/install_pkg/rsrcs/libdetcore_liteinst.package.json $OUT/rsrcs/libdetcore_liteinst.package.json" +
        " && rm -rf $OUT/target",
    target_compatible_with = ["ovr_config//cpu:x86_64"],
    visibility = ["//hermetic_infra/hermit/hermit-cli:"],
)

# The receipt alone, for consumers that stage files one by one (the
# fbpkg's path_actions). It extracts :detcore-liteinst-staged's receipt,
# so it carries that genrule's by-name refusal of unqualified bytes.
buck_genrule(
    name = "detcore-liteinst-receipt",
    out = "libdetcore_liteinst.package.json",
    cmd = "cp $(location :detcore-liteinst-staged)/rsrcs/libdetcore_liteinst.package.json $OUT",
    target_compatible_with = ["ovr_config//cpu:x86_64"],
    visibility = ["//hermetic_infra/hermit/fb-only:"],
)

# The staged .so alone, likewise for the fbpkg's path_actions, so the
# package's runtime and receipt are the same staged pair instead of a
# staged receipt beside the raw :detcore-liteinst[cdylib] bytes.
buck_genrule(
    name = "detcore-liteinst-staged-so",
    out = "libdetcore_liteinst.so",
    cmd = "cp $(location :detcore-liteinst-staged)/rsrcs/libdetcore_liteinst.so $OUT",
    target_compatible_with = ["ovr_config//cpu:x86_64"],
    visibility = ["//hermetic_infra/hermit/fb-only:"],
)

# The LiteInst preload's installed revision record: the Reverie pin this
# tree builds. hermit-cli stages it beside libreverie_liteinst.so; it is
# generated here because hermit-cli/BUCK may bind the pin only into its
# build environment (scripts/check-reverie-pin.rs).
buck_genrule(
    name = "reverie-liteinst-revision",
    out = "libreverie_liteinst.so.revision",
    cmd = "echo " + HERMIT_REVERIE_PIN + " > $OUT",
    visibility = [
        "//hermetic_infra/hermit/fb-only:",
        "//hermetic_infra/hermit/hermit-cli:",
    ],
)

# Export the maintained header, never a second generated/copy-owned header.
export_file(
    name = "hermit-engine-v1-header",
    src = "inguest-engine/include/hermit-engine-v1.h",
)

# Native callers consume the source-owned scalar header from the same cohort.
export_file(
    name = "hermit-engine-native-v1-header",
    src = "inguest-engine/include/hermit-engine-native-v1.h",
)

# Fails if an upstream path that is never imported (see the list) is present.
excluded_paths_test()

# libhermit's unit tests include detcore-liteinst's numeric mapped-entry bounds
# by #[path] (hermit-cli/src/bin/hermit/mod.rs).
buck_export_file(
    name = "mapped-entry-bounds",
    src = "detcore-liteinst/src/exec_initial/mapped_entry/bounds.rs",
    visibility = ["//hermetic_infra/hermit/hermit-cli:"],
)

# Shared sources that hermit-cli's Cargo integration tests include by #[path].
[
    buck_export_file(
        name = src,
        visibility = [
            # The e2e test harness includes the ci/*.rs ones by #[path].
            "//hermetic_infra/hermit/ci/manifest-plan:",
            "//hermetic_infra/hermit/fb-only/integration_tests:",
            "//hermetic_infra/hermit/fb-only/integration_tests/tests:",
            "//hermetic_infra/hermit/hermit-cli:",
        ],
    )
    for src in EXTERNAL_SRCS
]

# The OSS validation DAG declares which integration binaries fbsource runs.
buck_export_file(
    name = "ci/dag/validate.json",
    visibility = ["//hermetic_infra/hermit/fb-only/integration_tests/tests:"],
)

# The per-test budgets and validation environment that the generator carries
# into the DAG nodes' wrappers.
[
    buck_export_file(
        name = src,
        visibility = ["//hermetic_infra/hermit/fb-only/integration_tests/tests:"],
    )
    for src in [
        ".config/nextest.toml",
        "scripts/validate.rs",
    ]
]

# Included verbatim by the e2e test harness (ci/manifest-plan/src/dag_source_ownership.rs).
buck_export_file(
    name = "ci/dag-source-ownership.json",
    visibility = ["//hermetic_infra/hermit/ci/manifest-plan:"],
)

# The repository as hermit-cli's Cargo integration tests see it at run time:
# CARGO_MANIFEST_DIR is hermit-cli/, and guests and scripts are found relative
# to its parent.
buck_filegroup(
    name = "integration-repository",
    srcs = {
        "detcore/tests/fixtures": "//hermetic_infra/hermit/detcore:tests-fixtures",
        "detcore/tests/lit": "//hermetic_infra/hermit/detcore/tests/lit:guest-sources",
        # stress_suite.rs compiles it at run time, with tests/stress/concurrency.rs.
        "flaky-tests/cas_sequence_easy.rs": "//hermetic_infra/hermit/flaky-tests:cas_sequence_easy.rs",
        "hermit-cli": "//hermetic_infra/hermit/hermit-cli:integration-cli-tree",
        "tests": "//hermetic_infra/hermit/tests:integration-tree",
        # rr_suite.rs compiles the pinned rr test programs at run time from
        # repository()/third-party/rr (its generated_dir prerequisite). This
        # package is in the fbcode cell; the rr producer is in the fbsource
        # root cell, so the cell is named explicitly. That producer exists only
        # in fbsource.
        "third-party/rr": "fbsource//third-party/rr:test-sources",
    }
    | {
        src: src
        for src in glob(["examples/**"])
        + [
            "ci/publish-hermit-e2e-artifact.sh",
            "ci/run-reverie-pin-check.sh",
            "ci/run-with-hermit-e2e-artifact.sh",
            "ci/verify-hermit-e2e-artifact.sh",
            "reverie.submodule.txt",
        ]
    },
    target_compatible_with = ["fbcode//opensource/macros:broken-in-oss"],
    visibility = [
        "//hermetic_infra/hermit/fb-only/shell_tests/tests:",
        "//hermetic_infra/hermit/hermit-cli:",
        "//hermetic_infra/hermit/tests:",
    ],
)

# The source snapshot the e2e cells read: the src/ of
# //hermetic_infra/hermit/ci/buck-e2e:bundle, at upstream paths. It holds what
# `test-harness run`, the manifest planner's --source-snapshot check and the
# guests' arguments read for the cells that package defines (measured: every
# other file can be absent),
# where upstream's bundle holds a full `git archive`. .gitmodules lets the
# planner treat programs in the uninitialized rr submodule as not available
# here rather than missing. The open-source build, whose bundle takes that
# `git archive` (ci/buck-e2e/stage), does not build it (tests:e2e-source-tree).
buck_filegroup(
    name = "e2e-source-snapshot",
    srcs = {"tests": "//hermetic_infra/hermit/tests:e2e-source-tree"}
    | {
        src: src
        for src in [
            ".gitmodules",
            # c-programs/record-replay-lseek-seek-cur passes it as a guest argument.
            "README.md",
            "ci/ci-reason-baseline.json",
            "ci/execution-owners.json",
            "ci/expected-e2e-plan.json",
            "ci/matrix-symmetry-baseline.json",
        ]
    },
    target_compatible_with = ["fbcode//opensource/macros:broken-in-oss"],
    visibility = ["//hermetic_infra/hermit/ci/buck-e2e:"],
)

# The three roots check.exit_status_class scans (hermit-cli/tests,
# detcore/tests, tests), at their repository paths. Its test binds the Rust
# files here to the declared list in fb-only/contract_checks.
buck_filegroup(
    name = "exit-status-class-repository",
    srcs = {
        "detcore": "//hermetic_infra/hermit/detcore:tests-rust",
        "hermit-cli": "//hermetic_infra/hermit/hermit-cli:integration-cli-tree",
        "tests": "//hermetic_infra/hermit/tests:integration-tree",
    },
    visibility = ["//hermetic_infra/hermit/fb-only/contract_checks:"],
)

# tests/ at its repository path, for check.backend_parity_mutation: its
# harness compiles the identity fixtures from tests/c and refuses unless its
# registry is exactly the tests/c/*.c sources that include parity_probe.h.
buck_filegroup(
    name = "backend-parity-mutation-repository",
    srcs = {"tests": "//hermetic_infra/hermit/tests:integration-tree"},
    visibility = ["//hermetic_infra/hermit/fb-only/contract_checks:"],
)

# The tree check.backend_abstraction runs on: the root Cargo.toml, each of its
# workspace members' manifest at <member>/Cargo.toml (MEMBER_MANIFESTS names
# the target that exports it), detcore/src, the DAG it checks its budget
# against, and the script beside its source gate, built by
# fb-only/contract_checks. That script exists only in fbsource.
buck_filegroup(
    name = "backend-abstraction-repository",
    srcs = {
        "Cargo.toml": "Cargo.toml",
        "ci/dag/validate.json": "ci/dag/validate.json",
        "detcore/src": "//hermetic_infra/hermit/detcore:backend-abstraction-src",
        "scripts/check-detcore-backend-abstraction.sh": "//hermetic_infra/hermit/scripts:check-detcore-backend-abstraction.sh",
        "scripts/detcore-backend-source.rs": "//hermetic_infra/hermit/fb-only/contract_checks:detcore-backend-source-release",
    }
    | {member + "/Cargo.toml": manifest for member, manifest in MEMBER_MANIFESTS.items()},
    target_compatible_with = ["fbcode//opensource/macros:broken-in-oss"],
    visibility = ["//hermetic_infra/hermit/fb-only/contract_checks:"],
)
