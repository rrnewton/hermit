"""Execution platforms and an input-carried rust toolchain for running hermit's buck2
build on Meta Remote Execution (RE) as well as locally.

RE workers (CentOS Stream 9) have gcc 11.5 but no rustc, so the RE-capable rust
toolchain passes hermit's pinned nightly sysroot (rust-toolchain.toml) to every action
as an input directory. Selected by config; the default build is unchanged (local,
system rustc). See shim/modes/.
"""

load("@prelude//rust:rust_toolchain.bzl", "PanicRuntime", "RustToolchainInfo")

def _rust_toolchain_impl(ctx):
    sysroot = ctx.attrs.sysroot
    tool = lambda name: RunInfo(args = cmd_args(sysroot, format = "{}/bin/" + name))
    return [
        DefaultInfo(),
        RustToolchainInfo(
            allow_lints = [],
            clippy_driver = tool("clippy-driver"),
            compiler = tool("rustc"),
            default_edition = ctx.attrs.default_edition,
            panic_runtime = PanicRuntime("unwind"),
            deny_lints = [],
            doctests = False,
            nightly_features = True,
            report_unused_deps = False,
            rustc_binary_flags = [],
            rustc_flags = ctx.attrs.rustc_flags,
            rustc_target_triple = "x86_64-unknown-linux-gnu",
            rustc_test_flags = [],
            rustdoc = tool("rustdoc"),
            rustdoc_flags = [],
            warn_lints = [],
        ),
    ]

hermit_input_rust_toolchain = rule(
    impl = _rust_toolchain_impl,
    attrs = {
        "default_edition": attrs.string(default = "2024"),
        "rustc_flags": attrs.list(attrs.arg(), default = []),
        "sysroot": attrs.source(allow_directory = True),
    },
    is_toolchain_rule = True,
)

def _executor(mode, use_case):
    if mode == "local":
        return CommandExecutorConfig(local_enabled = True, remote_enabled = False)
    return CommandExecutorConfig(
        # Limited hybrid: every action that can run remotely does, and only local-only
        # actions (e.g. pkg-config probes of the host) run locally. "hybrid" also falls
        # back to local when RE itself fails; "remote" does not.
        local_enabled = True,
        remote_enabled = True,
        use_limited_hybrid = True,
        allow_limited_hybrid_fallbacks = mode == "hybrid",
        remote_execution_properties = {"platform": "linux-remote-execution"},
        remote_execution_use_case = use_case,
        remote_cache_enabled = True,
    )

def _platforms_impl(ctx):
    constraints = dict()
    constraints.update(ctx.attrs.cpu_configuration[ConfigurationInfo].constraints)
    constraints.update(ctx.attrs.os_configuration[ConfigurationInfo].constraints)
    configuration = ConfigurationInfo(constraints = constraints, values = {})
    platform = ExecutionPlatformInfo(
        label = ctx.label.raw_target(),
        configuration = configuration,
        executor_config = _executor(ctx.attrs.mode, ctx.attrs.use_case),
    )
    return [DefaultInfo(), ExecutionPlatformRegistrationInfo(platforms = [platform])]

hermit_execution_platforms = rule(
    impl = _platforms_impl,
    attrs = {
        "cpu_configuration": attrs.dep(providers = [ConfigurationInfo]),
        "mode": attrs.enum(["local", "hybrid", "remote"]),
        "os_configuration": attrs.dep(providers = [ConfigurationInfo]),
        "use_case": attrs.string(),
    },
)
