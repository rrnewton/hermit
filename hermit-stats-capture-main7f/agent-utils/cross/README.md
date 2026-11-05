# Behavioral differential tests

`differential.py` runs the independently implemented Python and Rust commands
against the same representative, adversarial, boundary, and seeded-random
inputs. A nonzero exit means the observable contracts diverged, or that a
check did not run in a run that claims full coverage (see "Skipped checks and
partial runs" below).

Run the complete paired-tool contract:

```sh
python3 cross/differential.py --tool all
```

The harness resolves each Rust command through its tracked `rs/bin` Cargo
launcher before starting comparisons. Cargo validates the real workspace cache
and the launcher refreshes source/binary provenance. While holding the same
checkout cache lock, the harness revalidates that provenance and makes a private,
hash-checked executable copy for the tool's full subprocess corpus. Concurrent
cleaning or rebuilding therefore cannot mix Rust versions within a differential.

Run one tool, or increase the randomized corpus reproducibly:

```sh
python3 cross/differential.py --tool dagrun
python3 cross/differential.py --tool tick-hub --random 100 --seed 8675309
python3 cross/differential.py --tool pr-landing-planner --random 100 --seed 8675309
python3 cross/differential.py --tool cpuset-alloc
python3 cross/differential.py --tool herdr-run
python3 cross/differential.py --tool herdr-agent
```

When `--tool` is omitted, the harness checks `dagrun`.

## What is compared

| Tool | Differential contract |
|---|---|
| `dagrun` | Canonical DAG listing, visualization, JSON, YAML loading, validation failures, plan and summary data, selection, successful parallel-speedup sweeps, argument forwarding, stress reports, resource sizing, run outcomes, profile-store schema, CLI surface, and enforcement-capability manifest. |
| `cpuset-alloc` | CLI surface, version, durable-ledger status and reclaim JSON, malformed and boundary arguments, reservation behavior, mutation self-test verdicts, and hard-pin fail-closed behavior. |
| `tick-hub` | Strict JSON/YAML config loading, canonical emission, cadence state, reminder gates, freshness output, flush transitions, CLI failures, numeric boundaries, malformed documents, and randomized tick configurations. |
| `pr-landing-planner` | Fixture collection, graphs, clusters, status and plan output in every format, exact-head/base validation, approval and gate safety decisions, ordering/conflict groups, malformed evidence, numeric boundaries, and randomized PR graphs. |
| `herdr-run` | Both editions invoked directly: the Python package under the real interpreter and the Rust binary. Output bytes (decoded losslessly, so a byte that is not UTF-8 is still compared) for every help screen, `--version`, `quickstart`, `userguide`, and the `--dump-surface` JSON, which must also equal the promised subcommand and option surface; the YAML-subset configuration loader with every configuration message, including each document in the shared YAML corpus, raw YAML 1.1 line breaks, flow forms that YAML readers disagree on, and names holding invisible characters (format characters, default-ignorable code points, and blanks); discovery and explicit paths; allow/deny policy, inert rendering, and refusal quoting, including `~` expanded from a hostile `HOME` before the policy judges it, abbreviated long options, `--exec`, `git clone -u`, subcommand-scoped options, and global options that take the next argument; the `--timeout`/`--wait-ready` grammar; agent inference with Unicode white space; bytes that are not UTF-8 in argv, the environment, and directory names; six locales; closed, full, and broken stdout (with and without `PYTHONUNBUFFERED`), a broken stderr, SIGINT mid-write (default and ignored), and a file-size limit; an argument that reads like the interpreter's `-X utf8=0`; OS error text; dry runs, audit JSONL, spool modes, and the un-ignored spool warning; paths and names holding terminal controls or invisible characters, which must never reach the terminal raw, in messages or in JSON; what each edition hands a process it starts (environment, descriptors, and signal dispositions, recorded by a stand-in `git` that reads its environment entry by entry from `/proc`, including a caller's ignored `SIGCHLD` and an environment holding a repeated name, an entry without `=`, and empty names, which must arrive exactly as the shared environment rule makes it, also after the Python edition restarts itself in UTF-8 mode), a stand-in with no `#!` line, which neither edition may run through `/bin/sh`, and a started process whose descendant holds its output past the bound; `net-doctor` refusals; `reap` over planted, malformed, too-deep, and too-large records, and run directories ordered by raw bytes (like `status` and the unsafe-path cases, against a fake Herdr behind the account-home shim described below, never against a Herdr installed on the host); a deleted working directory; and an end-to-end group (`e2e/*`) against a fake Herdr, described in "herdr-run end to end" below. The golden corpora in `rs/herdr-run/testdata/`, which both editions' unit tests load, additionally pin each protocol call, readiness rule, record format, admission, and text rule. `common/bin/herdr-run` is a smoke test only; see "herdr-run and its launcher" below. |
| `herdr-agent` | CLI bootstrap, installed guide, exact pane and stable-session targeting, identity assertions, status/read behavior, literal multiline submission, native readiness transitions, durable FIFO state, pending versus possibly-submitted outcomes, malformed-head quarantine, and fail-closed target contradictions. Each package talks through its production client to an isolated executable Herdr protocol fixture. |

