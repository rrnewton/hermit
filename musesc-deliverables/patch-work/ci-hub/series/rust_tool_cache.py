"""A content-addressed cache of the Rust programs the ledger publication runs.

Background. Each ledger publication (ci-hub/series/mirror.py, run by every
validate unit's ExecStopPost and by the publication backlog) runs three Rust
programs that it compiles from source first:

  * ci/compat-envelope/scorecard.rs, a rust-script, which renders the scorecard;
  * hermit-manifest-plan, a binary of the Hermit Cargo workspace, which the
    scorecard runs while rendering;
  * ci-hub/ci-hub.rs, a rust-script, whose compatibility-website-derived-census
    subcommand emits a finalized run's derived proof.

Every validate unit has a fresh checkout path, its own CARGO_HOME and its own
rust-script cache, so `cargo run` and `rust-script --force` compiled all of
them from nothing on every publication: about 31 s and 95 s of wall time and
600 CPU-seconds for the first two alone (devbig014, 2026-10-04).

What this module does. A program family (SCORECARD or CI_HUB) is built once
per distinct set of declared inputs and stored under a key that is the
sha256 of those inputs:

  * the committed tree entries (`git ls-tree`) of the family's source paths
    in each repository it reads, and every Cargo.toml the Hermit checkout
    commits (Cargo loads each workspace member's manifest to resolve the
    workspace and check Cargo.lock); any uncommitted change under those paths
    makes the cache unavailable, and the caller builds on its direct path;
  * for each rust-script, the Cargo.toml rust-script generates for it and the
    Cargo.lock `cargo generate-lockfile` resolves for that package;
  * `rustc -vV`, `cargo -vV` and `cc --version`;
  * the sha256 of every Cargo configuration file Cargo loads for the build
    (each `.cargo/config` and `.cargo/config.toml` from the build directory up
    to `/`, and those in CARGO_HOME);
  * the build environment and the build commands.

The build environment is fixed, not inherited: the process starts from an
empty environment and receives only PATH, HOME, RUSTUP_HOME and
CARGO_BUILD_JOBS from the caller, plus CARGO_HOME, RUSTUP_TOOLCHAIN,
RUSTUP_AUTO_INSTALL=0 and CARGO_UNSTABLE_CHECKSUM_FRESHNESS=true set here.
RUSTFLAGS, RUSTC, CARGO_TARGET_DIR, LD_PRELOAD and every other variable
therefore never reach a build. The stored programs are what a cold build in
that environment produces from the keyed inputs. A file a program reads at
compile time that is outside its family's declared source paths is a defect
in this module's input list, to be fixed by adding the path.

Where the builds happen. Builds run in a slot,
`<state root>/ignored/ci-hub/rust-tool-cache/<family>/<n>/`, which holds:

    tool        symlink to the tool checkout being built
    cargo-home  symlink to the caller's CARGO_HOME
    pkg/<name>  the Cargo package rust-script generates for each script
    target/     Cargo's target directory, kept from one build to the next
    target-identity  the toolchain and Cargo configuration target/ was built with
    store/<key> the programs built for <key>, with inputs.json, the key document

Cargo records source, registry and target paths in its fingerprints, so these
stable paths let a build with a new key recompile only the crates whose inputs
changed. Checksum freshness makes Cargo compare the content of each source
file rustc read rather than its modification time, because the `tool` link is
repointed to other checkouts. (Files that a build script reads are still
compared by modification time; no local package of either family has a build
script.) Cargo's fingerprints do not record the linker or the C compiler that
build scripts run, so the target directory is emptied before a build whenever
the keyed toolchain or Cargo configuration differs from the one recorded in
target-identity.

The programs record slot paths: Cargo bakes CARGO_MANIFEST_DIR and file!()
into them, and the scorecard finds its tool checkout through file!() while it
renders. A slot is therefore held with an exclusive lock, and its `tool` link
left pointing at the publication's checkout, from preparation until the end of
the session (mirror() opens one session per publication).
"""

from __future__ import annotations

import contextlib
import contextvars
import dataclasses
import fcntl
import hashlib
import json
import os
import shutil
import signal
import subprocess
import sys
import threading
import time
import tomllib
from pathlib import Path
from typing import Iterator, Mapping, Sequence

