"""Hermit e2e manifest cells as Buck tests, run by Tpx locally or on Meta RE.

One test per cell of ci/expected-e2e-plan.json. Each runs `test-harness run` for exactly
that cell (cell.sh) and reports one Tpx HPHP-JSON result with the cell's evidence (the
harness rows, verify report and both DETLOGs) as Tpx artifacts. Tpx owns retries: cells
pass --no-retry to the harness, so `buck2 test ... -- --retry 1` retries a failed cell
and the ingester keeps "passed only on rerun" red.

Routing (per cell, first match wins; see _route):
  local: kvm backend, privileged lane, a host-capability requirement, a `requires` tool
         RE workers lack, the LOCAL_TESTS deny-list, a test measured to fail only on RE
         (re_exclusions.json), or a PMU-armed cell under -c hermit_e2e.pmu_on_re=false.
  re:    everything else.
`-c hermit_e2e.routing=local` runs every cell locally (the buck-local test mode).

RE-routed cells support Buck's test execution caching: when the bundle (hermit, harness,
fixtures, sources) and the cell are unchanged, Buck answers from the RE action cache
instead of re-running. Local executions are never cached by Buck, and `--stress-runs`
disables caching.
"""

RE_PLATFORM = {"platform": "linux-remote-execution"}

# Guest tools a cell may run that RE workers (CentOS Stream 9) do not have, measured
# 2026-10-01/02.
RE_MISSING_TOOLS = ["clang", "cmake", "gdb", "git", "lua5.4", "node", "ruby", "rustc", "tclsh"]

# Cells that must run locally for reasons `requires` does not express.
LOCAL_TESTS = {
    "c-programs/environment-and-workdir": "asserts a fresh tmpfs workdir at /test, which RE cannot create",
    "compat/cargo": "runs the host rustup toolchain through compat fixtures",
    "compat/clang": "runs clang, missing on RE, without declaring it",
    "compat/cmake": "runs cmake, missing on RE, without declaring it",
    "compat/git": "runs git, missing on RE, without declaring it",
    "compat/lua": "runs /usr/bin/lua, missing on RE, without declaring it",
    "compat/ruby": "runs /usr/bin/ruby, missing on RE, without declaring it",
    "compat/tcl": "runs tclsh, missing on RE, without declaring it",
}

# Cells that pass only inside the pinned-root container the cargo flow uses: neither a
# buck-local host nor an RE worker provides that identity. They still run (local), and
# the ingester records them against this reason.
PINNED_ROOT_ONLY = {
    "c-programs/environment-and-workdir/custom": "asserts HOSTNAME=hermetic-container.local, the pinned-root PATH and /results paths, which only the pinned-root container provides",
}

# ptrace/liteinst cells that run with --max-timeslice=disabled and so never arm the PMU.
PMU_FREE_CELLS = [
    "c-programs/io-uring-fallback/custom@ptrace",
    "system-utils/clock-determinism/custom@liteinst",
    "system-utils/clock-determinism/custom@ptrace",
]

def cell_id(cell):
    return "{}/{}@{}".format(cell["test"], cell["mode"], cell["backend"])

def cell_slug(cell):
    # The harness's own artifact slug: unique over the plan, at most 63 characters.
    return "{}-{}-{}".format(cell["test"].replace("/", "-"), cell["mode"], cell["backend"])

def pmu_armed(cell):
    return cell["backend"] in ["ptrace", "liteinst"] and cell["category"] != "compat" and cell_id(cell) not in PMU_FREE_CELLS

def _route(cell, pmu_on_re, re_exclusions):
    """Returns (where, reason): where is "local" or "re"."""
    if cell["backend"] == "kvm":
        return ("local", "kvm backend: RE workers have no /dev/kvm")
    if cell["lane"] == "privileged":
        return ("local", "privileged lane")
    if cell.get("requires_host_capabilities"):
        return ("local", "needs host capability " + ",".join(cell["requires_host_capabilities"]))
    missing = [t for t in cell.get("requires", []) if t in RE_MISSING_TOOLS]
    if missing:
        return ("local", "requires " + ",".join(missing) + ", missing on RE")
    pinned = PINNED_ROOT_ONLY.get("{}/{}".format(cell["test"], cell["mode"]))
    if pinned:
        return ("local", "pinned-root-only: " + pinned)
    if cell["test"] in LOCAL_TESTS:
        return ("local", LOCAL_TESTS[cell["test"]])
    if cell["test"] in re_exclusions:
        return ("local", "measured: " + re_exclusions[cell["test"]]["reason"])
    if pmu_armed(cell) and not pmu_on_re:
        return ("local", "arms the PMU and -c hermit_e2e.pmu_on_re=false")
    return ("re", "")