### herdr-run and its launcher

The herdr-run differential compares the two TOOLS, so it starts each edition
itself rather than through `common/bin/herdr-run`. That launcher is shared by
every tool here, and some of its behaviour is launcher behaviour, not tool
behaviour. The smoke test strips the launcher's own `[herdr-run] ...` lines and
then requires identical tool output; these launcher-level differences are left
in place and are not herdr-run divergences:

- Each engine announces itself on stderr, and the Rust path also reports its
  Cargo cache verdict (`rust cache=verified ...`), or Cargo's own output when it
  rebuilds.
- Under a working directory that has been deleted, the Rust launcher fails in
  its own `cd` back to the caller's directory before the tool starts; the Python
  path starts the tool, which reports the missing directory itself.
- The Rust launcher needs `HOME` (or `CARGO_HOME` and `RUSTUP_HOME`) to find the
  toolchain; the Python path does not.
- Under a locale the C library cannot load, bash in the launcher prints its own
  `setlocale` warning before either engine runs.
- The Python path runs `$CI_HUB_PYTHON`, else the first `python3` on `PATH`,
  which may be a wrapper rather than the interpreter. A wrapper that cannot pass
  on an argument that is not valid UTF-8 fails before herdr-run starts, where the
  tool itself would refuse the argument with exit status 2.
- Each engine starts behind a different number of shells, so bash's own
  warnings (about a deleted working directory, for example) can appear a
  different number of times.

The herdr-run harness removes interpreter settings a developer's shell may
carry (`PYTHONUNBUFFERED`, `PYTHONIOENCODING`, `PYTHONUTF8`, `PYTHONWARNINGS`,
and the like) before it starts either edition, so the Python edition runs as an
ordinary caller starts it; a case about one of them sets it explicitly. With
`PYTHONUNBUFFERED` inherited, a failed buffered write had looked identical in
both editions while the default buffered interpreter exited 120.

### herdr-run end to end

Both editions find Herdr only in fixed places under the account home that the
account database reports, never through `PATH` or `HOME`, and neither has an
option naming an executable. That is production behaviour, and the end-to-end
group keeps it: it changes the account database's answer instead.
`cross/herdr_e2e/account_home_shim.c` is compiled with the system C compiler
(`cc`, `gcc`, or `clang`) at test time and preloaded (`LD_PRELOAD`) into both
editions; it makes `getpwuid_r` and `getpwuid` report a temporary directory as
the current account's home. `cross/herdr_e2e/fake_herdr.py` is installed there,
reached through a `/bin/sh` launcher at `.local/bin/herdr` that first reads
what the call was handed -- its ignored signals and its environment, entry by
entry from `/proc` -- before an interpreter can change either, with a stand-in
`sleep` process as each pane's shell, so `/proc` evidence is real. Each edition
then runs its production client against its own copy of the fake, and the
harness compares every step's exit status and output, every argument vector the
fake was called with, the environment and ignored signals every call was handed,
and every file left in the project and the account's state directory (with
mode), byte for byte after normalizing roots, run IDs, times, process IDs,
stand-in shells (named by pane), start ticks, and the durations each edition
measures itself. Cases cover runs
(exit statuses, a signal, bytes that are not UTF-8, `--cwd`, refusals, a timeout),
`target`, `status`, the session cache (reuse, and caches that are corrupt,
stale, hostile, or nested too deep), `reap` verdicts over live panes (SHELL_ALIVE,
STALE, GONE, IN_FLIGHT, OUT_OF_SCOPE), `net-doctor` against a local and a missing
repository, the pane cap evicting the least recently used idle tab and refusing
when every tab is busy, busy and half-typed panes, every Herdr call failing with
a hostile message, replies that are not what they claim (including a lone
surrogate under a duplicate key), a Herdr with no `#!` line, a caller that
ignores `SIGCHLD`, a caller that hands over an environment with a repeated name,
an entry without `=`, and empty names (every Herdr call and the direct
`net-doctor` probe must get exactly what the shared environment rule makes of
it), a caller that leaves signal 33 and others ignored (every Herdr call must
still have them ignored), and retention. A case also pins what its key step
must say, so two editions failing the same way cannot pass it.