SCHEMA = "rust-tool-cache/v1"
CACHE_RELATIVE = Path("ignored/ci-hub/rust-tool-cache")
#: Slots per family. A publication holds its slot until it ends, and
#: publications overlap only while one prepares and another publishes.
SLOT_COUNT = 2
#: How long prepare() waits for a free slot before the caller builds directly.
LOCK_WAIT_SECONDS = 60.0
POLL_SECONDS = 0.5
#: Completed builds kept in each slot's store, newest first.
STORE_ENTRIES = 4
#: A slot's target directory is emptied before a build when it exceeds this.
TARGET_BUDGET_BYTES = 16 * 1024 ** 3
#: The direct path gives each compile the same limit (mirror.DEFAULT_TIMEOUT_SECONDS).
COMMAND_TIMEOUT_SECONDS = 900.0
#: How long a killed command's process group may take to empty.
GROUP_EXIT_SECONDS = 30.0
#: Variables taken from the caller's environment; nothing else is inherited.
INHERITED = ("PATH", "HOME", "RUSTUP_HOME", "CARGO_BUILD_JOBS")
CHECKSUM_FRESHNESS_ENV = "CARGO_UNSTABLE_CHECKSUM_FRESHNESS"
LOG_PREFIX = "ledger publication: rust tool cache"

#: Every file the scorecard family compiles, as paths in the Hermit checkout.
#: hermit-manifest-plan's lib.rs reaches the hermit-cli files and
#: ci/manifest-plan/src/nextest_binaries.rs the two ci/*.rs files through
#: #[path]; scorecard.rs reaches the prelude the same way. mirror.py binds the
#: scorecard tool commit to the same list (_committed_tool_source); the cache
#: also keys on every Cargo.toml (Source.manifests).
SCORECARD_INPUTS = (
    "ci/compat-envelope/scorecard.rs", "scripts/lib/rust_script_prelude.rs",
    "ci/manifest-plan/Cargo.toml", "ci/manifest-plan/build.rs", "ci/manifest-plan/src",
    "hermit-cli/src/canonical_verdict.rs", "hermit-cli/src/logdiff_report.rs",
    "hermit-cli/tests/common/proc_locks_lease.rs",
    "ci/cargo-guest-binaries.rs", "ci/record-replay-workloads.rs",
    "detcore-model/Cargo.toml", "detcore-model/build.rs", "detcore-model/src",
    "Cargo.toml", "Cargo.lock", ".cargo", "rust-toolchain", "rust-toolchain.toml",
)
#: Every file ci-hub.rs compiles from the dev-hermit checkout. It also
#: depends on hermit-manifest-plan through ../hermit (SCORECARD_INPUTS).
CI_HUB_INPUTS = (
    "ci-hub/ci-hub.rs", "ci-hub/lib",
    "ci-hub/validate/qualifying-receipt.json",
    "ci-hub/validate/retained-evidence-quarantine.json",
)
#: Every Cargo.toml in a repository, as a git pathspec. A workspace build loads
#: the manifest of every workspace member and path dependency, and --locked
#: fails when any of them no longer matches Cargo.lock.
MANIFEST_PATHSPEC = ":(glob)**/Cargo.toml"


class Unavailable(Exception):
    """The cache cannot serve this build; the caller builds on its direct path."""


@dataclasses.dataclass(frozen=True)
class Source:
    """PATHS, committed in the repository at DIRECTORY of the tool checkout.

    With MANIFESTS, also every Cargo.toml committed in that repository.
    """

    directory: str
    paths: tuple[str, ...]
    manifests: bool = False


def status_pathspecs(source: Source) -> tuple[str, ...]:
    """The pathspecs whose uncommitted changes make SOURCE unavailable."""
    return (*source.paths, MANIFEST_PATHSPEC) if source.manifests else source.paths


@dataclasses.dataclass(frozen=True)
class WorkspaceBinary:
    """Binary BIN of package PACKAGE in the tool checkout's Cargo workspace (dev profile)."""

    name: str
    package: str


@dataclasses.dataclass(frozen=True)
class Script:
    """The rust-script at PATH in the tool checkout, built as rust-script builds it (release)."""

    name: str
    path: str