def _cell_test_impl(ctx):
    cmd = cmd_args(ctx.attrs.runner, ctx.attrs.args)
    remote = ctx.attrs.route == "re"
    executor = CommandExecutorConfig(
        local_enabled = not remote,
        remote_enabled = remote,
        remote_execution_properties = RE_PLATFORM,
        remote_execution_use_case = ctx.attrs.re_use_case,
        # Lookups only: Buck never uploads test results it executed locally.
        remote_cache_enabled = remote,
    )
    env = dict(ctx.attrs.env)
    env["HERMIT_E2E_BUNDLE"] = ctx.attrs.bundle[DefaultInfo].default_outputs[0]
    return [
        DefaultInfo(),
        RunInfo(args = cmd),
        ExternalRunnerTestInfo(
            type = "json",
            command = [cmd],
            env = env,
            labels = ctx.attrs.labels,
            default_executor = executor,
            run_from_project_root = True,
            use_project_relative_paths = True,
            supports_test_execution_caching = remote,
        ),
    ]

hermit_e2e_cell_test = rule(
    impl = _cell_test_impl,
    attrs = {
        "args": attrs.list(attrs.string()),
        "bundle": attrs.dep(),
        "env": attrs.dict(attrs.string(), attrs.string(), default = {}),
        "labels": attrs.list(attrs.string(), default = []),
        "re_use_case": attrs.string(default = "fbcode_re_tests"),
        "route": attrs.enum(["local", "re"]),
        "runner": attrs.source(),
    },
)

def _bundle_impl(ctx):
    """The cell runner's input tree: hermit/{hermit,install}, bin/, src/, build/, run-state/."""
    hermit = ctx.attrs.hermit[DefaultInfo].default_outputs[0]
    entries = {
        "bin": ctx.attrs.harness_bin,
        "build": ctx.attrs.fixtures,
        "hermit/hermit": hermit,
        "hermit/install": ctx.attrs.install,
        "run-state": ctx.attrs.run_state,
        "src": ctx.attrs.src,
        "SOURCE_SHA": ctx.attrs.source_sha,
    }
    return [DefaultInfo(default_output = ctx.actions.copied_dir("bundle", entries))]

hermit_e2e_bundle = rule(
    impl = _bundle_impl,
    attrs = {
        "fixtures": attrs.source(allow_directory = True),
        "harness_bin": attrs.source(allow_directory = True),
        "hermit": attrs.dep(),
        "install": attrs.source(allow_directory = True),
        "run_state": attrs.source(allow_directory = True),
        "source_sha": attrs.source(),
        "src": attrs.source(allow_directory = True),
    },
)

def hermit_e2e_cells(plan, re_exclusions, bundle = ":bundle", runner = "cell.sh"):
    """One hermit_e2e_cell_test per plan cell, plus test suites all / re / local."""
    routing = read_root_config("hermit_e2e", "routing", "hybrid")
    if routing not in ["hybrid", "local"]:
        fail("-c hermit_e2e.routing must be hybrid or local, got " + routing)
    pmu_on_re = read_root_config("hermit_e2e", "pmu_on_re", "true") == "true"
    if plan["schema"] != 1:
        fail("expected-e2e-plan.json schema must be 1")
    by_route = {"local": [], "re": []}
    for cell in plan["cells"]:
        where, reason = _route(cell, pmu_on_re, re_exclusions["tests"])
        if routing == "local":
            where = "local"
        name = cell_slug(cell)
        hermit_e2e_cell_test(
            name = name,
            runner = runner,
            args = [cell["test"], cell["mode"], cell["backend"]],
            bundle = bundle,
            env = {
                "HERMIT_E2E_ROUTE": where,
                "HERMIT_E2E_ROUTE_REASON": reason,
            },
            labels = [
                "tpx-enable-artifact-reporting",
                "hermit_e2e",
                "hermit_e2e_route_" + where,
                "hermit_e2e_backend_" + cell["backend"],
            ],
            route = where,
        )
        by_route[where].append(":" + name)
    native.test_suite(name = "all", tests = by_route["local"] + by_route["re"])
    native.test_suite(name = "re", tests = by_route["re"])
    native.test_suite(name = "local", tests = by_route["local"])