When a case is over, the harness ends every process the fake started, and only
those: the fake records each one's start time, and a process, or the process
group it leads, is signalled only while it is still the process that was
recorded. What remains of a group whose leader has exited is found by the
case's own `HOME` in its environment, never by the leader's number, which may
by then name another process. The harness checks that rule on itself before the
cases run.

`status`, `reap`, the unsafe-path cases, and the `status` calls in the
configuration and locale groups run behind the same shim, each project with
its own fake Herdr that has no workspace; their Herdr calls are compared too,
and must only read.

The shim is also a guard. It refuses (`EPERM`) to start any program named
`systemd-run`, and any `herdr` outside the temporary account home, and logs what
it refused; a case fails if anything was refused, and no case asks for a server
to be started. Before any case runs, the harness proves that each edition reads
the temporary account home (`~` with `HOME` unset is expanded from the account
database, and `check` contacts no Herdr) and that the guard refuses a decoy named
`systemd-run`; if either fails, nothing that could reach a Herdr runs and the
group fails. Those checks are skipped, as listed and required skips, only when
no C compiler exists
(`c-compiler`) or when a Herdr is installed in `/usr/local/bin` or `/usr/bin`,
which herdr-run would find before the account home (`system-herdr`). A compiler
that exists but cannot build the shim is a failure.

Machine-oriented output is compared byte for byte where it is specified as
canonical. YAML emitters and other intentionally idiomatic text are parsed or
checked structurally instead. Concurrent scheduler traces can complete in a
different order, so the harness compares their deterministic final outcome and
report rather than timing-dependent progress lines.

The `herdr-agent` and `agentctl` harnesses run every edition with a guard
directory first on `PATH`. It holds a failing stub for each host tool an edition
could reach by a bare name: `agentcloudctl`, `agentterm`, `agy`, `claude`,
`codex`, `gh`, `herdr`, `muse`, `opencode`, `tmux`, and `wrkslots`. A stub records its call,
and the check `harness/no-host-cli` fails when any call was recorded, so a case
must hand both editions a fixture (`--goal-command-json`, for example) rather
than compare two runs of whatever the developer's machine has installed. The
editions also do not inherit the caller's `AGENTCTL_WRKSLOTS_BIN`, `AGY_BIN`,
`CODEX_BIN`, `HERDR_BIN`, or `MUSE_BIN`, whose absolute paths would bypass the
stubs. Both editions run the system Git (`git check-ignore` for profile
configuration), and Git runs helper programs that its configuration names, such
as `core.fsmonitor`, by absolute path. So an edition inherits no `GIT_*`
variable (`GIT_CONFIG_COUNT`, `GIT_CONFIG_PARAMETERS` and the rest), reads no
system or global Git configuration (`GIT_CONFIG_NOSYSTEM=1`,
`GIT_CONFIG_GLOBAL=/dev/null`), and finds no repository above the case directory
(`GIT_CEILING_DIRECTORIES`). The case repository's own configuration is still
read, so `core.fsmonitor=false` is given at command scope (`GIT_CONFIG_COUNT`),
which overrides it, and `GIT_NO_LAZY_FETCH=1` stops Git fetching an object that a
promisor remote should supply, which would run the remote's transport program. A
case's `empty_environment` may not name a `GIT_*` variable. The harness creates
each case repository with that same environment (`init_case_repository`), so a
template directory the caller names (`GIT_TEMPLATE_DIR`, `init.templateDir`) is
not copied into it.

Without `--herdr-bin`, the Rust edition looks `herdr` up on `PATH` and meets the
stub, but the Python edition runs the `herdr` installed in a fixed location such
as `~/bin`, which a stub cannot shadow. The harness therefore does not start the
editions at all for an invocation that names no Herdr file inside the case
directory (symbolic links resolved, with `..` read both as the kernel and the
Rust client read it and as the Python client does; a `--herdr-bin` after a `--`
terminator does not count), unless its command is one that never contacts Herdr
(`capabilities`, `profiles`, `quickstart`, `skill`, `userguide`, or help and
version output). It records the refusal for `harness/no-host-cli` instead.
Every other command, including a deliberately invalid one, passes
`--herdr-bin <HERDR>`. Any `--herdr-bin` that names a file outside the case
directory refuses the invocation, even after a `--`: after a `--` at the start
of the command line, the Python subcommand parser reads the remaining tokens as
options again. `agentctl chat` is refused even with the fixture, apart from its
help, because the Python edition's Chat bridge ignores `--herdr-bin` and always
runs the installed Herdr. The command is found after every option either edition
accepts before it, spaced or with `=`, including the Rust-only `--agentcloudctl-bin`,
`--agentterm-bin`, `--agentcloud-url` and `--from-session`, so
`--from-session=ID chat publish` is refused too; a test reads both parsers to keep
that list complete. After any other option, a `chat` anywhere later refuses the
invocation, since where the command starts is then unknown.