@dataclasses.dataclass(frozen=True)
class Family:
    name: str
    sources: tuple[Source, ...]
    programs: tuple[WorkspaceBinary | Script, ...]


SCORECARD = Family(
    "scorecard",
    (Source(".", SCORECARD_INPUTS, manifests=True),),
    (WorkspaceBinary("hermit-manifest-plan", "hermit-manifest-plan"),
     Script("scorecard", "ci/compat-envelope/scorecard.rs")),
)
CI_HUB = Family(
    "ci-hub",
    (Source(".", CI_HUB_INPUTS), Source("hermit", SCORECARD_INPUTS, manifests=True)),
    (Script("ci-hub", "ci-hub/ci-hub.rs"),),
)


@dataclasses.dataclass(frozen=True)
class Built:
    """The programs of FAMILY for TOOL, stored under KEY in a held slot.

    OUTCOME is "hit" or "built". SCRIPTS maps each script program to its path
    in TOOL and the package name rust-script gives it.
    """

    family: str
    tool: Path
    compiler: str
    slot: Path
    key: str
    outcome: str
    programs: Mapping[str, Path]
    scripts: Mapping[str, tuple[Path, str]]

    def run_environment(self, name: str) -> dict[str, str]:
        """The variables rust-script sets when it runs script NAME.

        ci-hub.rs finds its checkout through RUST_SCRIPT_PATH, so a stored
        program runs with the values the direct path's rust-script would set.
        """
        path, package = self.scripts[name]
        return {"RUST_SCRIPT_PATH": str(path), "RUST_SCRIPT_BASE_PATH": str(path.parent),
                "RUST_SCRIPT_SAFE_NAME": path.stem, "RUST_SCRIPT_PKG_NAME": package}


@dataclasses.dataclass
class _Held:
    family: str
    root: Path
    fd: int
    built: dict[tuple[str, str], Built] = dataclasses.field(default_factory=dict)


@dataclasses.dataclass
class _Session:
    held: dict[Path, _Held] = dataclasses.field(default_factory=dict)
    #: The thread that opened the session; only it prepares or finds programs.
    owner: threading.Thread = dataclasses.field(default_factory=threading.current_thread)
    #: False once the session has ended, so a context copied during it holds nothing.
    open: bool = True
    #: The process that opened the session: a forked child inherits the
    #: context variable, but not the right to use or repoint its slots.
    pid: int = dataclasses.field(default_factory=os.getpid)


#: Per thread (and per asyncio task): two publications in one process do not share slots.
#: A context variable can be carried into another thread (contextvars.copy_context,
#: asyncio.to_thread) or a forked child, so _current() also requires the session
#: to be open and owned by the calling thread and process.
_SESSION: contextvars.ContextVar[_Session | None] = contextvars.ContextVar(
    "rust_tool_cache_session", default=None)


def _current() -> _Session | None:
    current = _SESSION.get()
    if (current is None or not current.open or current.pid != os.getpid()
            or current.owner is not threading.current_thread()):
        return None
    return current


def log(message: str) -> None:
    print(f"{LOG_PREFIX}: {message}", file=sys.stderr, flush=True)


def active() -> bool:
    return _current() is not None


@contextlib.contextmanager
def session() -> Iterator[None]:
    """Hold the slots prepared inside this block until it ends. Nested blocks share the outer one."""
    if _current() is not None:
        yield
        return
    current = _Session()
    token = _SESSION.set(current)
    try:
        yield
    finally:
        current.open = False
        _SESSION.reset(token)
        for held in current.held.values():
            _release(held)


def _release(held: _Held) -> None:
    if held.fd >= 0:
        os.close(held.fd)
        held.fd = -1


def _exec(command: Sequence[str], *, cwd: Path, environment: Mapping[str, str],
          timeout: float = COMMAND_TIMEOUT_SECONDS) -> str:
    """Run COMMAND with exactly ENVIRONMENT; return stdout or raise Unavailable.

    COMMAND runs in a process group of its own. When it times out, or this
    thread is interrupted, the whole group is killed and waited for before
    this returns: a compiler left running would go on writing the slot's
    target directory after the slot is released.
    """
    name = " ".join(command[:3])
    try:
        process = subprocess.Popen(list(command), cwd=cwd, env=dict(environment),
                                   stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True,
                                   start_new_session=True)
    except (OSError, subprocess.SubprocessError) as error:
        raise Unavailable(f"cannot run {name}: {error}") from error
    try:
        stdout, stderr = process.communicate(timeout=timeout)
    except subprocess.TimeoutExpired as error:
        _stop_group(process)
        raise Unavailable(f"{name} did not finish in {timeout:g} s") from error
    except BaseException:
        with contextlib.suppress(Unavailable):
            _stop_group(process)
        raise
    if process.returncode != 0:
        detail = "\n".join((stderr or stdout).strip().splitlines()[-20:])
        raise Unavailable(f"{name} exited {process.returncode}: {detail}")
    return stdout


def _stop_group(process: subprocess.Popen) -> None:
    """Kill PROCESS's process group and wait until no member of it is left."""
    with contextlib.suppress(ProcessLookupError):
        os.killpg(process.pid, signal.SIGKILL)
    process.wait()
    for stream in (process.stdout, process.stderr):
        if stream is not None:
            stream.close()
    # Killed descendants stay in the group until their new parent reaps them.
    deadline = time.monotonic() + GROUP_EXIT_SECONDS
    while True:
        try:
            os.killpg(process.pid, 0)
        except ProcessLookupError:
            return
        if time.monotonic() >= deadline:
            raise Unavailable(f"process group {process.pid} still has members "
                              f"{GROUP_EXIT_SECONDS:g} s after SIGKILL")
        time.sleep(0.05)


def _git(repo: Path, *args: str) -> str:
    try:
        result = subprocess.run(["git", "-C", str(repo), *args], capture_output=True, text=True,
                                check=False, timeout=COMMAND_TIMEOUT_SECONDS)
    except (OSError, subprocess.SubprocessError) as error:
        raise Unavailable(f"cannot run git in {repo}: {error}") from error
    if result.returncode != 0:
        raise Unavailable(f"git {args[0]} failed in {repo}: {result.stderr.strip()}")
    return result.stdout


def committed_listing(tool: Path, source: Source) -> str:
    """The committed tree entries of SOURCE; Unavailable when any of them is uncommitted."""
    repo = tool / source.directory
    # An uninitialized submodule directory would otherwise list the enclosing
    # repository's files under the submodule's paths.
    top = _git(repo, "rev-parse", "--show-toplevel").strip()
    if Path(top).resolve() != repo.resolve():
        raise Unavailable(f"{repo} is not the top of a git checkout (top: {top})")
    status = _git(repo, "status", "--porcelain", "--untracked-files=all", "--",
                  *status_pathspecs(source))
    if status:
        raise Unavailable(f"uncommitted changes in {repo}:\n{status.rstrip()}")
    paths = list(source.paths)
    if source.manifests:
        # git ls-tree takes no glob pathspec; list the manifests by name.
        names = _git(repo, "ls-tree", "-r", "--full-tree", "--name-only", "-z", "HEAD")
        paths += [name for name in names.split("\0")
                  if name == "Cargo.toml" or name.endswith("/Cargo.toml")]
    return _git(repo, "ls-tree", "-r", "--full-tree", "HEAD", "--", *paths)


def _cargo_home(caller: Mapping[str, str]) -> Path:
    configured = caller.get("CARGO_HOME")
    if configured is None:
        if not caller.get("HOME"):
            raise Unavailable("neither CARGO_HOME nor HOME is set")
        configured = str(Path(caller["HOME"]) / ".cargo")
    home = Path(configured)
    if not home.is_absolute() or not home.is_dir():
        raise Unavailable(f"CARGO_HOME {configured!r} is not an absolute directory")
    return home.resolve()


def fixed_environment(slot: Path, compiler: str, caller: Mapping[str, str]) -> dict[str, str]:
    """The complete environment of every probe and build in SLOT."""
    if not caller.get("PATH") or not caller.get("HOME"):
        raise Unavailable("PATH and HOME are required")
    environment = {name: caller[name] for name in INHERITED if caller.get(name)}
    environment.update({
        "CARGO_HOME": str(slot / "cargo-home"), "RUSTUP_TOOLCHAIN": compiler,
        "RUSTUP_AUTO_INSTALL": "0", CHECKSUM_FRESHNESS_ENV: "true",
    })
    return environment