A stub cannot catch a program named by an explicit path either, so the same
refusal applies, wherever the option appears, to an `--agentcloudctl-bin`,
`--agentterm-bin` or `--claude-bin` (Rust edition only) that names a file
outside the case directory, and to a `--goal-command-json` unless it is `[]` or
exactly the fixture goal transport, `["<HERDR>","goal-rpc"]`: its program must be
the case's `fake-herdr` under both readings of `..` (a link to it counts), and its
only argument `goal-rpc`, the mode that answers requests on standard input and
starts nothing. Naming any file inside the case is not enough, because every case
also holds `fake-muse-runtime`, a copy of the Python interpreter.
Without `--goal-command-json`, an edition runs the goal command stored in the
agent record, so the same rule applies to every `goal_command` and
`native_command` field in the `.json` files of each registry the invocation
can read: each `--registry` or `--state` value, and both defaults (`.agentctl`
and `.herdr-agents`) always, since a token after a `--` that only looks like
the option leaves the default in effect. Each registry is read under both
readings of `..`, and it and everything under it, with symbolic links
followed, must stay inside the case directory; a directory that cannot be
listed is refused, because an edition may still open a record in it by name.
The fixture Herdr applies the rule to the custom harness program it runs for
`pane run`, recording a refusal in the same log, and the editions get an empty
standard input, so a request read from it (by `agentctl mcp`) cannot name a
goal command the harness has not checked.

The harness also asks each implementation for its embedded user guide and
checks that the page is complete and does not mention the sibling language or
package manager. Artifact-level wheel and crate checks live in
`scripts/check_python_packages.py` and `scripts/check_rust_packages.py`.

## Cgroup and CPU-set checks

Kernel and service-manager capabilities differ across developer machines and
CI containers. Scheduling-core comparisons therefore opt out of cgroup boxing
explicitly and exercise the deterministic engine. The language-specific test
suites cover cgroup file mutation and cleanup with controlled fixtures.

Hard CPU-set wrappers do not degrade to process affinity. When a host cannot
create and mutation-verify an inescapable subtree scope, both editions must
refuse to launch the workload with the same operational status. On a capable
host, the differential additionally verifies successful reserve/apply/release
behavior. On a host that refuses, the identical refusal counts as a pass and the
reserve/apply/release check is listed as skipped (`hard-cpuset`).

The `selftest` mutation verdict counts only when the probe ran, that is when
both editions report `HARD` or both report `SOFT_OR_INERT`. When both report
`UNTESTABLE`, the identical refusal counts as a pass and
`selftest:mutation-verdict` is listed as skipped: `boxing` when
`systemd-run --user --scope` is unavailable, `multi-cpu` when no core is left
outside the reservation to escape to. An agreed `ERROR`, an `UNTESTABLE` whose
reason differs or is not one of those two, and a verdict whose exit status does
not match it are failures.

## Skipped checks and partial runs

Some checks cannot run everywhere. A skipped check is its own result: it is
never counted as a pass, and the summary lists every skipped check by name with
a total:

```text
cross[dagrun]: SKIPPED/UNVERIFIED 4 check(s), not counted as passes:
  SKIPPED [boxing; REQUIRED] profile-timeseries: the outer scheduler has no cgroup subtree to delegate
  ...
```

| Kind | Why the check did not run |
|---|---|
| `boxing` | no cgroup-v2 boxing: no working systemd `--user` scope, or an outer scheduler with no cgroup subtree to delegate |
| `delegated-live-scope` | the harness runs inside a parent-owned delegated cgroup, where creating a live systemd scope could escape the outer step |
| `hard-cpuset` | both engines refused a HARD cpuset pin on this host |
| `multi-cpu` | fewer than two usable CPUs: one in the CPU affinity mask, or a cgroup CPU quota below two cores |
| `eight-cpu` | a core budget below eight CPUs (the tighter of the CPU affinity mask and any cgroup CPU quota), so the CPA planner's memory cap cannot be shown to bind |
| `c-compiler` | no C compiler (`cc`, `gcc`, or `clang`) to build the account-home shim the herdr-run end-to-end group preloads |
| `system-herdr` | a Herdr installed in `/usr/local/bin` or `/usr/bin`, which herdr-run would find before the fake one in the temporary account home |