def cargo_config_files(build_directory: Path, cargo_home: Path, tool: Path) -> dict[str, str]:
    """sha256 of each Cargo configuration file a build in BUILD_DIRECTORY loads.

    Cargo reads `.cargo/config.toml` and the legacy `.cargo/config` in the
    current directory and each ancestor, then those in CARGO_HOME. Files in the
    tool checkout are named relative to it, so another checkout of the same
    content has the same key. A CARGO_HOME that is not also an ancestor's
    `.cargo` is read last, so its files are named relative to it too: validate
    gives every run a fresh CARGO_HOME, and the builds see it only as the
    slot's stable `cargo-home` link.
    """
    physical = build_directory.resolve()
    tool = tool.resolve()
    home = cargo_home.resolve()
    candidates = [(directory / ".cargo", False) for directory in (physical, *physical.parents)]
    if all(directory.resolve() != home for directory, _ in candidates):
        candidates.append((home, True))
    found: dict[str, str] = {}
    for directory, appended_home in candidates:
        for name in ("config", "config.toml"):
            path = directory / name
            if not path.is_file():
                continue
            resolved = path.resolve()
            if resolved.is_relative_to(tool):
                label = f"tool/{resolved.relative_to(tool)}"
            elif appended_home:
                label = f"cargo-home/{name}"
            else:
                label = str(resolved)
            found[label] = hashlib.sha256(resolved.read_bytes()).hexdigest()
    return found


def _point(link: Path, target: Path) -> None:
    """Make LINK a symlink to TARGET, replacing it atomically."""
    temporary = link.with_name(f".{link.name}.{os.getpid()}.new")
    with contextlib.suppress(FileNotFoundError):
        temporary.unlink()
    os.symlink(target, temporary)
    os.replace(temporary, link)


def _try_lock(path: Path) -> int | None:
    fd = os.open(path, os.O_RDWR | os.O_CREAT | os.O_CLOEXEC, 0o644)
    try:
        fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
    except BlockingIOError:
        os.close(fd)
        return None
    except BaseException:
        os.close(fd)
        raise
    return fd


def _acquire(family: str, family_root: Path) -> _Held:
    family_root.mkdir(parents=True, exist_ok=True)
    deadline = time.monotonic() + LOCK_WAIT_SECONDS
    while True:
        for index in range(SLOT_COUNT):
            root = family_root / str(index)
            root.mkdir(exist_ok=True)
            fd = _try_lock(root / "lock")
            if fd is not None:
                return _Held(family=family, root=root, fd=fd)
        if time.monotonic() >= deadline:
            raise Unavailable(f"all {SLOT_COUNT} slots of {family_root} stayed busy for "
                              f"{LOCK_WAIT_SECONDS:g} s")
        time.sleep(POLL_SECONDS)


def _size(root: Path) -> int:
    total = 0
    for directory, _, files in os.walk(root):
        for name in files:
            with contextlib.suppress(FileNotFoundError):
                total += os.lstat(os.path.join(directory, name)).st_blocks * 512
    return total


def _host(rustc_version: str) -> str:
    for line in rustc_version.splitlines():
        if line.startswith("host: "):
            return line.removeprefix("host: ").strip()
    raise Unavailable("rustc -vV names no host")


def _executables(stdout: str) -> dict[str, Path]:
    """The executables `cargo build --message-format=json` reports, by target name."""
    found: dict[str, Path] = {}
    for line in stdout.splitlines():
        if not line.startswith("{"):
            continue
        message = json.loads(line)
        if message.get("reason") == "compiler-artifact" and message.get("executable"):
            found[message["target"]["name"]] = Path(message["executable"])
    return found


def _script_binary(manifest: bytes) -> str:
    """The name of the one [[bin]] target in the package rust-script generated."""
    try:
        bins = tomllib.loads(manifest.decode())["bin"]
    except (KeyError, UnicodeError, tomllib.TOMLDecodeError) as error:
        raise Unavailable(f"cannot read the generated script manifest: {error!r}") from error
    if len(bins) != 1:
        raise Unavailable(f"the generated script manifest names {len(bins)} binaries, not one")
    return bins[0]["name"]


def script_package_name(path: str) -> str:
    """RUST_SCRIPT_PKG_NAME as rust-script 0.36 derives it from the script's file stem."""
    name = []
    for index, character in enumerate(Path(path).stem):
        if index == 0 and character.isascii() and character.isdigit():
            name.append("_" + character)
        elif character.isascii() and (character.isdigit() or character.islower() or character in "_-"):
            name.append(character)
        elif character.isascii() and character.isupper():
            name.append(character.lower())
        else:
            name.append("_")
    return "".join(name)


def _manifest(slot: Path, program: WorkspaceBinary | Script) -> Path:
    if isinstance(program, WorkspaceBinary):
        return slot / "tool/Cargo.toml"
    return slot / "pkg" / program.name / "Cargo.toml"


def _commands(slot: Path, program: WorkspaceBinary | Script) -> list[str]:
    common = ["--locked", "--offline", "--manifest-path", str(_manifest(slot, program)),
              "--target-dir", str(slot / "target"), "--message-format=json-render-diagnostics"]
    if isinstance(program, WorkspaceBinary):
        return ["cargo", "build", "-p", program.package, "--bin", program.name, *common]
    # rust-script builds a script with `cargo build --release`.
    return ["cargo", "build", "--release", *common]


def _complete(entry: Path, family: Family) -> bool:
    return (entry / "inputs.json").is_file() and all(
        (entry / program.name).is_file() for program in family.programs)


def _evict(store: Path, keep: str) -> None:
    """Keep the STORE_ENTRIES most recently used entries, KEEP among them."""
    for stale in store.glob(".tmp-*"):
        shutil.rmtree(stale, ignore_errors=True)
    others = sorted((entry for entry in store.iterdir() if entry.name != keep and entry.is_dir()),
                    key=lambda entry: entry.stat().st_mtime, reverse=True)
    for entry in others[STORE_ENTRIES - 1:]:
        shutil.rmtree(entry, ignore_errors=True)


def prepare(family: Family, state_root: Path, tool: Path, compiler: str, *,
            net_wrapper: str, environment: Mapping[str, str] | None = None) -> Built:
    """Return FAMILY's programs for TOOL and COMPILER, building them on a miss.

    Requires a session(); the slot stays held, and its `tool` link on TOOL,
    until the session ends. Raises Unavailable (or any other error) when the
    cache cannot serve; the caller then builds on its direct path.
    """
    current = _current()
    if current is None:
        raise Unavailable("no rust tool cache session")
    started = time.monotonic()
    tool = tool.resolve()
    caller = dict(os.environ if environment is None else environment)
    listings = {source.directory: committed_listing(tool, source) for source in family.sources}
    if any(isinstance(program, WorkspaceBinary) for program in family.programs) and not any(
            line.endswith("\tCargo.lock") for line in listings["."].splitlines()):
        raise Unavailable(f"{tool} commits no Cargo.lock for its --locked workspace build")
    cargo_home = _cargo_home(caller)
    family_root = state_root.resolve() / CACHE_RELATIVE / family.name
    held = current.held.get(family_root)
    acquired = held is None
    toolchain = None
    if held is None:
        held = _acquire(family.name, family_root)
        try:
            held, toolchain = _warm_slot(family_root, held, tool, compiler, cargo_home, caller)
        except BaseException:
            _release(held)
            raise
    try:
        held.built.clear()
        built = _prepare_in_slot(family, held.root, tool, compiler, listings, cargo_home, caller,
                                 net_wrapper, toolchain=toolchain)
    except BaseException:
        if acquired:
            _release(held)
        raise
    held.built[(str(tool), compiler)] = built
    current.held[family_root] = held
    log(f"{family.name} {built.outcome} {built.key[:16]} in {held.root} "
        f"({time.monotonic() - started:.1f} s)")
    return built


def prepared(family: Family, tool: Path, compiler: str) -> Built | None:
    """What prepare() returned for TOOL and COMPILER in this session, while its slot still serves it."""
    current = _current()
    if current is None:
        return None
    tool = tool.resolve()
    for held in current.held.values():
        built = held.built.get((str(tool), compiler))
        if held.family != family.name or held.fd < 0 or built is None:
            continue
        if os.readlink(held.root / "tool") == str(tool) and all(
                path.is_file() for path in built.programs.values()):
            return built
    return None