By default a run claims full coverage, so any skip makes it exit nonzero with an
`INCOMPLETE` line. A lane that is partial by design declares which kinds it
accepts, with `--allow-skip KIND[,KIND]` or
`AGENT_UTILS_CROSS_ALLOW_SKIP=KIND[,KIND]`. It says so on its first line, still
lists every skip, and ends with `PARTIAL` instead of `OK`, counting the skips
of each kind: `... 3 check(s) were skipped and are UNVERIFIED (allowed skips by
kind: eight-cpu 1, multi-cpu 2)`. An unknown kind is refused.

| Lane | Coverage |
|---|---|
| `make cross`, invoked by hand | full: every skip fails. No workflow or scheduled job runs it, so the live-scope checks the validation nodes skip run only here ([agent-utils issue 165](https://github.com/rrnewton/agent-utils/issues/165)) |
| validation nodes `cross.dagrun.differential` and `cross.dagrun.cpuset-differential` | partial for `delegated-live-scope` only; boxing stays required |
| hosted repository CI and the nightly full run | additionally partial for `boxing`, because hosted runners run the graph with `--allow-cgroup-failure`, and for `eight-cpu`, because hosted runners have fewer than eight CPUs |

The CPA memory-cap check needs a core budget of eight inside the
`cross.dagrun.differential` node. The node asks for eight
(`preferred_inner_jobs: 8`), and `scripts/validate.py` plans with
`--planner critical-path`, which never changes a step's width from profile
history; only `--planner cpa` does. The node therefore gets fewer than eight
cores only from an input the caller sets: a host with fewer than eight usable
CPUs, `VALIDATE_MAX_CPUS` below eight, `--cores` below eight or `--planner cpa`
in `VALIDATE_DAGRUN_FLAGS`, or a smaller CPU ceiling inherited from an outer
dagrun. Any of these skips the check as `eight-cpu`, and the node, and with it
the local validate run, fails `INCOMPLETE` until the caller declares
`AGENT_UTILS_CROSS_ALLOW_SKIP=eight-cpu`. The node's width stays resizable
rather than fixed at eight, because dagrun refuses a fixed width above the
run's `--max-cpus`, and four-core hosted runners must still run it.

A boxed check counts only when both engines print `cgroup boxing ACTIVE`, which
they do exactly when they have set up per-step cgroups. When the outer scheduler
has no cgroup subtree to delegate, the boxed checks are listed as `boxing` skips
and not run, because a nested engine would run them uncontained.

`--tool all` ends with one line for the whole run, after each tool's own
verdict: `cross: OK - ...`, `cross: PARTIAL - N skipped across tools (TOOL N,
...; by kind: KIND N, ...)`, or `cross: FAILED - ...` naming each tool that did
not pass.

dagrun reports a node by its exit status, so a partial cross node that exits 0
is shown as `✓ PASS`, and the run's last line counts it among the passes. The
node's own `PARTIAL` line appears only in the brackets after that `PASS`.
`scripts/validate.py` therefore sets `AGENT_UTILS_CROSS_COVERAGE_DIR` for the
graph. Each cross verdict leaves a JSON record there, and when any node skipped
checks the run ends with `validate: PARTIAL - every selected node passed, but N
cross check(s) were skipped and are UNVERIFIED (KIND N, ...)`, listing each node
with its own count per kind, instead of `validate: OK`. Its exit status stays 0.
Every selected cross node must leave a
record: a record that cannot be read, or a selected node that left none (for
example one that `VALIDATE_DAGRUN_FLAGS` narrowed out of the run), is listed as
could not be confirmed and also ends the run with `PARTIAL`. Under GitHub
Actions, where the job still shows a green tick, a `PARTIAL` run also emits a
`::warning` annotation and appends the verdict to the job summary. A graph run
directly with `common/bin/dagrun run` has no such summary: read the bracketed
line of each cross node.

## Fixtures and reproducibility

`cross/yaml_fixtures/` contains YAML scalar, quoting, block-text, duplicate-key,
and numeric edge cases. The harness also consumes bundled examples where they
form useful paired fixtures. Random cases are generated from `--seed`; a failed
seed and case index can therefore be replayed exactly.