def _warm_slot(family_root: Path, held: _Held, tool: Path, compiler: str, cargo_home: Path,
               caller: Mapping[str, str]) -> tuple[_Held, dict[str, str]]:
    """Trade HELD for a free slot whose target this caller's toolchain built.

    Callers with different toolchains or Cargo configurations share the
    family's slots, and a slot's target directory is emptied when the next
    build's identity differs (_match_target_to). Moving to a free slot that
    already carries the caller's identity keeps each identity's target warm
    instead of letting them evict each other: on 2026-10-10 two validates'
    cell-ledger publications each spent 125 s of their 529 s bound rebuilding
    the scorecard tool from an emptied target, and both were killed at the
    bound. Returns the slot to use and the toolchain probed for it, which the
    caller reuses rather than probing again.
    """
    _point(held.root / "tool", tool)
    _point(held.root / "cargo-home", cargo_home)
    toolchain = _toolchain(held.root / "tool", fixed_environment(held.root, compiler, caller))
    wanted = _target_identity(toolchain, cargo_config_files(held.root / "tool", cargo_home, tool))
    if _recorded_identity(held.root) in (wanted, None):
        return held, toolchain
    # A slot built under this identity, else one never built, before emptying HELD's.
    for acceptable in (wanted, None):
        for index in range(SLOT_COUNT):
            root = family_root / str(index)
            root.mkdir(exist_ok=True)
            if root == held.root or _recorded_identity(root) != acceptable:
                continue
            fd = _try_lock(root / "lock")
            if fd is not None:
                _release(held)
                return _Held(family=held.family, root=root, fd=fd), toolchain
    return held, toolchain


def _prepare_in_slot(family: Family, slot: Path, tool: Path, compiler: str,
                     listings: Mapping[str, str], cargo_home: Path, caller: Mapping[str, str],
                     net_wrapper: str, toolchain: dict[str, str] | None = None) -> Built:
    _point(slot / "tool", tool)
    _point(slot / "cargo-home", cargo_home)
    environment = fixed_environment(slot, compiler, caller)
    build_directory = slot / "tool"
    if toolchain is None:
        toolchain = _toolchain(build_directory, environment)
    host = _host(toolchain["rustc"])
    manifests: dict[str, dict[str, str]] = {}
    binaries: dict[str, str] = {}
    scripts: dict[str, tuple[Path, str]] = {}
    for program in family.programs:
        if isinstance(program, WorkspaceBinary):
            binaries[program.name] = program.name
            continue
        package = slot / "pkg" / program.name
        shutil.rmtree(package, ignore_errors=True)
        package.parent.mkdir(exist_ok=True)
        _exec(["rust-script", "--package", "--pkg-path", str(package),
               str(slot / "tool" / program.path)],
              cwd=build_directory, environment={**environment, "XDG_CACHE_HOME": str(slot / "xdg")})
        # A fresh rust-script package has no lock file: Cargo resolves the
        # newest compatible versions, as the direct path's build does.
        _exec([net_wrapper, "cargo", "generate-lockfile", "--manifest-path",
               str(package / "Cargo.toml")], cwd=build_directory, environment=environment)
        manifests[program.name] = _package_digests(package)
        binaries[program.name] = _script_binary((package / "Cargo.toml").read_bytes())
        scripts[program.name] = (tool / program.path, script_package_name(program.path))
    commands = {program.name: _commands(slot, program) for program in family.programs}
    document = {
        "schema": SCHEMA, "family": family.name, "slot": str(slot),
        "build_directory": str(build_directory), "commands": commands,
        "sources": dict(listings), "script_manifests": manifests, "toolchain": toolchain,
        "cargo_config": cargo_config_files(build_directory, cargo_home, tool),
        "environment": environment,
    }
    encoded = json.dumps(document, sort_keys=True, indent=1).encode() + b"\n"
    key = hashlib.sha256(encoded).hexdigest()
    store = slot / "store"
    entry = store / key
    outcome = "hit"
    if _complete(entry, family):
        os.utime(entry)
    else:
        outcome = "built"
        _match_target_to(slot, document)
        built = _build(family, slot, environment, net_wrapper, host, commands, binaries)
        # The build read whatever the inputs were while it ran: a commit, an
        # edit, a configuration or toolchain change during it leaves programs
        # that match no key.
        after = {**document,
                 "sources": {source.directory: committed_listing(tool, source)
                             for source in family.sources},
                 "script_manifests": {name: _package_digests(slot / "pkg" / name)
                                      for name in manifests},
                 "toolchain": _toolchain(build_directory, environment),
                 "cargo_config": cargo_config_files(build_directory, cargo_home, tool)}
        changed = sorted(name for name in document if after[name] != document[name])
        if changed:
            raise Unavailable(f"{', '.join(changed)} changed during the build; not storing it")
        store.mkdir(exist_ok=True)
        staging = store / f".tmp-{os.getpid()}-{key[:16]}"
        shutil.rmtree(staging, ignore_errors=True)
        staging.mkdir()
        for name, path in built.items():
            _exec(["cp", "--reflink=auto", "--preserve=mode", str(path), str(staging / name)],
                  cwd=slot, environment=environment)
        (staging / "inputs.json").write_bytes(encoded)
        shutil.rmtree(entry, ignore_errors=True)
        os.rename(staging, entry)
        _evict(store, key)
    return Built(family=family.name, tool=tool, compiler=compiler, slot=slot, key=key,
                 outcome=outcome,
                 programs={program.name: entry / program.name for program in family.programs},
                 scripts=scripts)


def _toolchain(build_directory: Path, environment: Mapping[str, str]) -> dict[str, str]:
    return {
        "rustc": _exec(["rustc", "-vV"], cwd=build_directory, environment=environment),
        "cargo": _exec(["cargo", "-vV"], cwd=build_directory, environment=environment),
        "cc": _exec(["cc", "--version"], cwd=build_directory, environment=environment),
    }


def _package_digests(package: Path) -> dict[str, str]:
    """sha256 of the Cargo.toml and Cargo.lock of a generated script package."""
    return {name: hashlib.sha256((package / name).read_bytes()).hexdigest()
            for name in ("Cargo.toml", "Cargo.lock")}


def _target_identity(toolchain: object, cargo_config: object) -> str:
    """What a slot's target directory was built under: toolchain and Cargo configuration."""
    return hashlib.sha256(json.dumps(
        {"toolchain": toolchain, "cargo_config": cargo_config}, sort_keys=True).encode()
    ).hexdigest()


def _recorded_identity(slot: Path) -> str | None:
    try:
        return (slot / "target-identity").read_text().strip()
    except FileNotFoundError:
        return None


def _match_target_to(slot: Path, document: Mapping[str, object]) -> None:
    """Empty SLOT's target directory unless DOCUMENT's toolchain and configuration built it.

    Cargo does not fingerprint the linker (`cc`) or the C compiler a build
    script runs: after either changes it would keep artifacts that a cold
    build would produce differently.
    """
    identity = _target_identity(document["toolchain"], document["cargo_config"])
    marker = slot / "target-identity"
    if _recorded_identity(slot) == identity:
        return
    target = slot / "target"
    if target.exists():
        log(f"emptying {target}: built with another toolchain or Cargo configuration")
        shutil.rmtree(target)
    marker.write_text(identity + "\n")


def _build(family: Family, slot: Path, environment: Mapping[str, str],
           net_wrapper: str, host: str, commands: Mapping[str, list[str]],
           binaries: Mapping[str, str]) -> dict[str, Path]:
    """Build every program of FAMILY in SLOT; return the executables Cargo reports."""
    target = slot / "target"
    if target.exists() and _size(target) > TARGET_BUDGET_BYTES:
        log(f"{family.name}: emptying {target}, over {TARGET_BUDGET_BYTES} bytes")
        shutil.rmtree(target)
    build_directory = slot / "tool"
    built: dict[str, Path] = {}
    for program in family.programs:
        _exec([net_wrapper, "cargo", "fetch", "--locked", "--target", host, "--manifest-path",
               str(_manifest(slot, program))], cwd=build_directory, environment=environment)
        stdout = _exec(commands[program.name], cwd=build_directory, environment=environment)
        executable = _executables(stdout).get(binaries[program.name])
        if executable is None or not executable.is_file():
            raise Unavailable(f"cargo reported no executable {binaries[program.name]!r} for "
                              f"{program.name}")
        built[program.name] = executable
    return built
