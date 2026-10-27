#!/usr/bin/env python3
"""Publish cell history directly to the existing hermit_test_ledger repository.

The historical command name is retained for callers. Validation receipts stay
in dev-hermit; cell series batches stay in its ignored spool until a freshly
fetched ledger commit proves publication. One persistent linked worktree owns
ledger writes. A scorecard failure does not undo published rows or change the
validation verdict, and leaves the writer and the spooled rows available for
recovery. The renderer's own partial output is discarded with its copy (below);
it is a function of the published series commit and the measured tree, so it
is regenerated rather than recovered.

The scorecard is rendered by the measured checkout's own scorecard.rs, so a
renderer change travels with its SCORECARD.md. That renderer is code under
validation, so it never receives the writer: it runs against a throwaway copy
of the ledger at the published series commit, without the network wrapper,
proxy variables, GitHub credential variables, or the user's and system's Git
configuration (the fleet's Git proxy and credential helper live there), and
only its exact generated files are copied back (_render_copy). Every ledger commit is then checked to sit directly
on that series commit and to change only the paths being published before it is
pushed (_commit_and_push). This is not an operating-system sandbox: the
renderer runs as the same Unix user, so a hostile renderer could still reach the
writer path, read that user's stored credentials, or rebuild a network route
itself. These checks stop the accidental
and the in-band routes (writing through DEV_HERMIT_TEST_LEDGER_ROOT, committing
in the ledger it was given, planting files beside a carrier), not a deliberate
escape.

The renderer's detailed history, scorecard/cells.json, outgrew the largest
file GitHub accepts, so the ledger stores it in parts with a manifest
(scorecard_cells). The renderer never sees the parts: its copy shows it the
single file, and _copy_back splits the file it writes before anything reaches
the writer.

A derived portable proof is independent of that scorecard and publishes even
when the scorecard fails or its head order cannot be decided. The public
scorecard head (latest.json and the rendered "Last regenerated" notice) only
moves forward: an older head publishes its rows and proof but not a scorecard.
The head is not required to be on Hermit main (see scorecard_order).
"""

from __future__ import annotations

import argparse
import array
import contextlib
import errno
import fcntl
import functools
import hashlib
import io
import json
import math
import os
import re
import shutil
import signal
import stat
import subprocess
import sys
import tempfile
import time
from datetime import datetime, timezone
from pathlib import Path
from typing import Any, Callable, Iterable, Iterator, Mapping, NamedTuple, Sequence

TOOL_ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(TOOL_ROOT / "ci-hub" / "lib"))
sys.path.insert(0, str(TOOL_ROOT / "ci-hub" / "series"))
sys.path.insert(0, str(TOOL_ROOT / "ci-hub" / "ledger"))
sys.path.insert(0, str(TOOL_ROOT / "ci-hub" / "validate"))
import git_env  # noqa: E402
import ledger_publication_outcome  # noqa: E402
import rust_toolchain  # noqa: E402
import series  # noqa: E402
import publisher  # noqa: E402
import ledger_snapshot  # noqa: E402
import finalized_proof  # noqa: E402
import rust_tool_cache  # noqa: E402
import scorecard_coalesce  # noqa: E402
import unwrapped_git  # noqa: E402
import scorecard_cells  # noqa: E402
import ledger_retention  # noqa: E402
import retention_render  # noqa: E402

REMOTE_URL = series.LEDGER_REPOSITORY
REMOTE_BRANCH = "main"
METADATA_SCHEMA = "hermit-test-ledger/v1"
SCORECARD_HEAD_TIME_FIELD = "scorecard_hermit_commit_time"
_COMMIT = re.compile(r"[0-9a-f]{40}")
MIRRORED_SCORECARD_PATHS = (Path("scorecard/SCORECARD.md"), Path("scorecard/cells.json"))
#: Generated files a scorecard tool may or may not write. A tool that predates
#: the parity section writes no ``scorecard/parity.json``; it is bound into
#: latest.json only when this renderer wrote it, and its absence is never an
#: error (https://github.com/rrnewton/hermit/issues/3301).
OPTIONAL_SCORECARD_PATHS = (Path("scorecard/parity.json"),)
#: The messages of the derived commits this tool makes: a finalized-proof
#: attachment, alone or after a scorecard that failed, was superseded or was
#: deferred, and a regenerated scorecard. Each is computed from already
#: published rows and a run's retained results; none carries a row.
#: set_aside_stranded_derived_commit recognizes a commit left in the writer by
#: exactly these messages.
PROOF_COMMIT_MESSAGE = "Attach verified finalized comparison operands"
PROOF_SUPERSEDED_COMMIT_MESSAGE = f"{PROOF_COMMIT_MESSAGE}; scorecard superseded"
PROOF_DEFERRED_COMMIT_MESSAGE = f"{PROOF_COMMIT_MESSAGE}; scorecard deferred"
PROOF_FALLBACK_COMMIT_MESSAGE = f"{PROOF_COMMIT_MESSAGE}; scorecard generation failed"
SCORECARD_COMMIT_MESSAGE = "Generate scorecard from published cell history"
PROOF_COMMIT_MESSAGES = frozenset({
    PROOF_COMMIT_MESSAGE, PROOF_SUPERSEDED_COMMIT_MESSAGE, PROOF_DEFERRED_COMMIT_MESSAGE,
    PROOF_FALLBACK_COMMIT_MESSAGE,
})
#: A finalized run's publication does not regenerate the scorecard when the
#: last regeneration on the ledger's main branch (the newest commit there that
#: changed latest.json) is younger than this; its rows and finalized carrier
#: still publish, and the scorecard records the outcome "deferred". Every
#: render projects every published series row its history does not already
#: represent (scorecard.rs project_observations), so the next render picks the
#: deferred run's rows up. Measured 2026-10-11: 41 regenerations in 24 hours,
#: each storing about 1.37 GiB of new scorecard/cells/ parts, about 55 GiB a
#: day in the ledger's object store; at most one per two hours is at most 12.
#: Replaying four consecutive regenerations showed that no part layout helps:
#: about 2,000 cells change on every regeneration and they hold about 925 MiB,
#: because each cell carries its whole observation history. A render that is
#: asked for explicitly (a scorecard command, or a publication without
#: --finalized-run-id, which projects the series alone) is never deferred.
SCORECARD_REGENERATION_INTERVAL_SECONDS = 2 * 60 * 60
#: A render left to a queued publication (queued_deferral) is never deferred
#: again once the last regeneration, or any pending deferral, is this much older
#: than the regeneration interval: the holder renders whoever is waiting. Read
#: from durable timestamps (latest.json's commit time, the markers'
#: deferred_at_epoch), never from the queue, so continuous load cannot keep the
#: scorecard stale (coalesce_staleness_cap_seconds).
COALESCE_CAP_MARGIN_SECONDS = 60 * 60
#: The longest one publication unit holds publication.lock: start_unit's
#: PUBLICATION_STARTUP_RESERVE_SECONDS (300) + this tool's lifecycle (2,400) +
#: PUBLICATION_TAIL_SECONDS (300, commit status); a test holds them equal.
PUBLICATION_HOLD_SECONDS = 300 + 2_400 + 300
PARITY_PUBLICATION_SCHEMA = "parity-publication/v1"
#: Store each regenerated scorecard history's parts in a parentless commit on
#: scorecard_cells.HISTORY_REF, replaced at every regeneration, and only the
#: external manifest naming it on main (scorecard_cells, "external" layout).
#: Storage audit 2026-10-12 (ai_docs/2026-10-12-ledger-and-site-storage-audit.md):
#: the parts on main were 8.49 GiB of the 9.69 GiB packed ledger and grew by
#: about 103 MB on disk per regeneration; no page of the site reads them. A
#: mirror.py from before the external layout refuses a v2 manifest, so every
#: publishing host must run a mirror.py that reads it (dev-hermit 1b8bd766)
#: before this writes one.
EXTERNAL_HISTORY = True
#: Fetches the history ref beside main; a glob refspec, so a remote that has
#: no history ref yet is not an error.
HISTORY_FETCH_REFSPEC = "+refs/heads/scorecard-history*:refs/remotes/origin/scorecard-history*"
WRITER_RELATIVE = Path("ignored/ci-hub/test-ledger-writer")
#: This host's receipts that a finalized run's scorecard was rendered here and
#: its transaction reached origin/main (write_completion_receipt). Beside the
#: writer, not in it: the writer refuses any file the ledger does not track.
RECEIPTS_RELATIVE = ledger_publication_outcome.RECEIPTS_RELATIVE
RECEIPT_SCHEMA = ledger_publication_outcome.RECEIPT_SCHEMA
SUPERSEDED_RECEIPT_SCHEMA = ledger_publication_outcome.SUPERSEDED_RECEIPT_SCHEMA
DEFERRED_RECEIPT_SCHEMA = ledger_publication_outcome.DEFERRED_RECEIPT_SCHEMA
_RECEIPT_MAX_BYTES = ledger_publication_outcome.RECEIPT_MAX_BYTES
#: write_carrier_receipt reads each carrier file back from its commit; no
#: carrier or operand stage_carrier writes approaches this.
_SUPERSEDED_FILE_MAX_BYTES = 256 << 20
PUBLICATION_LOCK = "ci-hub-series-publication.lock"
LOCK_FD_ENV = "DEV_HERMIT_TEST_LEDGER_PUBLISH_LOCK_FD"
DEFAULT_TIMEOUT_SECONDS = 900
#: Ceiling on one whole publication: preparation, lock wait, raw append,
#: scorecard render, proof, commit and push. Sized from measurement on
#: 2026-10-03, against hermit_test_ledger 7faf82bd (216,929 series rows):
#: about 262 s before the render (shared checker, proof preparation, renderer
#: compile, lock, raw append, quarantine append), 1,066 s for the render itself
#: (project-and-observe-results, 1,610 cells), and about 56 s after it (derived
#: proof and push), about 1,390 s in all; 2,400 s is 1.7 times that. It was
#: 1,800 s. The in-validation caller runs inside validate-lock's 4,800 s child
#: deadline and starts this publication about 1,200-1,260 s into the run, so
#: 2,400 s still leaves over 1,000 s for slower tests. The render rebuilds
#: its projection from every row, and the ledger grew by 42,299 rows in the
#: 13 hours before that measurement, so this buys a day or two, not weeks;
#: the real fix is an incremental projection in scorecard.rs:
#: https://github.com/rrnewton/hermit/issues/3640.
LIFECYCLE_TIMEOUT_SECONDS = 2_400
#: What the scorecard render must leave of the lifecycle for the work after
#: it: copying and hashing the generated files, the finalized proof (a derived
#: census emission when the scorecard fails), latest.json, commit and push.
#: That took about 56 s after a failed render on 2026-10-03; 240 s also covers
#: the larger push of a successful render. When less than twice this is left
#: (a short caller deadline such as the 600 s retirement path), the render and
#: the work after it split what remains evenly, so a quick render still runs.
#: The render itself has no separate fixed bound: it was the 900 s
#: DEFAULT_TIMEOUT_SECONDS until 2026-10-03, which a 1,066 s render could never
#: meet inside a lifecycle that had room for it.
SCORECARD_RESERVE_SECONDS = 240
DEFAULT_ATTEMPTS = 3
SCORECARD_COMMANDS = (
    "observe-results", "import-results", "update-observations",
    "project-observations", "project-and-observe-results", "export-legacy",
)
# The renderer runs after prepare_scorecard fetched its dependencies, against a
# ledger copy it cannot push from. None of these ambient routes or credentials
# is needed, so none is passed on. Git configuration injected through the
# environment is withheld too; build_scorecard also replaces the user's and the
# system's Git configuration, which on the fleet hosts carry the forward proxy
# and the GitHub credential helper.
RENDERER_WITHHELD_ENV = (
    "http_proxy", "https_proxy", "ftp_proxy", "all_proxy",
    "HTTP_PROXY", "HTTPS_PROXY", "FTP_PROXY", "ALL_PROXY",
    "GH_TOKEN", "GITHUB_TOKEN", "GH_ENTERPRISE_TOKEN", "GITHUB_ENTERPRISE_TOKEN",
    "GIT_ASKPASS", "SSH_ASKPASS", "SSH_AUTH_SOCK", "FLEET_REAL_GH",
    "GIT_CONFIG_COUNT", "GIT_CONFIG_PARAMETERS",
    # Either would hide the render copy's replacement view of the published
    # scorecard history (scorecard_cells.install_render_view) from Git.
    "GIT_NO_REPLACE_OBJECTS", "GIT_REPLACE_REF_BASE",
)
# Every Git command on the render copy runs with the renderer's configuration:
# no user or system file, so core.useReplaceRefs cannot be turned off there.
COPY_GIT_ENV = {"GIT_CONFIG_GLOBAL": os.devnull, "GIT_CONFIG_NOSYSTEM": "1"}
RENDER_COPY_PREFIX = "test-ledger-render-"


class MirrorRefused(RuntimeError):
    """Publication is incomplete; local inputs must be retained."""


class SeriesPublicationFailed(MirrorRefused):
    """The series rows did not publish; ``quarantine`` says what the refusals did.

    The refused inputs are published whatever happens to the rows, so this
    failure still carries their machine-readable count for the run record.
    It also carries the parity outcome and ``parity_deferred``, the replay of
    the kept append-parity calls that ran before the rows failed: the
    backlog's parity store is judged by them, not by the failed rows.
    """

    def __init__(self, message: str, quarantine: dict[str, Any],
                 parity: dict[str, Any] | None = None,
                 parity_deferred: dict[str, Any] | None = None):
        super().__init__(message)
        self.quarantine = quarantine
        self.parity = parity
        self.parity_deferred = parity_deferred


def _quote_config(text: str) -> str:
    """TEXT quoted as Git quotes a GIT_CONFIG_PARAMETERS word (sq_quote_buf)."""
    return "'" + text.replace("'", "'\\''").replace("!", "'\\!'") + "'"


def _append_config(environment: dict[str, str], settings: Sequence[tuple[str, str]]) -> None:
    """Add SETTINGS to ENVIRONMENT's GIT_CONFIG_PARAMETERS, after any already there.

    Git reads these after every configuration file and GIT_CONFIG_COUNT, and
    the last value of a key wins, so each setting overrides any other source.
    """
    words = " ".join(f"{_quote_config(key)}={_quote_config(value)}" for key, value in settings)
    existing = environment.get("GIT_CONFIG_PARAMETERS")
    environment["GIT_CONFIG_PARAMETERS"] = f"{existing} {words}" if existing else words


def _environment(extra: Mapping[str, str] | None = None,
                 withhold: Sequence[str] = ()) -> dict[str, str]:
    environment = git_env.sanitized_git_env()
    # Under core.ignoreStat, Git sets the assume-unchanged bit itself on each
    # index entry a checkout, add or refresh writes, and `git add` then stages
    # nothing there (_clear_index_bits). No Git command run from here sets it.
    # A command that withholds GIT_CONFIG_PARAMETERS (the renderer) loses
    # this too; it never touches the writer.
    _append_config(environment, (("core.ignoreStat", "false"),))
    # GIT_CONFIG names the one file `git config` reads, as --file would, and
    # no other Git command reads it: the filter-driver census (git config)
    # would then not see a driver that a checkout or diff runs.
    environment.pop("GIT_CONFIG", None)
    for name in withhold:
        environment.pop(name, None)
    environment.setdefault("GIT_AUTHOR_NAME", "Hermit ledger publisher")
    environment.setdefault("GIT_AUTHOR_EMAIL", "hermit-ledger-publisher@example.invalid")
    environment.setdefault("GIT_COMMITTER_NAME", environment["GIT_AUTHOR_NAME"])
    environment.setdefault("GIT_COMMITTER_EMAIL", environment["GIT_AUTHOR_EMAIL"])
    # Only the locked scorecard invocation receives write authority.
    environment.pop(LOCK_FD_ENV, None)
    environment.pop(series.LEDGER_ROOT_ENV, None)
    if extra:
        environment.update(extra)
    return environment


def _run(
    command: Sequence[str], *, cwd: Path | None = None, check: bool = True,
    timeout: float = DEFAULT_TIMEOUT_SECONDS, environment: Mapping[str, str] | None = None,
    pass_fds: tuple[int, ...] = (), withhold: Sequence[str] = (),
) -> subprocess.CompletedProcess[str]:
    try:
        result = subprocess.run(
            list(command), cwd=cwd, capture_output=True, text=True, check=False,
            timeout=timeout, env=_environment(environment, withhold), pass_fds=pass_fds,
        )
    except (OSError, subprocess.SubprocessError) as error:
        raise MirrorRefused(f"cannot run {command[0]}: {error}") from error
    if check and result.returncode != 0:
        detail = (result.stderr or result.stdout).strip() or f"exit {result.returncode}"
        raise MirrorRefused(f"{command[0]} failed: {detail}")
    return result


# Run-private directories for the scorecard's unwrapped git, under the state
# root; one per publication process (see _scorecard_git).
UNWRAPPED_GIT_RELATIVE = Path("ignored/ci-hub/unwrapped-git")


@contextlib.contextmanager
def _scorecard_git(
    state_root: Path | None, environment: dict[str, str], *,
    real_git: Path | None = None, wrapper_config: str | os.PathLike[str] | None = None,
) -> Iterator[unwrapped_git.GitSubstitution | None]:
    """Run the scorecard renderer's git as the binary the host's telemetry
    wrapper execs: the same substitution validate steps get
    (ci-hub/validate/unwrapped_git.py, which documents what it gives up).

    One projection of the published store ran 6,020 git commands through the
    wrapper on 2026-10-07 (strace), which fanned out into 29,695 git
    processes and 4,833 detached telemetry loggers; a wrapped call cost
    14.8 ms against 2.3 ms unwrapped. The renderer's git commands read the
    local ledger checkout; DEV_HERMIT_NETWORK_GIT names the wrapper for any
    command that uses the network, as for the steps. Only the renderer gets
    the substituted PATH: this process's own fetch and push keep the
    wrapper and its HTTP 429 retry. When a check fails, PATH is unchanged.
    The decision is logged in one line, and the directory is removed when the
    renderer is done.
    """
    if state_root is None:
        yield None
        return
    inherited = _environment(environment)
    directory = state_root / UNWRAPPED_GIT_RELATIVE / f"mirror-{os.getpid()}"
    substitution = unwrapped_git.plan(
        inherited.get("PATH", ""), directory, real_git=real_git,
        wrapper_config=wrapper_config, env=inherited,
    )
    substitution = unwrapped_git.materialize(substitution)
    print(f"ledger publication: scorecard {substitution.summary()}", file=sys.stderr)
    if substitution.applied:
        environment["PATH"] = substitution.path
        if substitution.resolved_git is not None:
            environment[unwrapped_git.NETWORK_GIT_ENV] = substitution.resolved_git
    try:
        yield substitution
    finally:
        if substitution.applied and substitution.directory is not None:
            problem = unwrapped_git.remove(Path(substitution.directory), substitution.real_git)
            if problem is not None:
                print(f"ledger publication: scorecard git directory: {problem}", file=sys.stderr)


def scorecard_timeout(lifecycle_deadline_ns: int | None) -> float:
    """Seconds the scorecard renderer may run before it is stopped.

    Everything the publication lifecycle has left, less the reserve for the
    proof, commit and push after the render (or half of what is left, when
    that is smaller), so a slow render is stopped with a report and the
    derived proof still attaches, instead of the whole worker being killed at
    the deadline. A direct library caller with no lifecycle deadline gets the
    same share of a full lifecycle.
    """
    if lifecycle_deadline_ns is None:
        return float(LIFECYCLE_TIMEOUT_SECONDS - SCORECARD_RESERVE_SECONDS)
    remaining = (lifecycle_deadline_ns - time.monotonic_ns()) / 1e9
    if remaining <= 0:
        raise MirrorRefused(
            "publication lifecycle deadline passed before the scorecard render; the renderer "
            "was not started")
    return remaining - min(float(SCORECARD_RESERVE_SECONDS), remaining / 2)


def _git(repo: Path, *args: str, check: bool = True) -> str:
    return _run(["git", "-C", str(repo), *args], check=check).stdout.strip()


def _network(command: Sequence[str], *, cwd: Path | None, wrapper: str, **kwargs: Any):
    return _run([wrapper, *command], cwd=cwd, **kwargs)


def submodule_remote(parent: Path) -> str:
    state = series.shared_parent_root(parent)
    configured = _run([
        "git", "config", "--file", str(state / ".gitmodules"),
        "--get", "submodule.hermit_test_ledger.url",
    ]).stdout.strip()
    if configured != REMOTE_URL:
        raise MirrorRefused(f"hermit_test_ledger submodule URL is absent or unexpected: {configured!r}")
    return configured


def _worktree_changes(repo: Path, *, environment: Mapping[str, str] | None = None,
                      withhold: Sequence[str] = ()) -> dict[str, str]:
    """Map every staged, unstaged or untracked path to its two-letter status.

    A rename or copy names two paths; both are reported. Paths are read with
    -z, so quoting cannot hide or disguise a name.
    """
    output = _run(["git", "-C", str(repo), "status", "--porcelain=v1", "-z",
                   "--untracked-files=all"], environment=environment, withhold=withhold).stdout
    entries = output.split("\0")
    changes: dict[str, str] = {}
    index = 0
    while index < len(entries):
        entry = entries[index]
        index += 1
        if not entry:
            continue
        if len(entry) < 4 or entry[2] != " ":
            raise MirrorRefused(f"cannot parse ledger writer status entry {entry!r}")
        status = entry[:2]
        changes[entry[3:]] = status
        if "R" in status or "C" in status:
            if index >= len(entries) or not entries[index]:
                raise MirrorRefused(f"ledger writer rename entry has no source path: {entry!r}")
            changes[entries[index]] = status
            index += 1
    return changes


class _PublicationInputsUnavailable(MirrorRefused):
    """An input refusal must never enter the failed-push recovery path."""


def _verify_publication_inputs(verify_inputs: Callable[[], None] | None) -> None:
    if verify_inputs is not None:
        try:
            verify_inputs()
        except (MirrorRefused, ValueError, OSError, subprocess.SubprocessError) as error:
            raise _PublicationInputsUnavailable(str(error)) from error


def _commit_and_push(writer: Path, paths: Sequence[str], message: str, *,
                     net_wrapper: str, base: str,
                     verify_inputs: Callable[[], None] | None = None) -> str | None:
    """Commit exactly PATHS directly on BASE, push, and prove it reached origin/main.

    BASE is the published series commit this publication was computed from.
    The writer must still be at BASE, and the new commit must have BASE as its
    only parent and change exactly the staged PATHS; otherwise nothing is
    pushed and the writer is retained. So a commit that anything else made in
    the writer (an empty one, or one carrying a forged latest.json or carrier)
    can never ride along underneath this publication. Returns None when none
    of PATHS changed. Refuses if the index holds any other path. Hooks are
    disabled: nothing planted in the ledger's Git directory runs here.

    A push refused because another host pushed first is not lost when main
    gained only appended data meanwhile: the same changes are committed on
    the new main and pushed (_push_rebuilt_over_appends), that commit is
    returned, and the writer moves to it. Otherwise the commit is set aside
    and the writer returns to BASE (_set_aside_unpushed_commit).
    """
    _verify_publication_inputs(verify_inputs)
    if not paths:
        return None
    head = _git(writer, "rev-parse", "HEAD^{commit}")
    if head != base:
        raise MirrorRefused(f"ledger writer HEAD {head} is not the publication base {base}; "
                            "nothing was pushed and the writer is retained for recovery")
    _git(writer, "add", "--", *paths)
    # --no-renames: a staged rename must list its source path too, as
    # diff-tree below does.
    staged = {path for path in _run([
        "git", "-C", str(writer), "diff", "--cached", "--no-renames", "--name-only", "-z",
    ]).stdout.split("\0") if path}
    if not staged <= set(paths):
        raise MirrorRefused(
            "ledger writer index holds paths outside this publication: "
            + ", ".join(sorted(staged - set(paths))))
    if not staged:
        return None
    _verify_publication_inputs(verify_inputs)
    _git(writer, "-c", "core.hooksPath=/dev/null", "commit", "--quiet", "--no-verify", "-m", message)
    commit = _git(writer, "rev-parse", "HEAD^{commit}")
    parents = _git(writer, "rev-list", "--parents", "-n", "1", commit).split()
    if parents != [commit, base]:
        raise MirrorRefused(f"ledger commit {commit} does not sit directly on the publication base "
                            f"{base} (parents {parents[1:]}); nothing was pushed")
    changed = {path for path in _run([
        "git", "-C", str(writer), "diff-tree", "--no-commit-id", "--name-only", "-r", "-z", base, commit,
    ]).stdout.split("\0") if path}
    if changed != staged:
        raise MirrorRefused(f"ledger commit {commit} changes paths other than this publication: "
                            + ", ".join(sorted(changed ^ staged)) + "; nothing was pushed")
    _move_render_provenance(writer, base, commit, changed)
    push_command = _push_main_command(writer, commit)
    _verify_publication_inputs(verify_inputs)
    try:
        _network(push_command, cwd=writer, wrapper=net_wrapper)
        _network(["git", "fetch", "--quiet", "origin", "main", HISTORY_FETCH_REFSPEC], cwd=writer, wrapper=net_wrapper)
        _verify_publication_inputs(verify_inputs)
        if not _published(writer, commit):
            raise MirrorRefused(f"pushed commit {commit} is not on the fetched main")
        _verify_publication_inputs(verify_inputs)
    except _PublicationInputsUnavailable:
        raise
    except MirrorRefused as error:
        try:
            published, detail = _push_rebuilt_over_appends(
                writer, commit, base, net_wrapper=net_wrapper, verify_inputs=verify_inputs)
        except _PublicationInputsUnavailable:
            raise
        except (MirrorRefused, OSError, subprocess.SubprocessError, *_UNPROVABLE) as failure:
            published, detail = None, f"not rebuilt: {failure}"
        if published is None:
            raise MirrorRefused(f"{error}; {detail}; {_set_aside_unpushed_commit(writer, commit, base)}") from error
        if published == commit:
            print(f"ledger publication: PUSH CONFIRMED ON A SECOND READ: {error}; {detail}", file=sys.stderr)
            return commit
        try:
            _widen_render_provenance(writer, commit, published)
            _git(writer, "-c", "core.hooksPath=/dev/null", "checkout", "--quiet", "--no-overwrite-ignore",
                 "--no-recurse-submodules", "--detach", published)
            _move_render_provenance(writer, commit, published)
            outcome = f"writer moved to {published}"
        except (MirrorRefused, OSError) as failure:
            # The changes are on main; only the writer is left behind, and a
            # derived commit that is not on main would jam the next run.
            try:
                kept = _set_aside_unpushed_commit(writer, commit, base)
            except (MirrorRefused, OSError) as again:
                kept = f"writer retained: {again}"
            outcome = f"the writer did not move to it ({failure}); {kept}"
        print(f"ledger publication: PUSH RACE LOST AND RECOVERED: {error}; {detail}; {outcome}",
              file=sys.stderr)
        return published
    return commit


def _push_main_command(writer: Path, commit: str) -> list[str]:
    """The push of COMMIT to main, with the history parts its manifest names when they are external.

    Both refs move in one atomic push, and main must fast-forward for it to
    land, so the history ref always holds the parts commit that main's newest
    manifest names; a forced update is safe because each parts commit is
    parentless and replaces the last.
    """
    command = ["git", "-c", "core.hooksPath=/dev/null", "push"]
    parts = _external_parts_commit(writer, commit)
    if parts is None:
        return [*command, "origin", f"{commit}:refs/heads/main"]
    return [*command, "--atomic", "origin", f"{commit}:refs/heads/main",
            f"+{parts}:{scorecard_cells.HISTORY_REF}"]


def _external_parts_commit(writer: Path, commit: str) -> str | None:
    """The parts commit COMMIT's external history manifest names, or None for any other layout."""
    oid = _blob_at(writer, commit, scorecard_cells.MANIFEST_PATH)
    encoded = None if oid is None else _stored_blob(writer, oid, scorecard_cells.MANIFEST_MAX_BYTES)
    if encoded is None:
        return None
    try:
        manifest = json.loads(encoded)
    except ValueError as error:
        raise MirrorRefused(f"{scorecard_cells.MANIFEST_PATH} at {commit} is not JSON: {error}") from error
    if not isinstance(manifest, dict) or manifest.get("schema") != scorecard_cells.EXTERNAL_MANIFEST_SCHEMA:
        return None
    parts = manifest.get("parts_commit")
    if not isinstance(parts, str) or not _COMMIT.fullmatch(parts):
        raise MirrorRefused(f"{scorecard_cells.MANIFEST_PATH} at {commit} names no forty-hex parts commit")
    return parts


def _set_aside_unpushed_commit(writer: Path, commit: str, base: str) -> str:
    """Name COMMIT by a recovery ref and return the writer to BASE.

    A refused push (another host pushed first) used to leave the writer on a
    derived commit that ``recover_spooled_commit`` refuses, so every later run
    on this host published nothing until an operator moved the writer
    (devbig030, 2026-10-03, https://github.com/rrnewton/dev-hermit/issues/447).
    Since the rebuild over appends (_push_rebuilt_over_appends) this runs
    only when the commit could not be published that way.
    The render record is made to hold at both COMMIT and BASE before the
    switch and narrowed to BASE after it (_widen_render_provenance).
    BASE is already published, so the next run starts from it; the commit
    stays inspectable under the ref (a push that landed before its fetch
    failed is harmless: BASE is then an ancestor of main). The
    switch carries any unrelated unstaged leftovers and removes only the
    commit's own paths; if it cannot, the writer stays and the reason is
    returned for the error message. --no-overwrite-ignore: an ignored file
    the switch would replace (an operator's scorecard/cells.json after the
    commit moved the history into parts) is in no commit and no recovery
    ref, so the switch fails and the writer stays instead. Every checkout of
    the writer passes --no-recurse-submodules: under submodule.recurse a
    gitlink would be populated from a retained module directory, and every
    later publication would refuse that checkout (_unstored_bytes).
    """
    recovery = f"refs/heads/recovery/derived-{commit}"
    existing = _run(["git", "-C", str(writer), "rev-parse", "--verify", "--quiet", recovery], check=False)
    if existing.returncode not in {0, 1} or (existing.returncode == 0 and existing.stdout.strip() != commit):
        return f"writer retained at {commit}: recovery ref {recovery} already names different work"
    try:
        _git(writer, "update-ref", recovery, commit, commit if existing.returncode == 0 else "0" * 40)
        _widen_render_provenance(writer, commit, base)
        _git(writer, "-c", "core.hooksPath=/dev/null", "checkout", "--quiet", "--no-overwrite-ignore",
             "--no-recurse-submodules", "--detach", base)
    except MirrorRefused as error:
        return f"writer retained at {_git(writer, 'rev-parse', 'HEAD', check=False)}: {error}"
    _move_render_provenance(writer, commit, base)
    return f"commit {commit}, not confirmed on main, kept as {recovery}; writer returned to {base}"


_CARRIER_PATH = re.compile(re.escape(finalized_proof.PREFIX) + r"/runs/[0-9a-f]{64}\.json")
_OBJECT_PATH = re.compile(re.escape(finalized_proof.PREFIX) + r"/objects/sha256/([0-9a-f]{64})")
_SCORECARD_PATHS = frozenset({
    "latest.json", *(str(path) for path in (*MIRRORED_SCORECARD_PATHS, *OPTIONAL_SCORECARD_PATHS)),
})
#: The (old mode, new mode) pairs a change a regenerated scorecard records may
#: have: it writes ordinary files that are not executable, so a type change,
#: symlink, gitlink or a new executable bit is not its output. Writing such a
#: file over one HEAD stores as executable (100755), or removing that file, is.
_SCORECARD_MODES = {
    "A": frozenset({("000000", "100644")}),
    "M": frozenset({("100644", "100644"), ("100755", "100644")}),
    "D": frozenset({("100644", "000000"), ("100755", "000000")}),
}


def _git_stored(repo: Path, *args: str, check: bool = True, input: bytes | None = None,
                environment: Mapping[str, str] | None = None) -> bytes:
    """Read stored Git objects as written, with replacement refs disabled.

    Bytes, not text: a decoded message would hide a CR or CRLF ending.
    INPUT, when given, is the command's standard input. ENVIRONMENT adds
    variables (a temporary index).
    """
    command = ["git", "--no-replace-objects", "-C", str(repo), *args]
    try:
        result = subprocess.run(command, capture_output=True, check=False, input=input,
                                timeout=DEFAULT_TIMEOUT_SECONDS,
                                env=_environment({"GIT_NO_REPLACE_OBJECTS": "1", **(environment or {})}))
    except (OSError, subprocess.SubprocessError) as error:
        raise MirrorRefused(f"cannot run git: {error}") from error
    if check and result.returncode != 0:
        detail = (result.stderr or result.stdout).decode(errors="replace").strip()
        raise MirrorRefused(f"git failed: {detail or f'exit {result.returncode}'}")
    return result.stdout


def _stored_ancestor(writer: Path, ancestor: str, descendant: str) -> bool:
    result = _run(["git", "--no-replace-objects", "-C", str(writer), "merge-base", "--is-ancestor",
                   ancestor, descendant], check=False, environment={"GIT_NO_REPLACE_OBJECTS": "1"})
    if result.returncode not in {0, 1}:
        raise MirrorRefused(f"cannot order ledger commits {ancestor} and {descendant}: "
                            + (result.stderr.strip() or f"exit {result.returncode}"))
    return result.returncode == 0


SNAPSHOT_TRAILER = ledger_snapshot.TRAILER


def snapshot_tips(writer: Path, upstream: str = "refs/remotes/origin/main") -> list[str]:
    """The old tips that ledger snapshots in UPSTREAM's history fold in, newest first.

    A squash (https://github.com/rrnewton/dev-hermit/issues/562) replaces
    main's history with a snapshot commit, so commits published before it are
    no longer ancestors of main; ledger_snapshot.folded_tips proves, from the
    old history this writer holds, which ones are folded in. Stored objects
    are read with replacement refs disabled.
    """
    def read(*args: str) -> str | None:
        result = _run(["git", "--no-replace-objects", "-C", str(writer), *args], check=False,
                      environment={"GIT_NO_REPLACE_OBJECTS": "1"})
        return result.stdout if result.returncode == 0 else None
    return ledger_snapshot.folded_tips(read, upstream)


def _published(writer: Path, commit: str, upstream: str = "refs/remotes/origin/main") -> bool:
    """Whether COMMIT is on UPSTREAM: an ancestor of it, or folded into one of its snapshots."""
    if _stored_ancestor(writer, commit, upstream):
        return True
    return any(_stored_ancestor(writer, commit, tip) for tip in snapshot_tips(writer, upstream))


def _published_base(writer: Path, head: str, upstream: str = "refs/remotes/origin/main") -> str | None:
    """The newest published commit HEAD descends from, or None if it shares none.

    That is the merge base of HEAD and UPSTREAM, or, when a squash left them
    unrelated, of HEAD and the old tip a snapshot folds in, whose tree is
    published unchanged.
    """
    for candidate in (upstream, *snapshot_tips(writer, upstream)):
        result = _run(["git", "--no-replace-objects", "-C", str(writer), "merge-base", head, candidate],
                      check=False, environment={"GIT_NO_REPLACE_OBJECTS": "1"})
        if result.returncode == 0:
            return result.stdout.strip()
        if result.returncode != 1:
            raise MirrorRefused(f"cannot find where {head} meets {candidate}: "
                                + (result.stderr.strip() or f"exit {result.returncode}"))
    return None


def _stored_view_clean(writer: Path) -> bool:
    """Whether the index and work tree match the commit HEAD stores.

    The caller's status check compares against the HEAD ordinary commands see.
    This one reads the stored commit, gitlinks included, so the checkout that
    sets HEAD aside cannot discard a file that only another view of HEAD had
    accounted for, however that view was made. It repeats _history_overlay's
    protection by a different route. The refresh only updates cached stat
    data, so an untouched file is not reported as changed.
    """
    _run(["git", "--no-replace-objects", "-C", str(writer), "update-index", "-q", "--refresh"],
         check=False, environment={"GIT_NO_REPLACE_OBJECTS": "1"})
    for check in (("diff-index", "--cached", "--quiet", "--ignore-submodules=none", "HEAD", "--"),
                  ("diff-files", "--quiet", "--ignore-submodules=none", "--")):
        result = _run(["git", "--no-replace-objects", "-C", str(writer), *check], check=False,
                      environment={"GIT_NO_REPLACE_OBJECTS": "1"})
        if result.returncode == 1:
            return False
        if result.returncode != 0:
            raise MirrorRefused(f"cannot compare the ledger writer with its stored HEAD: "
                                + (result.stderr.strip() or f"exit {result.returncode}"))
    return True


def _history_overlay(writer: Path) -> str | None:
    """Name a replacement ref or graft file that makes Git show other history.

    Either lets a commit present another commit's parents, message or tree to
    ordinary commands, including the cleanliness check and the checkout, so a
    writer under one is never classified. Git takes replacements from the
    refs under GIT_REPLACE_REF_BASE when it is set, matched as a literal
    prefix (``refs/custom-<sha>`` under ``refs/custom-``), which a
    for-each-ref pattern does not do, and from ``refs/replace/`` otherwise.
    Every ref, read as bytes (a ref name need not be UTF-8), is matched
    literally against both, so neither setting hides a replacement; a base
    with no ref under it replaces nothing, and the publication goes ahead (an
    ambient base must not stop publication, and the render copy withholds it).
    A graft file override (GIT_GRAFT_FILE) is refused outright: this
    publisher never sets one. The default graft path is read as Git prints
    it, minus only its newline, so no trailing space is lost.
    """
    environment = _environment()
    if "GIT_GRAFT_FILE" in environment:
        return f"graft file override GIT_GRAFT_FILE={environment['GIT_GRAFT_FILE']!r}"
    bases = [b"refs/replace/"]
    if "GIT_REPLACE_REF_BASE" in environment:
        bases.append(os.fsencode(environment["GIT_REPLACE_REF_BASE"]))
    for refname in _git_stored(writer, "for-each-ref", "--format=%(refname)").split(b"\n"):
        if refname and refname.startswith(tuple(bases)):
            under = ("" if refname.startswith(b"refs/replace/") else
                     f" under GIT_REPLACE_REF_BASE={environment['GIT_REPLACE_REF_BASE']!r}")
            return f"replacement ref {refname.decode(errors='replace')}{under}"
    grafts = _git_stored(writer, "rev-parse", "--path-format=absolute", "--git-path",
                         "info/grafts").removesuffix(b"\n")
    if os.path.lexists(grafts):
        return f"graft file {grafts.decode(errors='replace')!r}"
    return None


def _refuse_history_overlay(writer: Path) -> None:
    """Refuse to touch a writer under a replacement ref or graft file.

    Under one, the ancestry checks, the cleanliness check and the checkout can
    each see other history than the commits store: a graft can make an
    unpublished row commit read as an ancestor of main, so that it looks
    published and the checkout to main drops its row with no ref left. No
    production writer has one (the render copy's replacements live only in
    the copy), so this refuses before any decision that reads ancestry.
    """
    overlay = _history_overlay(writer)
    if overlay is not None:
        head = _git_stored(writer, "rev-parse", "--verify", "HEAD").decode().strip()
        raise MirrorRefused(f"ledger writer at {head} is under a {overlay}, which makes Git show other "
                            "history; nothing was published and the writer is retained for recovery")


def _stored_blob(writer: Path, oid: str, limit: int) -> bytes | None:
    size = int(_git_stored(writer, "cat-file", "-s", oid))
    if size > limit:
        return None
    encoded = _git_stored(writer, "cat-file", "blob", oid)
    return encoded if len(encoded) == size else None


def _blob_at(writer: Path, commit: str, path: str) -> str | None:
    """The blob id of the ordinary file PATH in COMMIT, else None."""
    listing = _git_stored(writer, "ls-tree", "-z", commit, "--", path).split(b"\0")
    if len(listing) != 2 or listing[1] != b"":
        return None
    header, _tab, name = listing[0].partition(b"\t")
    fields = header.split()
    if len(fields) != 3 or fields[0] != b"100644" or fields[1] != b"blob" or name != path.encode():
        return None
    return fields[2].decode()


#: Malformed stored content: a refusal from finalized_proof or scorecard_cells
#: (both ValueError), a missing or mistyped field, undecodable bytes, or JSON
#: nested past the interpreter's recursion limit. Each means "not provably
#: derived", never a crash before recover_spooled_commit.
_UNPROVABLE = (ValueError, KeyError, TypeError, UnicodeError, RecursionError)


def _proof_bound(writer: Path, head: str, carriers: Mapping[str, str],
                 objects: Mapping[str, str]) -> bool:
    """Whether the added carriers are valid and bind exactly the added objects.

    Each carrier must be the canonical manifest named by its own identity
    (finalized_proof.read_carriers), and every object it references must be an
    ordinary file in HEAD whose bytes hash to its name and pass the role's
    structural check. Every added object must be referenced by an added
    carrier; an existing carrier's object may already be on main. A carrier's
    artifacts together stay within finalized_proof.ARTIFACT_BYTES_PER_ROW, the
    budget the producer (capture_artifacts) and the reader
    (read_artifact_objects) hold one carrier to, so no single-file allowance
    admits a proof neither could carry.
    """
    limit = finalized_proof.ARTIFACT_BYTES_PER_ROW
    try:
        referenced = set()
        for path, oid in carriers.items():
            encoded = _stored_blob(writer, oid, limit)
            if encoded is None:
                return False
            value = finalized_proof.validate_manifest(finalized_proof._json(encoded))
            if (finalized_proof.carrier_path(value) != path
                    or encoded != finalized_proof.canonical_bytes(value) + b"\n"):
                return False
            total = 0
            for role in finalized_proof.artifact_roles(value["artifacts"]):
                descriptor = value["artifacts"][role]
                total += descriptor["bytes"]
                if total > limit:
                    return False
                digest = descriptor["sha256"]
                oid = objects.get(digest) or _blob_at(writer, head, finalized_proof.object_path(digest))
                payload = None if oid is None else _stored_blob(writer, oid, limit)
                if (payload is None or len(payload) != descriptor["bytes"]
                        or hashlib.sha256(payload).hexdigest() != digest):
                    return False
                finalized_proof.validate_artifact_bytes(payload, role, value)
                referenced.add(digest)
        return set(objects) <= referenced
    except _UNPROVABLE:
        return False


#: The largest file main can hold (GitHub refuses a file over 100 MiB).
_FILE_LIMIT = 100 * 1024 * 1024


def _scorecard_bound(writer: Path, head: str, changed: Sequence[str]) -> bool:
    """Whether HEAD's latest.json binds every scorecard change by its digest.

    The publisher writes latest.json last (write_metadata) with the sha256 of
    every file it generated, scorecard/cells.json as the reassembled history,
    and commits only those files, latest.json and the history's stored parts
    (publish_scorecard). So a regenerated scorecard carries that metadata,
    changes nothing it does not name, and every named file at HEAD hashes to
    its recorded digest; the history must reassemble from a manifest that
    binds every part, with no part beside a single-file history
    (scorecard_cells.read_commit).
    """
    try:
        oid = _blob_at(writer, head, "latest.json")
        encoded = None if oid is None else _stored_blob(writer, oid, scorecard_cells.MANIFEST_MAX_BYTES)
        if encoded is None:
            return False
        value = finalized_proof._json(encoded)
        if not isinstance(value, dict) or value.get("schema") != METADATA_SCHEMA:
            return False
        hashes = value.get("scorecard")
        if (not isinstance(hashes, dict)
                or not all(path.as_posix() in hashes for path in MIRRORED_SCORECARD_PATHS)):
            return False
        for path in changed:
            if not (path == "latest.json" or path in hashes or (
                    scorecard_cells.LOGICAL_PATH in hashes and scorecard_cells.is_storage_path(path))):
                return False
        for path, digest in hashes.items():
            if not isinstance(path, str) or not isinstance(digest, str):
                return False
            if path == scorecard_cells.LOGICAL_PATH:
                data = scorecard_cells.read_commit(
                    writer, head, environment=_environment({"GIT_NO_REPLACE_OBJECTS": "1"}))
            else:
                # A proof file has the producer's capacity, as in
                # _proof_bound, so a commit that qualifies as a proof alone
                # still qualifies beside a scorecard. _proof_bound has
                # already held each added carrier's artifacts to that
                # budget together.
                limit = (finalized_proof.ARTIFACT_BYTES_PER_ROW
                         if _CARRIER_PATH.fullmatch(path) or _OBJECT_PATH.fullmatch(path) else _FILE_LIMIT)
                blob = _blob_at(writer, head, path)
                data = None if blob is None else _stored_blob(writer, blob, limit)
            if data is None or hashlib.sha256(data).hexdigest() != digest:
                return False
        return True
    except _UNPROVABLE:
        return False


class _Change(NamedTuple):
    """One change git diff-tree --raw lists: modes and blob ids as Git prints them."""

    status: str
    old_mode: str
    new_mode: str
    old: str
    new: str
    path: str


#: (status, old mode, new mode) of a new ordinary file that is not executable.
_NEW_FILE = ("A", "000000", "100644")


def _raw_changes(writer: Path, old: str, new: str) -> list[_Change] | None:
    """Every change from commit OLD to commit NEW in the stored trees, gitlinks included.

    A rename is listed as a removal and an addition. None when the listing
    cannot be parsed or a path is not UTF-8.
    """
    # --ignore-submodules=none: submodule.<name>.ignore or diff.ignoreSubmodules
    # would otherwise hide a gitlink change from this list.
    fields = _git_stored(writer, "diff-tree", "--raw", "--no-renames", "--no-commit-id", "-r", "-z",
                         "--ignore-submodules=none", old, new).split(b"\0")
    if fields and fields[-1] == b"":
        fields.pop()
    if len(fields) % 2:
        return None
    changes = []
    # -z raw output: ":<old mode> <new mode> <old id> <new id> <status>", then the path.
    for meta, name in zip(fields[0::2], fields[1::2]):
        parts = meta.decode(errors="replace").lstrip(":").split()
        if len(parts) != 5:
            return None
        old_mode, new_mode, old_id, new_id, status = parts
        try:
            path = name.decode()
        except UnicodeError:
            return None
        changes.append(_Change(status, old_mode, new_mode, old_id, new_id, path))
    return changes


def derived_commit_base(writer: Path, head: str) -> tuple[str, str, int] | None:
    """Classify HEAD as one derived commit this tool makes, without changing anything.

    Returns (parent, message, number of changes) only when every condition
    holds, reading stored objects with replacement refs disabled:

    - no replacement ref or graft file is present (_history_overlay);
    - the commit object has exactly one parent header, and that parent is on
      the fetched main;
    - the stored message is exactly one derived message plus one LF, as
      `git commit -m` writes it (bytes, so no CR, CRLF or second line);
    - every change, gitlinks included, is either a new ordinary carrier or
      object file at a canonical finalized-proof path, or, under the scorecard
      message only, an ordinary-file addition, edit or removal of latest.json,
      a scorecard file or a stored cells part;
    - the added carriers are valid and bind exactly the added objects
      (_proof_bound); and
    - under the scorecard message, latest.json names every change and every
      named file matches its digest (_scorecard_bound).

    Anything else returns None.
    """
    if _history_overlay(writer) is not None:
        return None
    raw = _git_stored(writer, "cat-file", "commit", head)
    headers, separator, body = raw.partition(b"\n\n")
    if not separator:
        return None
    parents = [line[len(b"parent "):].decode() for line in headers.split(b"\n")
               if line.startswith(b"parent ")]
    if len(parents) != 1 or not _COMMIT.fullmatch(parents[0]):
        return None
    base = parents[0]
    message = next((candidate for candidate in sorted((*PROOF_COMMIT_MESSAGES, SCORECARD_COMMIT_MESSAGE))
                    if body == candidate.encode() + b"\n"), None)
    if message is None:
        return None
    if not _published(writer, base):
        return None
    changes = _raw_changes(writer, base, head)
    if not changes:
        return None
    carriers: dict[str, str] = {}
    objects: dict[str, str] = {}
    changed = [change.path for change in changes]
    for status, old_mode, new_mode, _old, new, path in changes:
        found = _OBJECT_PATH.fullmatch(path)
        if found or _CARRIER_PATH.fullmatch(path):
            if (status, old_mode, new_mode) != ("A", "000000", "100644"):
                return None
            if found:
                objects[found.group(1)] = new
            else:
                carriers[path] = new
            continue
        if (message != SCORECARD_COMMIT_MESSAGE
                or (path not in _SCORECARD_PATHS and not scorecard_cells.is_storage_path(path))
                or (old_mode, new_mode) not in _SCORECARD_MODES.get(status, ())):
            return None
    if not _proof_bound(writer, head, carriers, objects):
        return None
    if message == SCORECARD_COMMIT_MESSAGE and not _scorecard_bound(writer, head, changed):
        return None
    return base, message, len(changes)


#: The largest append-only ledger file the completion check and the rebuild
#: after a lost push read: a month's series file for one host was 63 MB by
#: 2026-09-17 and keeps growing.
_APPENDED_FILE_LIMIT = 1 << 30


def _appended_bytes(writer: Path, change: _Change) -> bytes | None:
    """The bytes CHANGE appended to an ordinary file, or None if it did anything else.

    A new file appends all of its bytes. An edit appends only when the old
    file is empty or ends in a newline and is an exact prefix of the new one:
    Git's own blob identity of that prefix must equal the old blob id. A
    removal, a mode change, an executable file, or a file over
    _APPENDED_FILE_LIMIT is None.
    """
    if change[:3] not in {_NEW_FILE, ("M", "100644", "100644")}:
        return None
    current = _stored_blob(writer, change.new, _APPENDED_FILE_LIMIT)
    if current is None:
        return None
    size = 0
    if change.status == "M":
        size = int(_git_stored(writer, "cat-file", "-s", change.old))
        algorithm = {40: "sha1", 64: "sha256"}.get(len(change.old))
        if algorithm is None or size > len(current) or (size and current[size - 1:size] != b"\n"):
            return None
        prefix = hashlib.new(algorithm, b"blob %d\0" % size)
        prefix.update(memoryview(current)[:size])
        if prefix.hexdigest() != change.old:
            return None
    return current[size:]


def _json_objects(data: bytes) -> bool:
    """Whether every line of DATA is one JSON object."""
    for line in data.splitlines():
        try:
            if not isinstance(json.loads(line), dict):
                return False
        except (ValueError, RecursionError):
            return False
    return True


def _carrier_names(writer: Path, path: str, oid: str) -> tuple[str, str] | None:
    """The run and event of the canonical finalized carrier stored as blob OID at PATH, else None."""
    try:
        encoded = _stored_blob(writer, oid, finalized_proof.ARTIFACT_BYTES_PER_ROW)
        if encoded is None:
            return None
        value = finalized_proof.validate_manifest(finalized_proof._json(encoded))
        if (finalized_proof.carrier_path(value) != path
                or encoded != finalized_proof.canonical_bytes(value) + b"\n"):
            return None
        names = (value["terminal"]["run_id"], value["terminal"]["event_id"])
    except _UNPROVABLE:
        return None
    return names if all(isinstance(name, str) for name in names) else None


def _appends_only_since(writer: Path, base: str, upstream: str, *, runs: Iterable[str] = (),
                        events: Iterable[str] = ()) -> tuple[dict[str, tuple[str, str]] | None, str]:
    """What UPSTREAM appended after BASE, if that is all it changed; else None and why.

    The map gives each path UPSTREAM added or appended to its new mode and
    blob id. UPSTREAM must descend from BASE, and every change between them
    must be data a derived commit computed at BASE does not read:

    - a new ordinary file at a finalized-proof object path, or at a
      refused-input path (series.quarantine_input_path);
    - a new canonical finalized carrier of a run not in RUNS, for an event
      not in EVENTS (a second carrier of one run is refused, as
      stage_carrier refuses it); or
    - a new or appended-to .jsonl file (_appended_bytes) whose every
      appended line is one JSON object.

    latest.json and every other file under scorecard/ are refused (a
    scorecard another host published has its own head and rows), and so is
    any other path, edit, removal or mode.
    """
    if not _published(writer, base, upstream):
        return None, f"{upstream} does not descend from {base}"
    changes = _raw_changes(writer, base, upstream)
    if changes is None:
        return None, f"the changes from {base} to {upstream} cannot be listed"
    runs, events = frozenset(runs), frozenset(events)
    appended: dict[str, tuple[str, str]] = {}
    for change in changes:
        path = change.path
        why = f"{path} ({change.status} {change.old_mode} -> {change.new_mode}) is not appended data"
        named = series._QUARANTINE_REF_RE.fullmatch(path)
        if _OBJECT_PATH.fullmatch(path) or (named is not None and named.group(2).startswith(named.group(1))):
            if change[:3] != _NEW_FILE:
                return None, why
        elif _CARRIER_PATH.fullmatch(path):
            names = _carrier_names(writer, path, change.new) if change[:3] == _NEW_FILE else None
            if names is None:
                return None, why
            if names[0] in runs or names[1] in events:
                return None, f"{path} is another finalized carrier of run {names[0]} (event {names[1]})"
        elif (path == "latest.json" or path.startswith(("scorecard/", "ledger-archive/"))
              or not path.endswith(".jsonl")):
            return None, why
        else:
            data = _appended_bytes(writer, change)
            if data is None or not _json_objects(data):
                return None, why
        appended[path] = (change.new_mode, change.new)
    return appended, ""


def _appends_since(writer: Path, base: str, upstream: str, *, runs: Iterable[str] = (),
                   events: Iterable[str] = (), proof_paths: Iterable[str] | None = None
                   ) -> tuple[dict[str, tuple[str, str]] | None, str]:
    """Prove appends, optionally crossing exact compaction for a proof alone.

    A frozen scorecard or completion projection gets the original append-only
    check. Only a caller rebuilding a classified independent carrier/object
    commit supplies proof_paths. Compaction's regenerated summary is never
    overwritten with output computed before its window changed.
    """
    runs, events = frozenset(runs), frozenset(events)
    appended, why = _appends_only_since(writer, base, upstream, runs=runs, events=events)
    if appended is not None and _git_stored(
            writer, "rev-list", "--full-history", "--max-count=1", f"{base}..{upstream}",
            "--", "ledger-archive/").strip():
        # An endpoint diff can hide a compaction followed by a rollback of
        # its entire manifest and window. Require the interval proof even
        # when only ordinary appended files remain at the endpoint.
        appended = None
        why = "ledger-archive/ changed within the interval and is not appended data"
    if appended is not None or proof_paths is None:
        return appended, why

    def refused(reason: str):
        # Keep the original diagnostic (and its existing regression contract)
        # before explaining why this additional narrow proof also refused.
        return None, f"{why}; compaction replay refused: {reason}"

    pending = frozenset(proof_paths)
    if not pending or any(not (_CARRIER_PATH.fullmatch(path) or _OBJECT_PATH.fullmatch(path))
                          for path in pending):
        return refused("only independent finalized carrier/object appends may cross compaction")
    if not _published(writer, base, upstream):
        return None, why
    # A long or merged interval is left for a fresh publication instead of
    # turning this proof-only recovery into an unbounded history scan.
    commits = _git_stored(writer, "rev-list", "--first-parent", "--reverse", "--max-count=257",
                          f"{base}..{upstream}").decode().split()
    if not commits or len(commits) > 256:
        return refused("compaction replay requires at most 256 ordinary linear commits")
    from compaction_delta import classify
    combined, previous, found = {}, base, False
    guarded_paths, guarded_runs, guarded_events = set(), set(), set()
    for commit in commits:
        parents = _git_stored(writer, "rev-parse", f"{commit}^@").decode().split()
        if parents != [previous]:
            return refused("compaction replay interval is not an ordinary linear chain")
        additions, reason = _appends_only_since(
            writer, previous, commit, runs=runs | guarded_runs, events=events | guarded_events)
        if additions is None:
            delta, reason = classify(writer, previous, commit)
            if delta is None:
                return refused(f"unproved compaction at {commit}: {reason}")
            if runs & delta.archived_run_ids or events & delta.archived_event_ids:
                return refused("pending proof names an archived run or terminal event")
            if pending & (delta.retired_paths | delta.archived_carrier_paths):
                return refused("pending proof would resurrect an archived carrier or retired operand")
            additions = {change.path: (change.new_mode, change.new) for change in delta.changes}
            guarded_paths.update(delta.retired_paths | delta.archived_carrier_paths)
            guarded_runs.update(delta.archived_run_ids)
            guarded_events.update(delta.archived_event_ids)
            found = True
        elif guarded_paths & additions.keys():
            return refused("a later append recreates an archived carrier or retired path")
        combined.update(additions)
        previous = commit
    if previous != upstream or not found:
        return refused("no complete compaction interval was proved")
    # Include deletions and regenerated outputs in this map: the caller's
    # overlap check must see them rather than silently ignoring a dependency.
    return combined, ""


#: How many times _push_rebuilt_over_appends rebuilds a derived commit on a
#: main that keeps moving before it gives up. A rebuild takes seconds and
#: append commits landed every 5 to 15 minutes on 2026-10-06.
REAPPLY_ATTEMPTS = 3


def _push_rebuilt_over_appends(writer: Path, commit: str, base: str, *,
                               net_wrapper: str,
                               verify_inputs: Callable[[], None] | None = None) -> tuple[str | None, str]:
    """Publish derived COMMIT's changes on a main that moved on from BASE only by appends.

    Returns the commit that reached main and how, or None and why not. The
    writer is never moved.

    A derived commit (a finalized proof or a regenerated scorecard) is
    computed from the rows at BASE and a run's retained results. Until this,
    its push was refused whenever another host pushed first, and set aside:
    on 2026-10-06 one carrier took about 5 minutes from reading main to its
    push, append commits landed every 5 to 15 minutes, and the same carrier
    lost three races in a row. Appended rows, refused inputs and other runs'
    proofs (_appends_since) change nothing the commit was computed from, so
    its changes are committed again on the new main and pushed, as the raw
    row publisher rebases its rows (publisher.publish), up to
    REAPPLY_ATTEMPTS times while main keeps moving by appends only.

    COMMIT itself is returned when it is on main after all (what failed was
    the check after a push that landed). A rebuilt commit is returned once it
    is on the fetched main. Its only parent is the main it was rebuilt on,
    its changes are exactly COMMIT's (each path to the same blob, or removed),
    less any object file main already holds with the same bytes, and
    derived_commit_base classifies it with COMMIT's message. Anything else
    returns None: COMMIT is not a derived commit on BASE, main did not move
    (the push was refused for another reason), main moved by more than
    appends, a fetch failed, or main moved again after the last rebuild.
    """
    _verify_publication_inputs(verify_inputs)
    classified = derived_commit_base(writer, commit)
    if classified is None or classified[0] != base:
        return None, f"{commit} is not a derived commit on {base}, so it was not rebuilt"
    message = classified[1]
    ours = {change.path: (change.new_mode, change.new) for change in _raw_changes(writer, base, commit) or ()}
    runs, events = set(), set()
    for path, (_mode, oid) in ours.items():
        if _CARRIER_PATH.fullmatch(path):
            names = _carrier_names(writer, path, oid)
            if names is None:
                return None, f"carrier {path} in {commit} cannot be read, so it was not rebuilt"
            runs.add(names[0])
            events.add(names[1])
    # Each commit pushed so far, and the main it was built on.
    pushed = {commit: base}
    tried = base
    refusals: list[str] = []
    for attempt in range(REAPPLY_ATTEMPTS + 1):
        try:
            _network(["git", "fetch", "--quiet", "origin", "main", HISTORY_FETCH_REFSPEC], cwd=writer, wrapper=net_wrapper)
        except MirrorRefused as error:
            rebuilt = [candidate for candidate in pushed if candidate != commit]
            return None, ("main could not be fetched again"
                          + (f" after pushing {', '.join(rebuilt)}, which may be on main" if rebuilt else "")
                          + f": {error}")
        for candidate, built_on in pushed.items():
            if _published(writer, candidate):
                _verify_publication_inputs(verify_inputs)
                if candidate == commit:
                    return commit, f"{commit} is on main after all"
                return candidate, (f"rebuilt as {candidate} on {built_on}, which main reached from {base} "
                                   f"by appends only, and on main after {attempt} rebuild(s)"
                                   + (f"; refused first: {'; '.join(refusals)}" if refusals else ""))
        if attempt == REAPPLY_ATTEMPTS:
            return None, (f"main moved again after {REAPPLY_ATTEMPTS} rebuilds; last refusal: "
                          f"{refusals[-1] if refusals else 'none'}")
        upstream = _git_stored(writer, "rev-parse", "--verify",
                               "refs/remotes/origin/main^{commit}").decode().strip()
        if upstream == tried:
            return None, f"main is still at {upstream}, so the push was not refused because main moved"
        appended, why = _appends_since(
            writer, base, upstream, runs=runs, events=events,
            **({"proof_paths": ours} if message in PROOF_COMMIT_MESSAGES else {}))
        if appended is None:
            return None, f"main moved from {base} to {upstream} by more than appends: {why}"
        if any(path.startswith("ledger-archive/") for path in appended):
            # Owner direction for retention: a failed push is a stop, even
            # when the read-only proof establishes that another ordinary
            # child could safely be built. Leave this proof for the existing
            # recovery-ref path and a fresh scheduled publication; do not
            # enable a new network retry across compaction.
            return None, (f"main advanced through authenticated compaction to {upstream}; "
                          "the failed push is not retried and the proof remains for recovery")
        remaining = dict(ours)
        for path in appended.keys() & ours.keys():
            # The same content-named object, added by another run's proof.
            if not (_OBJECT_PATH.fullmatch(path) and appended[path] == ours[path]):
                return None, f"main {upstream} also changed {path}"
            del remaining[path]
        with tempfile.TemporaryDirectory(prefix="ledger-reapply-") as temporary:
            index = {"GIT_INDEX_FILE": str(Path(temporary) / "index")}
            _git_stored(writer, "read-tree", upstream, environment=index)
            # Mode 0 removes the path, as a removal in COMMIT lists it.
            entries = b"".join(f"{mode} {oid}\t".encode() + path.encode() + b"\0"
                               for path, (mode, oid) in sorted(remaining.items()))
            _git_stored(writer, "update-index", "-z", "--index-info", input=entries, environment=index)
            tree = _git_stored(writer, "write-tree", environment=index).decode().strip()
        rebuilt = _git_stored(writer, "commit-tree", tree, "-p", upstream, "-m", message).decode().strip()
        listed = _raw_changes(writer, upstream, rebuilt)
        if (listed is None or {change.path: (change.new_mode, change.new) for change in listed} != remaining
                or derived_commit_base(writer, rebuilt) != (upstream, message, len(remaining))):
            return None, f"{commit} rebuilt on {upstream} as {rebuilt} is not the same derived commit"
        tried = upstream
        pushed[rebuilt] = upstream
        push_command = _push_main_command(writer, rebuilt)
        _verify_publication_inputs(verify_inputs)
        try:
            _network(push_command, cwd=writer, wrapper=net_wrapper)
        except MirrorRefused as error:
            refusals.append(f"{rebuilt}: {error}")
    raise AssertionError("unreachable")


def set_aside_stranded_derived_commit(writer: Path) -> str | None:
    """Free a writer that an earlier run left on one unconfirmed derived commit.

    _commit_and_push sets a refused derived push aside itself, but a run can
    still start with the writer on such a commit: one left by a tool tree from
    before that set-aside (RUN 2004 on devbig014, 2026-10-03, left ea80c0c
    after its push lost a race to devbig030's append), or one whose worker was
    killed between its commit and its push. recover_spooled_commit refuses any
    commit that is not a spooled append, so every later publication refused
    "unpublished derived or unknown work" until an operator moved the writer
    (https://github.com/rrnewton/dev-hermit/issues/447).

    Only a commit this tool provably made is set aside (derived_commit_base).
    Its content is computed from published rows and a run's retained results,
    so nothing unpublished is lost, and the commit stays inspectable under
    refs/heads/recovery/derived-<sha>, as _commit_and_push names it. Anything
    else returns None and stays where it is, for recover_spooled_commit to
    recover or refuse exactly as before. The caller holds the publication
    lock, has fetched main, has required a clean writer, and has refused a
    writer under a replacement ref or graft file (_refuse_history_overlay) or
    holding bytes its HEAD does not store (_require_stored_bytes).
    """
    head = _git_stored(writer, "rev-parse", "--verify", "HEAD^{commit}").decode().strip()
    if _published(writer, head):
        return None
    classified = derived_commit_base(writer, head)
    if classified is None or not _stored_view_clean(writer):
        return None
    base, message, count = classified
    outcome = _set_aside_unpushed_commit(writer, head, base)
    if _git_stored(writer, "rev-parse", "--verify", "HEAD^{commit}").decode().strip() != base:
        raise MirrorRefused(f"ledger writer is stranded on derived commit {head} and could not be "
                            f"freed: {outcome}")
    print(f"ledger publication: WRITER FREED: a previous run left the writer on derived commit "
          f"{head} ({message!r}, {count} path(s)), which never reached main; {outcome}. "
          "Its content is derived from published rows and is not republished by this run.",
          file=sys.stderr)
    return outcome


RENDER_RESCUE_COMMIT_MESSAGE = "Preserve scorecard output an unpublished render left in the ledger writer"
#: The statuses a render's leftovers can have: _copy_back writes, edits and
#: removes files unstaged, and _commit_and_push stages exactly those. A path
#: whose index and work tree differ, a rename, a copy or a conflict is not one.
_RENDER_LEFTOVER_STATUSES = {" M": True, "M ": True, "A ": True, "??": True, " D": False, "D ": False}


def _render_leftover(path: str, status: str) -> bool:
    if _scorecard_path(path):
        return status in _RENDER_LEFTOVER_STATUSES
    # finish_portable_proof only ever adds content-named carrier and object files.
    return bool(_CARRIER_PATH.fullmatch(path) or _OBJECT_PATH.fullmatch(path)) and status in {"??", "A "}


#: In the writer's per-worktree Git directory: the paths a publication was
#: about to write (the SHA-256 of the bytes) or remove (null) at HEAD,
#: recorded by _replace_writer_files before it changes any of them, and the
#: scorecard paths _copy_back found changed while the scorecard tool ran.
#: set_aside_render_leftovers takes only changes it names with those bytes.
RENDER_PROVENANCE = "ledger-render-provenance.json"
_RENDER_PROVENANCE_LIMIT = 16 * 1024 * 1024


def _render_provenance_path(writer: Path) -> Path:
    return Path(_git(writer, "rev-parse", "--absolute-git-dir")) / RENDER_PROVENANCE


def _read_render_provenance(writer: Path, head: str) -> dict[str, set[str | None]]:
    """What RENDER_PROVENANCE records at HEAD: each path's recorded SHA-256s, None for a removal.

    Empty when there is no record, it is not a regular file (a symbolic
    link is not followed, and a FIFO is not waited on: O_NONBLOCK), it is
    larger than _RENDER_PROVENANCE_LIMIT, it names none of HEAD among its
    heads (or, as a3f62cfee1 wrote it, as its one head), or any part of it is
    malformed.
    """
    try:
        descriptor = os.open(_render_provenance_path(writer),
                             os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK | os.O_CLOEXEC)
    except OSError:
        return {}
    try:
        if not stat.S_ISREG(os.fstat(descriptor).st_mode):
            return {}
        data = b""
        while chunk := os.read(descriptor, _RENDER_PROVENANCE_LIMIT + 1 - len(data)):
            data += chunk
            if len(data) > _RENDER_PROVENANCE_LIMIT:
                return {}
    finally:
        os.close(descriptor)
    try:
        value = json.loads(data)
    except (ValueError, RecursionError):
        return {}
    if not isinstance(value, dict) or not isinstance(value.get("paths"), dict):
        return {}
    heads = value["heads"] if "heads" in value else [value.get("head")]
    if not isinstance(heads, list) or head not in heads:
        return {}
    recorded: dict[str, set[str | None]] = {}
    for name, digests in value["paths"].items():
        if not isinstance(digests, list) or not all(
                digest is None or (isinstance(digest, str) and _SHA256_HEX.fullmatch(digest))
                for digest in digests):
            return {}
        recorded[name] = set(digests)
    return recorded


def _record_render_provenance(writer: Path, changes: Sequence[tuple[str, bytes | None]]) -> None:
    """Add CHANGES ((path, the bytes about to be written there or None for a removal) pairs) to RENDER_PROVENANCE.

    Entries recorded at another HEAD are dropped. The record is written to a
    file created under a fresh name, flushed to disk and renamed into place,
    so a reader sees the old record or the new one.
    """
    head = _git_stored(writer, "rev-parse", "--verify", "HEAD^{commit}").decode().strip()
    recorded = _read_render_provenance(writer, head)
    for name, data in changes:
        recorded.setdefault(name, set()).add(None if data is None else hashlib.sha256(data).hexdigest())
    _write_render_provenance(writer, [head], recorded)


def _move_render_provenance(writer: Path, old: str, new: str, committed: Iterable[str] = ()) -> None:
    """Carry what RENDER_PROVENANCE records at OLD over to NEW, where this publication just moved HEAD.

    The COMMITTED paths (stored at NEW now) are dropped. A derived commit made
    after the scorecard step failed (or its undoing after a refused push)
    moves HEAD while that step's leftovers are still in the writer, and the
    record keeps vouching for them there.
    """
    recorded = _read_render_provenance(writer, old)
    for name in committed:
        recorded.pop(name, None)
    if recorded:
        _write_render_provenance(writer, [new], recorded)
    else:
        _forget_render_provenance(writer)


def _widen_render_provenance(writer: Path, old: str, new: str) -> None:
    """Make what RENDER_PROVENANCE records at OLD hold at NEW too, before HEAD moves from OLD to NEW.

    Review of a3f62cfee1: a publication killed after a refused push's
    checkout returned HEAD to its base, and before the record followed it
    there (_move_render_provenance), left a record naming only the derived
    commit; the next run refused the scorecard step's leftovers and the
    writer jammed. A record that holds at both heads holds wherever the kill
    lands, and the move afterwards narrows it to NEW.
    """
    recorded = _read_render_provenance(writer, old)
    if recorded:
        _write_render_provenance(writer, [old, new], recorded)


def _write_render_provenance(writer: Path, heads: Sequence[str], recorded: Mapping[str, set[str | None]]) -> None:
    text = json.dumps({"heads": list(heads), "paths": {name: sorted(digests, key=lambda digest: digest or "")
                                                       for name, digests in recorded.items()}},
                      indent=1, sort_keys=True) + "\n"
    target = _render_provenance_path(writer)
    temporary = target.with_name(f".{RENDER_PROVENANCE}.{os.urandom(8).hex()}.tmp")
    descriptor = os.open(temporary, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW | os.O_CLOEXEC, 0o600)
    try:
        with os.fdopen(descriptor, "wb") as output:
            output.write(text.encode())
            output.flush()
            os.fsync(output.fileno())
        os.replace(temporary, target)
    except BaseException:
        with contextlib.suppress(OSError):
            os.unlink(temporary)
        raise


def _forget_render_provenance(writer: Path) -> None:
    with contextlib.suppress(FileNotFoundError):
        _render_provenance_path(writer).unlink()


def _scorecard_path(path: str) -> bool:
    return path in _SCORECARD_PATHS or scorecard_cells.is_storage_path(path)


#: What a writer that rescued files a render without provenance left commits.
FOREIGN_RENDER_RESCUE_COMMIT_MESSAGE = (
    "Preserve scorecard output of an unknown hermit commit found in the ledger writer")
#: One JSON line per writer self-heal, read by ci-hub/health/ledger_writer_self_heal.py.
SELF_HEAL_LOG_RELATIVE = Path("ignored/ci-hub/ledger-writer-self-heal.jsonl")
#: An index.lock this old, held by no live process, is left by a killed git
#: command; the publication queue cap is the longest a holder legitimately waits.
STALE_INDEX_LOCK_SECONDS = 1800


def record_writer_self_heal(parent: Path, kind: str, detail: str) -> None:
    """Append one writer self-heal to SELF_HEAL_LOG_RELATIVE, so the health tick names it."""
    path = series.shared_parent_root(parent) / SELF_HEAL_LOG_RELATIVE
    path.parent.mkdir(parents=True, exist_ok=True)
    entry = {"recorded_at": datetime.now(timezone.utc).isoformat(), "kind": kind, "detail": detail}
    with path.open("a", encoding="utf-8") as stream:
        stream.write(json.dumps(entry, sort_keys=True) + "\n")


def _known_hermit_commit(parent: Path, commit: str) -> bool:
    """Whether COMMIT is in the parent's Hermit checkout or is the target of a local run record."""
    hermit = series.shared_parent_root(parent) / "hermit"
    if _run(["git", "-C", str(hermit), "cat-file", "-e", f"{commit}^{{commit}}"],
            check=False).returncode == 0:
        return True
    runs = series.shared_parent_root(parent) / "ignored" / "validate" / "runs"
    for record in runs.glob("*.json") if runs.is_dir() else ():
        try:
            if commit.encode() in record.read_bytes() and json.loads(record.read_bytes()).get("target") == commit:
                return True
        except (OSError, ValueError):
            continue
    return False


def _foreign_render_commit(writer: Path, parent: Path | None) -> str | None:
    """The unknown Hermit commit the writer's latest.json names, when its render cannot be ours.

    A publication renders a validated commit: one in the parent's Hermit
    checkout and named by a run record. Scorecard files naming any other
    commit were written into the writer by something else (2026-10-11 12:11Z:
    a render of hermit 6c0043760f71, a commit on no remote and in no run
    record, stopped every publication for 1 h 45 min).
    """
    if parent is None:
        return None
    try:
        value = json.loads((writer / "latest.json").read_bytes())
    except (OSError, ValueError):
        return None
    commit = value.get("scorecard_hermit_commit") if isinstance(value, dict) else None
    if not isinstance(commit, str) or not _COMMIT.fullmatch(commit):
        return None
    return None if _known_hermit_commit(parent, commit) else commit


def _live_processes_using(paths: Sequence[Path]) -> list[int]:
    """PIDs whose working directory or command line names any of PATHS."""
    names = [str(path) for path in paths]
    found = []
    for entry in Path("/proc").iterdir():
        if not entry.name.isdigit():
            continue
        try:
            cwd = os.readlink(entry / "cwd")
            command = (entry / "cmdline").read_bytes().replace(b"\0", b" ").decode(errors="replace")
        except OSError:
            continue
        if any(cwd == name or cwd.startswith(name + "/") or name in command for name in names):
            found.append(int(entry.name))
    return found


def clear_stale_writer_index_lock(writer: Path, parent: Path, *, now: float | None = None) -> str | None:
    """Remove the writer's index.lock when a killed git command left it, and say so.

    Only a regular file older than STALE_INDEX_LOCK_SECONDS that no live
    process could still hold (none runs in, or names, the writer or its Git
    directory) is removed; the caller holds the publication lock.
    """
    lock = Path(_git(writer, "rev-parse", "--path-format=absolute", "--git-path", "index.lock"))
    try:
        info = os.lstat(lock)
    except FileNotFoundError:
        return None
    age = (time.time() if now is None else now) - info.st_mtime
    if not stat.S_ISREG(info.st_mode) or age < STALE_INDEX_LOCK_SECONDS:
        return None
    holders = _live_processes_using([writer.resolve(), lock.parent])
    if holders:
        return None
    lock.unlink()
    outcome = f"removed a stale {lock} ({info.st_size} bytes, {age:.0f} s old, no live process uses the writer)"
    record_writer_self_heal(parent, "stale-index-lock", outcome)
    print(f"ledger publication: WRITER SELF-HEAL: {outcome}", file=sys.stderr)
    return outcome


def set_aside_render_leftovers(writer: Path, parent: Path | None = None) -> str | None:
    """Save what an unpublished render left in the writer to a recovery ref, and clean it.

    The renderer runs in a throwaway copy (_render_copy), so a render killed
    while it runs never touches the writer. Its outputs reach the writer only
    in _copy_back, after which latest.json and any finalized-proof carrier are
    written and the commit is pushed. A failure between the copy-back and the
    push (devbig030, 2026-10-03 18:40Z: an rr-compat-only validate's derived
    proof refused after its render was copied back), or a kill there, left
    those files unstaged, and every later publication refused the dirty writer
    until an operator rescued them by hand: the series and parity spools
    stalled for about two hours.

    Acts only when HEAD is already on the fetched main, no replacement ref or
    graft file is present, every change is an ordinary-file render output
    (_render_leftover) with a mode the copy-back writes (_SCORECARD_MODES),
    every change to a scorecard file is one RENDER_PROVENANCE records at HEAD
    with the same bytes (or as a removal), and no other file holds bytes
    HEAD does not store (_unstored_paths). An operator's edit, mode change
    or removal of a scorecard file when no render ran is therefore refused,
    whatever its shape. The
    changed files are committed on HEAD under a temporary index outside the
    writer, the commit is named
    refs/heads/recovery/render-<sha>, each saved blob (or removal) is checked
    against the work tree, and only then is each path returned to HEAD
    (_restore_from_head), which moves every file aside and keeps it rather
    than writing over it. Anything
    else returns None and leaves the writer for _require_clean to refuse as
    before. The caller holds the publication lock and has fetched main.
    """
    changes = _worktree_changes(writer)
    if not changes or not all(_render_leftover(path, status) for path, status in changes.items()):
        return None
    if _history_overlay(writer) is not None:
        return None
    head = _git_stored(writer, "rev-parse", "--verify", "HEAD^{commit}").decode().strip()
    if not _published(writer, head):
        return None
    for path, status in changes.items():
        present = os.path.lexists(writer / path)
        if present != _RENDER_LEFTOVER_STATUSES[status]:
            return None
        if present and ((writer / path).is_symlink() or not (writer / path).is_file()):
            return None
    # Review of 0287e858e4: a change of the right shape is not proof that a
    # publication made it. Only what a publication recorded writing or
    # removing at this HEAD (_replace_writer_files), or found changed while
    # its scorecard tool ran (_copy_back), is render output. The saved
    # blobs are held to the same record below.
    provenance = _read_render_provenance(writer, head)
    # With PARENT, files no publication recorded are still set aside when they
    # are scorecard output of a commit no publication renders
    # (_foreign_render_commit); they are kept in the recovery commit.
    foreign = None
    for path, status in changes.items():
        if _scorecard_path(path):
            if _RENDER_LEFTOVER_STATUSES[status]:
                recorded = hashlib.sha256((writer / path).read_bytes()).hexdigest() in provenance.get(path, ())
            else:
                recorded = None in provenance.get(path, ())
            if not recorded:
                # A render always rewrites latest.json; an edit that left it
                # alone is not a render's output.
                foreign = (_foreign_render_commit(writer, parent)
                           if changes.get("latest.json") in {" M", "M "} else None)
                if foreign is None or not all(_scorecard_path(name) for name in changes):
                    return None
                break
    # The reset below rewrites every file whose bytes HEAD does not store,
    # including one the status view hides (an assume-unchanged or
    # skip-worktree bit, a filter, an ignored file): leave the writer for
    # _require_stored_bytes to refuse unless those are all render leftovers.
    if not set(_unstored_paths(writer)) <= set(changes):
        return None
    paths = sorted(changes)
    # An index bit (lowercase: assume-unchanged; S: skip-worktree) makes the
    # status view and the reset disagree about a path, and an untracked path
    # that HEAD stores is a removal plus a new file under one name: the unlink
    # after the reset would delete HEAD's copy. Leave both to _require_clean.
    tags = _git(writer, "ls-files", "-v", "-z", "--", *paths).split("\0")
    if any(entry and (entry[0].islower() or entry[0] == "S") for entry in tags):
        return None
    if any(status == "??" and _run(["git", "-C", str(writer), "cat-file", "-e", f"{head}:{path}"],
                                   check=False).returncode == 0
           for path, status in changes.items()):
        return None
    with tempfile.TemporaryDirectory(prefix="ledger-render-rescue-") as temporary:
        index = {"GIT_INDEX_FILE": str(Path(temporary) / "index")}
        _run(["git", "-C", str(writer), "read-tree", head], environment=index)
        _run(["git", "-C", str(writer), "add", "--all", "--", *paths], environment=index)
        tree = _run(["git", "-C", str(writer), "write-tree"], environment=index).stdout.strip()
    commit = _git(writer, "commit-tree", tree, "-p", head, "-m",
                  RENDER_RESCUE_COMMIT_MESSAGE if foreign is None else FOREIGN_RENDER_RESCUE_COMMIT_MESSAGE)
    # -z raw output: ":<old mode> <new mode> <old id> <new id> <status>", then the path.
    fields = _run(["git", "-C", str(writer), "diff-tree", "--no-renames", "--no-commit-id", "-r", "-z",
                   head, commit]).stdout.split("\0")
    modes = {fields[index + 1]: fields[index].lstrip(":").split() for index in range(0, len(fields) - 1, 2)}
    if set(modes) != set(paths):
        raise MirrorRefused(f"render leftovers in the ledger writer were not saved exactly (commit {commit} "
                            f"changes {sorted(set(modes) ^ set(paths))}); writer retained for recovery")
    # The copy-back writes ordinary files that are not executable: a new
    # executable bit or a type change on a stored file is not its output,
    # even when an index bit hid it until the bit was cleared.
    if any((change[0], change[1]) not in _SCORECARD_MODES.get(change[4][:1], ()) for change in modes.values()):
        return None
    saved: dict[str, bytes] = {}
    for path in paths:
        stored = _run(["git", "-C", str(writer), "cat-file", "-e", f"{commit}:{path}"], check=False)
        if _RENDER_LEFTOVER_STATUSES[changes[path]]:
            blob = _git_stored(writer, "cat-file", "blob", f"{commit}:{path}") if stored.returncode == 0 else None
            if blob is None or blob != (writer / path).read_bytes():
                raise MirrorRefused(f"render leftover {path} differs from its saved copy in {commit}; "
                                    "writer retained for recovery")
            # Changed since the check above: not what the publication wrote.
            if (foreign is None and _scorecard_path(path)
                    and hashlib.sha256(blob).hexdigest() not in provenance.get(path, ())):
                return None
            saved[path] = blob
        elif stored.returncode == 0:
            raise MirrorRefused(f"removed render output {path} is still present in {commit}; "
                                "writer retained for recovery")
    recovery = f"refs/heads/recovery/render-{commit}"
    _git(writer, "-c", "core.hooksPath=/dev/null", "update-ref", recovery, commit, "0" * 40)
    _restore_from_head(writer, head, commit, {path: changes[path] for path in paths}, saved)
    for path in paths:
        for directory in Path(path).parents:
            if directory == Path("."):
                break
            try:
                (writer / directory).rmdir()
            except OSError:
                break
    remaining = _worktree_changes(writer)
    if remaining or _git(writer, "rev-parse", "HEAD^{commit}") != head:
        raise MirrorRefused(f"render leftovers were saved as {recovery} but the ledger writer is still not "
                            f"clean at {head}: " + ", ".join(sorted(remaining)))
    _forget_render_provenance(writer)
    outcome = (f"{len(paths)} render output path(s) saved as {recovery} ({commit}); writer cleaned at {head}")
    if foreign is not None:
        outcome = (f"scorecard output of hermit {foreign}, which no publication renders (in no run record "
                   f"and not in the parent's Hermit checkout): {outcome}")
    if parent is not None:
        record_writer_self_heal(parent, "foreign-render" if foreign is not None else "render-leftovers", outcome)
    print(f"ledger publication: WRITER CLEANED: an unpublished scorecard render left "
          f"{', '.join(paths)} in the ledger writer; {outcome}. Nothing was published from them.",
          file=sys.stderr)
    return outcome


def _require_clean(repo: Path) -> None:
    status = _git(repo, "status", "--porcelain", "--untracked-files=all")
    if status:
        raise MirrorRefused(
            f"ledger writer has unpublished work; preserve and recover it before retrying: {repo}\n{status}"
        )


_ABSENT = "absent"


def _link_blob(target: bytes, algorithm: str) -> tuple[str, str, int]:
    """A symbolic link to TARGET as a blob: (blob id, SHA-256 of TARGET, its size)."""
    return (hashlib.new(algorithm, b"blob %d\0" % len(target) + target).hexdigest(),
            hashlib.sha256(target).hexdigest(), len(target))


def _work_tree_blob(path: bytes, mode: bytes, algorithm: str,
                    descriptor: int | None = None) -> tuple[str, str, int] | str | None:
    """PATH's bytes, read with no filter, as a blob stored with MODE.

    Returns _ABSENT when nothing is at PATH (it holds no bytes to lose), else
    (blob id, SHA-256 of the bytes, size), or None when what is there cannot be
    such a blob or cannot be read whole. A symbolic link stored in HEAD is
    read as its target, or, where a checkout wrote it as a regular file
    holding the target (core.symlinks=false), as that file's bytes. With
    DESCRIPTOR, an open file is read through it instead of opening PATH.
    """
    try:
        status = os.lstat(path) if descriptor is None else os.fstat(descriptor)
    except (FileNotFoundError, NotADirectoryError):
        return _ABSENT
    except OSError:
        return None
    blob = hashlib.new(algorithm)
    plain = hashlib.sha256()
    if mode == b"120000" and stat.S_ISLNK(status.st_mode):
        try:
            return _link_blob(os.readlink(path), algorithm)
        except OSError:
            return None
    if mode not in {b"100644", b"100755", b"120000"} or not stat.S_ISREG(status.st_mode):
        return None
    blob.update(b"blob %d\0" % status.st_size)
    read = 0
    try:
        if descriptor is None:
            with open(path, "rb") as source:
                while chunk := source.read(8 * 1024 * 1024):
                    read += len(chunk)
                    blob.update(chunk)
                    plain.update(chunk)
        else:
            while chunk := os.pread(descriptor, 8 * 1024 * 1024, read):
                read += len(chunk)
                blob.update(chunk)
                plain.update(chunk)
    except OSError:
        return None
    return (blob.hexdigest(), plain.hexdigest(), read) if read == status.st_size else None


def _without_filter_drivers(writer: Path) -> dict[str, str] | None:
    """An environment under which no filter driver runs for a Git command in WRITER.

    A filter driver is an arbitrary program: its smudge output can be the
    file being compared, so matching it would prove nothing. Every driver
    configured for WRITER, from any source Git reads there, gets an empty
    smudge, clean and process command, which Git treats as no filter, and
    required=false, appended to GIT_CONFIG_PARAMETERS so they override every
    other source. (Git 2.53 uses neither the smudge nor the clean command of
    a driver whose process command is set, even to an empty one; all three
    are emptied so that no Git version's reading of one is relied on.) That
    includes a driver with an empty name ([filter ""]), which an attribute
    set to nothing (`filter=`) selects. A driver named in the attributes but
    configured nowhere runs nothing already. None when the configuration
    cannot be read.
    """
    environment = _environment({"GIT_NO_REPLACE_OBJECTS": "1"})
    try:
        result = subprocess.run(
            ["git", "--no-replace-objects", "-C", str(writer), "config", "--includes", "-z", "--name-only",
             "--get-regexp", r"^filter\."],
            capture_output=True, check=False, timeout=DEFAULT_TIMEOUT_SECONDS, env=environment)
    except (OSError, subprocess.SubprocessError):
        return None
    if result.returncode not in {0, 1}:
        return None
    drivers = set()
    for key in result.stdout.split(b"\0"):
        driver, separator, _variable = key.removeprefix(b"filter.").rpartition(b".")
        if key.startswith(b"filter.") and separator:
            drivers.add(os.fsdecode(driver))
    _append_config(environment, [(f"filter.{driver}.{variable}", value) for driver in sorted(drivers)
                                 for variable, value in (("smudge", ""), ("clean", ""), ("process", ""),
                                                         ("required", "false"))])
    return environment


def _checkout_digest(writer: Path, name: bytes, oid: bytes) -> tuple[str, int] | None:
    """SHA-256 and size of the bytes a checkout writes for blob OID at NAME through Git alone.

    `git cat-file --filters` applies the conversions a checkout applies,
    under the writer's own configuration and attributes: the built-in
    end-of-line, ident and working-tree-encoding conversions, which read the
    blob, the configuration and the attributes, never the file being
    compared. Filter drivers are switched off (_without_filter_drivers), so
    no program runs. None when Git cannot produce them.
    """
    environment = _without_filter_drivers(writer)
    if environment is None:
        return None
    try:
        result = subprocess.run(
            ["git", "--no-replace-objects", "-C", str(writer), "cat-file", "--filters",
             b"--path=" + name, oid],
            capture_output=True, check=False, timeout=DEFAULT_TIMEOUT_SECONDS, env=environment)
    except (OSError, subprocess.SubprocessError):
        return None
    if result.returncode != 0:
        return None
    return hashlib.sha256(result.stdout).hexdigest(), len(result.stdout)


def _holds_stored_bytes(writer: Path, name: bytes, mode: bytes, oid: bytes,
                        read: tuple[str, str, int] | str | None) -> bool:
    """Whether READ (_work_tree_blob) is bytes HEAD stores at NAME as MODE OID.

    They are stored when they are the blob itself, or exactly what a checkout
    writes for it through Git's built-in conversions alone (_checkout_digest);
    a checkout recreates either from HEAD.
    """
    if read is None or read == _ABSENT:
        return False
    if read[0] == oid.decode():
        return True
    return mode != b"120000" and _checkout_digest(writer, name, oid) == read[1:]


def _unstored_bytes(writer: Path) -> list[str]:
    """Describe each file in the writer whose bytes its HEAD does not store (_unstored_paths)."""
    return [f"{name!r} {reason}" for name, reason in _unstored_paths(writer).items()]


def _unstored_paths(writer: Path) -> dict[str, str]:
    """Map each file in the writer whose bytes its HEAD does not store to why.

    The status check reads the work tree through Git's own view: it does not
    list ignored files, and a clean filter, an end-of-line conversion, an
    assume-unchanged or skip-worktree bit, or core.ignoreCase can make a
    changed or extra file compare clean. Every checkout and the scorecard
    regeneration a publication makes may then overwrite or delete such
    bytes, and they are in no commit and no recovery ref.

    This lists every untracked file, ignored ones included, with
    core.ignoreCase off so a name differing only in case from a stored one is
    listed. It reads every file HEAD stores (replacement refs disabled) from
    disk with no filter, and keeps those _holds_stored_bytes accepts: HEAD's
    blob, or exactly what a checkout writes for it through Git's built-in
    conversions (an end-of-line conversion under core.autocrlf, a symbolic
    link written as a file under core.symlinks=false), never through a filter
    program. A stored path with nothing on disk holds no bytes (a
    publication refuses a sparse checkout, which leaves one, writes back a
    file a skip-worktree bit left absent (_clear_index_bits), and git status
    reports a deleted file). A gitlink must have no
    checkout, or an empty directory: a populated submodule's bytes are
    another repository's.
    """
    found = {os.fsdecode(name): "is untracked or ignored"
             for name in _git_stored(writer, "-c", "core.ignoreCase=false", "ls-files", "-z",
                                     "--others").split(b"\0") if name}
    algorithm = _git_stored(writer, "rev-parse", "--show-object-format").decode().strip()
    if algorithm not in {"sha1", "sha256"}:
        raise MirrorRefused(f"ledger writer uses an unknown object format {algorithm!r}")
    root = os.fsencode(writer)
    for entry in _git_stored(writer, "ls-tree", "-r", "-z", "--full-tree", "HEAD").split(b"\0"):
        if not entry:
            continue
        header, _tab, name = entry.partition(b"\t")
        mode, kind, oid = header.split()
        path = os.path.join(root, name)
        if kind == b"commit":
            try:
                status = os.lstat(path)
                empty = stat.S_ISDIR(status.st_mode) and not os.listdir(path)
            except (FileNotFoundError, NotADirectoryError):
                continue
            except OSError:
                empty = False
            if not empty:
                found[os.fsdecode(name)] = ("is a submodule checkout whose bytes this "
                                            "publication cannot prove stored")
            continue
        read = _work_tree_blob(path, mode, algorithm)
        if read == _ABSENT or _holds_stored_bytes(writer, name, mode, oid, read):
            continue
        found[os.fsdecode(name)] = "differs from the bytes HEAD stores"
    return found


def _refuse_sparse_checkout(writer: Path) -> None:
    """Refuse a writer with a sparse checkout.

    The scorecard regeneration writes files outside the sparse patterns, and
    `git add` then refuses them, leaving staged metadata and the written
    files behind to block every later run. The tool creates the writer
    without one.
    """
    if _git(writer, "config", "--type=bool", "--get", "core.sparseCheckout", check=False) == "true":
        raise MirrorRefused(f"ledger writer has a sparse checkout, which a publication cannot write "
                            f"its scorecard into; nothing was published; disable it (git sparse-checkout "
                            f"disable) before retrying: {writer}")


def _clear_index_bits(writer: Path) -> None:
    """Clear every skip-worktree and assume-unchanged bit in the writer's index.

    `git add` does not stage a path under either bit: it refuses one under a
    skip-worktree bit when named and skips it under a named directory, so a
    deleted stale history file would stay stored beside the new parts, and
    under an assume-unchanged bit it stages nothing, so the old bytes would
    be published and the regenerated ones left on disk to block every later
    run. The tool sets neither bit, but Git sets the assume-unchanged bit
    itself under core.ignoreStat on each entry a checkout, add or refresh
    writes: the Git commands run from here override that setting
    (_environment), the publisher's raw-row checkout and commit do not. So
    the bits are cleared, not refused: when a publication starts and again
    just before the scorecard files are written (_copy_back).

    Call only just after _unstored_bytes found nothing: it reads every file
    from disk whatever its bit, so clearing a bit uncovers no edit and loses
    no byte. A stored file a skip-worktree bit left absent is then written
    from the index (`git checkout-index`, which never replaces a file already
    there), since `git add` would otherwise stage its deletion. Refuses
    unless the index then holds neither bit and git status is clean.
    """
    def marked() -> tuple[list[bytes], list[bytes]]:
        entries = [(entry[:1], entry[2:]) for entry in
                   _git_stored(writer, "ls-files", "-v", "-z").split(b"\0") if entry]
        # Tags: lowercase for assume-unchanged, S (or s) for skip-worktree.
        return ([name for tag, name in entries if tag.islower()],
                [name for tag, name in entries if tag.upper() == b"S"])

    assumed, skipped = marked()
    if not assumed and not skipped:
        return
    # update-index applies one flag option to the paths it reads.
    for flag, names in (("--no-assume-unchanged", assumed), ("--no-skip-worktree", skipped)):
        if names:
            _git_stored(writer, "update-index", flag, "-z", "--stdin", input=b"".join(
                name + b"\0" for name in names))
    absent = [name for name in skipped if not os.path.lexists(os.path.join(os.fsencode(writer), name))]
    if absent:
        _git_stored(writer, "checkout-index", "-u", "-z", "--stdin",
                    input=b"".join(name + b"\0" for name in absent))
    remaining = [repr(os.fsdecode(name)) for name in {*sum(marked(), [])}]
    changes = _worktree_changes(writer)
    if remaining or changes:
        raise MirrorRefused(
            f"ledger writer index bits were cleared but the writer is not clean (still marked: "
            f"{', '.join(sorted(remaining)[:20]) or 'none'}; changed: "
            f"{', '.join(sorted(changes)[:20]) or 'none'}); nothing more was written: {writer}")
    print(f"ledger publication: cleared {len(assumed)} assume-unchanged and {len(skipped)} skip-worktree "
          f"index bit(s) in the ledger writer, writing back {len(absent)} absent stored file(s)",
          file=sys.stderr)


def _head_entries(writer: Path, names: Sequence[str]) -> dict[str, tuple[bytes, bytes]]:
    """Map each of NAMES that HEAD stores as a file to its (mode, object id)."""
    if not names:
        return {}
    found = {}
    for entry in _git_stored(writer, "--literal-pathspecs", "ls-tree", "-r", "-z", "--full-tree", "HEAD",
                             "--", *names).split(b"\0"):
        if entry:
            header, _tab, name = entry.partition(b"\t")
            mode, kind, oid = header.split()
            if kind == b"blob":
                found[os.fsdecode(name)] = (mode, oid)
    return found


#: How long a file a publication moved aside (_replace_writer_files) is kept
#: whole before a later publication may empty it (prune_displaced).
DISPLACED_RETENTION_SECONDS = 3600
_DISPLACED_PREFIX = ".displaced-"
_DISPLACED_RECORD = "displaced.json"
#: The most bytes prune_displaced reads of a record; a larger one is not used.
_DISPLACED_RECORD_LIMIT = 16 * 1024 * 1024
_SHA256_HEX = re.compile(r"[0-9a-f]{64}")
#: Opens a directory, refusing a symbolic link in its place (ENOTDIR on
#: Linux, ELOOP elsewhere).
_DIRECTORY_FLAGS = os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW | os.O_CLOEXEC
#: Lease-break notices (_empty_if_unchanged) are sent as this signal, which a
#: process ignores unless it handles it; the kernel's default, SIGIO, ends it.
_LEASE_SIGNAL = signal.SIGURG
#: Where the kernel says how long a lease holder has after a break begins
#: before the lease is broken for it and the waiting open goes ahead.
LEASE_BREAK_TIME = Path("/proc/sys/fs/lease-break-time")
#: A shorter lease-break-time leaves no margin to bound the truncation by.
_LEASE_BREAK_MINIMUM = 4
#: How long after its deadline _truncate_leased waits for the child that
#: truncates before killing it (a stopped child does not exit on its timer).
_TRUNCATE_GRACE_SECONDS = 5
#: Run by _truncate_leased: truncate the inherited leased descriptor, or exit
#: 3 if the deadline (a CLOCK_MONOTONIC time) has passed. An ITIMER_REAL
#: timer, armed before the last clock reading, raises SIGALRM by about the
#: deadline plus the time remaining when it was armed. Its default action
#: ends the process before it next runs in user space: at once when it is
#: running or waiting to run; a stopped one handles the signal before
#: anything else when it is continued, and _truncate_leased kills it a few
#: seconds after the deadline. Either way it never enters the truncation
#: later than the timer. Once the truncation returns it reads the clock and
#: prints that time, by which the truncation had taken effect.
_TRUNCATE_PROGRAM = """\
import os, signal, sys, time
descriptor, deadline = int(sys.argv[1]), float(sys.argv[2])
signal.signal(signal.SIGALRM, signal.SIG_DFL)
signal.pthread_sigmask(signal.SIG_UNBLOCK, {signal.SIGALRM})
remaining = deadline - time.monotonic()
if remaining <= 0:
    sys.exit(3)
signal.setitimer(signal.ITIMER_REAL, remaining)
if time.monotonic() >= deadline:
    sys.exit(3)
os.ftruncate(descriptor, 0)
ended = time.monotonic()
signal.setitimer(signal.ITIMER_REAL, 0)
sys.stdout.write(repr(ended))
"""
_HELD_UNSTORED = "held bytes its HEAD does not store, written while this publication ran"


class _DisplacedUnstored(Exception):
    """A file moved aside holds bytes HEAD does not store at its path; the message says why."""


def _inside(relative: str) -> bool:
    """Whether RELATIVE names a path below a directory: relative, encodable, and without an empty, . or .. part."""
    try:
        encoded = os.fsencode(relative)
    except (UnicodeError, TypeError):
        return False
    return (bool(encoded) and b"\0" not in encoded and not encoded.startswith(b"/")
            and all(part not in (b"", b".", b"..") for part in encoded.split(b"/")))


def _directory_under(root: int, relative: str, *, create: bool = False) -> tuple[int, str]:
    """A descriptor for the directory holding RELATIVE under the directory ROOT, and RELATIVE's last part.

    Each directory on the way is opened through the one above it, refusing a
    symbolic link or anything else that is not a directory (_DIRECTORY_FLAGS),
    so no rename or link planted while it runs can lead outside ROOT. A missing
    directory raises FileNotFoundError, or is made (mode 0700) with CREATE.
    The caller closes the descriptor.
    """
    if not _inside(relative):
        raise ValueError(f"{relative!r} is not a path inside a directory")
    *parents, leaf = relative.split("/")
    current = os.dup(root)
    try:
        for part in parents:
            try:
                following = os.open(part, _DIRECTORY_FLAGS, dir_fd=current)
            except FileNotFoundError:
                if not create:
                    raise
                with contextlib.suppress(FileExistsError):
                    os.mkdir(part, 0o700, dir_fd=current)
                following = os.open(part, _DIRECTORY_FLAGS, dir_fd=current)
            os.close(current)
            current = following
    except BaseException:
        os.close(current)
        raise
    return current, leaf


def _displaced_bytes(writer: Path, name: str, directory: int, leaf: str, entry: tuple[bytes, bytes] | None,
                     algorithm: str) -> tuple[str, int] | None:
    """SHA-256 and size of LEAF in DIRECTORY, just moved from NAME, when it holds bytes HEAD stores there.

    Raises _DisplacedUnstored when it does not. A regular file is read
    through one descriptor and left where it was moved: a process that still
    has it open, or reopens it through a handle it holds (an O_PATH
    descriptor, /proc/<pid>/fd), can write to it after any comparison, and
    such bytes then stay in a named file. A symbolic link holding stored
    bytes is left there too, and None returned: nothing can be written into
    a link, so no publication need empty it.
    """
    if entry is None:
        raise _DisplacedUnstored(_HELD_UNSTORED)
    mode, oid = entry
    try:
        status = os.lstat(leaf, dir_fd=directory)
    except OSError as error:
        raise _DisplacedUnstored(f"could not be read after it was moved ({error.strerror})") from None
    if stat.S_ISLNK(status.st_mode):
        try:
            target = os.readlink(os.fsencode(leaf), dir_fd=directory)
        except OSError as error:
            raise _DisplacedUnstored(f"could not be read after it was moved ({error.strerror})") from None
        read = _link_blob(target, algorithm) if mode == b"120000" else None
        if not _holds_stored_bytes(writer, os.fsencode(name), mode, oid, read):
            raise _DisplacedUnstored(_HELD_UNSTORED)
        return None
    if not stat.S_ISREG(status.st_mode):
        raise _DisplacedUnstored("was not a regular file, whose bytes a publication can compare")
    try:
        descriptor = os.open(leaf, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK | os.O_CLOEXEC, dir_fd=directory)
    except OSError as error:
        raise _DisplacedUnstored(f"could not be opened after it was moved ({error.strerror})") from None
    try:
        read = _work_tree_blob(os.fsencode(leaf), mode, algorithm, descriptor)
        if not _holds_stored_bytes(writer, os.fsencode(name), mode, oid, read):
            raise _DisplacedUnstored(_HELD_UNSTORED)
        return read[1], read[2]
    finally:
        os.close(descriptor)


def _record_name() -> str:
    """A fresh name for a record while it is written (_write_displaced_record)."""
    return f".{_DISPLACED_RECORD}.{os.urandom(8).hex()}.tmp"


def _write_displaced_record(retention: int, displaced_at: float, files: Sequence[Mapping[str, Any]]) -> None:
    """Record FILES (path under files/, SHA-256, size), moved at DISPLACED_AT, in the directory RETENTION.

    prune_displaced empties only what a record names. The record is written
    into a file created under a fresh name (O_CREAT|O_EXCL|O_NOFOLLOW: never
    through a link or into a file already there) and renamed over the old
    record, so a reader sees the old one or the new one.
    """
    text = json.dumps({"displaced_at": displaced_at, "files": list(files)}, indent=1, sort_keys=True) + "\n"
    for _attempt in range(16):
        temporary = _record_name()
        try:
            descriptor = os.open(temporary, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW | os.O_CLOEXEC,
                                 0o600, dir_fd=retention)
            break
        except FileExistsError:
            continue
    else:
        raise FileExistsError(errno.EEXIST, "every name tried for a new record was taken", _DISPLACED_RECORD)
    try:
        with os.fdopen(descriptor, "w", encoding="utf-8") as output:
            output.write(text)
        os.replace(temporary, _DISPLACED_RECORD, src_dir_fd=retention, dst_dir_fd=retention)
    except BaseException:
        with contextlib.suppress(OSError):
            os.unlink(temporary, dir_fd=retention)
        raise


def _remove_empty_directories(name: str | Path, parent: int | None = None) -> None:
    """Remove the directory NAME (in the directory PARENT), and every directory under it, that holds nothing.

    The walk (os.fwalk) goes through descriptors and follows no symbolic link.
    """
    with contextlib.suppress(OSError):
        for _path, subdirectories, _files, directory in os.fwalk(name, topdown=False, dir_fd=parent):
            for subdirectory in subdirectories:
                with contextlib.suppress(OSError):
                    os.rmdir(subdirectory, dir_fd=directory)
    with contextlib.suppress(OSError):
        os.rmdir(name, dir_fd=parent)


class _Retention:
    """A retention directory beside WRITER (<writer>.displaced-*/files/), made when a file is first moved there.

    _replace_writer_files and _restore_from_head move files there instead of
    writing over them; keep() records one holding bytes that are stored (in
    HEAD, or in a recovery ref) in displaced.json, so prune_displaced may
    empty it later. close() releases the descriptors and removes the
    directory if nothing was moved there.
    """

    def __init__(self, writer: Path) -> None:
        self.writer = writer
        self.path: Path | None = None
        self._directory: int | None = None
        self._files: int | None = None
        self._displaced_at = time.time()
        self._recorded: list[dict[str, Any]] = []

    def move(self, name: str) -> tuple[int, str, Path]:
        """Rename the writer's NAME to files/NAME here: the directory now holding it (the caller closes
        the descriptor), its last part, and where it is."""
        if self.path is None:
            self.path = Path(tempfile.mkdtemp(prefix=f"{self.writer.name}{_DISPLACED_PREFIX}",
                                              dir=self.writer.parent))
            self._directory = os.open(self.path, _DIRECTORY_FLAGS)
            os.mkdir("files", 0o700, dir_fd=self._directory)
            self._files = os.open("files", _DIRECTORY_FLAGS, dir_fd=self._directory)
        assert self._files is not None
        directory, leaf = _directory_under(self._files, name, create=True)
        try:
            os.rename(self.writer / name, leaf, dst_dir_fd=directory)
        except BaseException:
            os.close(directory)
            raise
        return directory, leaf, self.path / "files" / name

    def keep(self, name: str, digest: str, size: int) -> None:
        """Record the file moved from NAME, holding SIZE bytes with SHA-256 DIGEST, for prune_displaced."""
        assert self._directory is not None
        self._recorded.append({"path": name, "sha256": digest, "size": size})
        _write_displaced_record(self._directory, self._displaced_at, self._recorded)

    def close(self) -> None:
        for descriptor in (self._files, self._directory):
            if descriptor is not None:
                os.close(descriptor)
        self._files = self._directory = None
        if self.path is not None:
            _remove_empty_directories(self.path)


#: Beside the writer: where _install_exclusive writes a file whole before
#: linking it into the writer. Removed when the publication step ends; one a
#: kill left holds part of a file the next render writes again.
_STAGING_PREFIX = ".staging-"


def _staging_directory(writer: Path) -> Path:
    return Path(tempfile.mkdtemp(prefix=f"{writer.name}{_STAGING_PREFIX}", dir=writer.parent))


def _install_exclusive(staging: Path, target: Path, data: bytes, mode: int = 0o666) -> None:
    """Create TARGET holding all of DATA, never over a file already there (FileExistsError).

    DATA is written to a new file in STAGING (a directory beside the writer,
    so on its filesystem and outside its work tree) and then linked to
    TARGET; link(2) refuses when anything is there. Review of a3f62cfee1:
    DATA written straight into TARGET left a prefix of it when the
    publication was killed part way (SIGXFSZ after 4096 of 18000 bytes),
    bytes no render record names, and every later run refused the writer.
    Such a kill now leaves TARGET absent, which the record names as
    possible. MODE is as for os.open (the umask applies, as for git).
    """
    target.parent.mkdir(parents=True, exist_ok=True)
    staged = staging / os.urandom(8).hex()
    descriptor = os.open(staged, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW | os.O_CLOEXEC, mode)
    try:
        with os.fdopen(descriptor, "wb") as output:
            output.write(data)
        os.link(staged, target)
    finally:
        with contextlib.suppress(FileNotFoundError):
            os.unlink(staged)


def _read_moved(directory: int, leaf: str) -> bytes | None:
    """The bytes of the regular file LEAF in DIRECTORY, read through one descriptor; None for anything else."""
    try:
        descriptor = os.open(leaf, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK | os.O_CLOEXEC, dir_fd=directory)
    except OSError:
        return None
    try:
        if not stat.S_ISREG(os.fstat(descriptor).st_mode):
            return None
        chunks = []
        while chunk := os.read(descriptor, 8 * 1024 * 1024):
            chunks.append(chunk)
        return b"".join(chunks)
    finally:
        os.close(descriptor)


def _restore_from_head(writer: Path, head: str, commit: str, changes: Mapping[str, str],
                       saved: Mapping[str, bytes]) -> None:
    """Return each of CHANGES (path: two-letter status) to what HEAD stores, keeping every file it moves.

    Review of a3f62cfee1: `git reset --hard` here rewrote every path from
    HEAD, erasing bytes an operator wrote to one after
    set_aside_render_leftovers compared it with its copy SAVED in COMMIT.
    Each file still present is now renamed into a retention directory
    (_Retention), an atomic move no later open of the path reaches, and
    compared with SAVED there. One that differs stops the cleanup, kept
    where it was moved and named in the error; one that matches is recorded
    there for prune_displaced, which empties it once it is unchanged and
    closed (a process that has it open can still write to it) and never
    unlinks it. HEAD's bytes, as a checkout writes them (cat-file
    --filters), are then created at each path HEAD stores, never over a file
    that appeared there meanwhile (_install_exclusive), which stops the
    cleanup and stays. The index entries of staged changes are reset first
    (that changes no file), and a removal of each stored path is put on
    record before any path changes, so a run killed part way leaves only
    changes the next run's cleanup takes.
    """
    stored = sorted(path for path, status in changes.items() if status not in ("??", "A "))
    _record_render_provenance(writer, [(path, None) for path in stored])
    staged = sorted(path for path, status in changes.items() if status[0] not in (" ", "?"))
    if staged:
        _git(writer, "-c", "core.hooksPath=/dev/null", "reset", "--quiet", "--no-recurse-submodules",
             head, "--", *staged)
    entries = _head_entries(writer, stored)
    if set(entries) != set(stored):
        raise MirrorRefused(f"render leftovers {sorted(set(stored) - set(entries))} are not files HEAD stores; "
                            f"they were saved as {commit} and the writer is retained for recovery")
    retention = _Retention(writer)
    staging = _staging_directory(writer)
    try:
        for path in sorted(changes):
            if _RENDER_LEFTOVER_STATUSES[changes[path]] and os.path.lexists(writer / path):
                directory, leaf, aside = retention.move(path)
                try:
                    held = _read_moved(directory, leaf)
                finally:
                    os.close(directory)
                if held is None or held != saved.get(path):
                    raise MirrorRefused(
                        f"render leftover {path} changed after it was saved in {commit}: it was moved to "
                        f"{aside} and kept, and the cleanup stopped; the writer is retained for recovery")
                retention.keep(path, hashlib.sha256(held).hexdigest(), len(held))
            if path in entries:
                data = _git_stored(writer, "cat-file", "--filters", f"{head}:{path}")
                try:
                    _install_exclusive(staging, writer / path, data,
                                       0o777 if entries[path][0] == b"100755" else 0o666)
                except FileExistsError:
                    raise MirrorRefused(
                        f"ledger writer path {path} was written by another process while the cleanup "
                        "returned it to HEAD; it was left there and the cleanup stopped; the writer is "
                        "retained for recovery") from None
    finally:
        retention.close()
        _remove_empty_directories(staging)


def _sha256_through(descriptor: int) -> tuple[str, int]:
    """SHA-256 and size of the bytes read through DESCRIPTOR from its start."""
    digest = hashlib.sha256()
    offset = 0
    while chunk := os.pread(descriptor, 8 * 1024 * 1024, offset):
        offset += len(chunk)
        digest.update(chunk)
    return digest.hexdigest(), offset


def _lease_break_seconds() -> int | None:
    """The kernel's lease-break-time (LEASE_BREAK_TIME) in seconds, or None when it cannot be read."""
    try:
        return int(LEASE_BREAK_TIME.read_text().strip())
    except (OSError, ValueError):
        return None


def _truncate_leased(descriptor: int, deadline: float) -> tuple[int, float | None]:
    """Truncate DESCRIPTOR in a child process the kernel ends at about DEADLINE (_TRUNCATE_PROGRAM).

    Returns the child's exit status (subprocess convention) and, when it
    exited 0, the CLOCK_MONOTONIC time (time.monotonic, which is system-wide)
    it read once the truncation had returned, else None. The status is 0
    when it truncated, 3 when the deadline had passed, -SIGALRM when its
    timer ended it, which may be during the truncation, and 3 too when it
    had not exited _TRUNCATE_GRACE_SECONDS after the deadline and was killed
    (a stopped child; it was waited for, so it truncates nothing later). The
    child shares the open file description, so the truncation breaks none
    of its leases.
    """
    try:
        result = subprocess.run(
            [sys.executable, "-I", "-S", "-c", _TRUNCATE_PROGRAM, str(descriptor), repr(deadline)],
            pass_fds=(descriptor,), stdin=subprocess.DEVNULL, stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL, timeout=max(0.0, deadline - time.monotonic()) + _TRUNCATE_GRACE_SECONDS,
            check=False,
        )
    except subprocess.TimeoutExpired:
        return 3, None
    except (OSError, subprocess.SubprocessError):
        return -1, None
    ended = None
    if result.returncode == 0:
        with contextlib.suppress(ValueError):
            ended = float(result.stdout.decode("ascii", "replace"))
        if ended is not None and not math.isfinite(ended):
            ended = None
    return result.returncode, ended


def _empty_if_unchanged(directory: int, leaf: str, digest: str, size: int) -> tuple[str, str | None]:
    """Empty LEAF in DIRECTORY if it still holds SIZE bytes with SHA-256 DIGEST and no other process has it open.

    Returns ("emptied", note), ("gone", None) when nothing is there or it is
    already empty, or ("kept", why). Nothing is unlinked: a rename or link
    does not break a lease, so no check made before an unlink could stop a
    process from opening the file in between and writing into it once it
    had no name. Instead the file is read and truncated through a descriptor
    holding a Linux write lease (F_SETLEASE F_WRLCK), which the kernel grants
    only while no other descriptor or shared mapping has the file open, and
    which makes any later open of it (even through a handle held from
    before) wait until the lease is released or, LEASE_BREAK_TIME seconds
    after that open began the break, broken for it. The lease is confirmed
    unbroken (F_GETLEASE) at a time T after the comparison, so no such open
    can go ahead before T plus lease-break-time; the truncation runs in a
    child that cannot begin it later than T plus half that
    (_truncate_leased, _TRUNCATE_PROGRAM), and the lease is released only
    after the child has exited. A waiting
    open therefore returns after the truncation, and what its process
    writes stays in the file, which keeps its name. The file is kept when
    the lease cannot be taken, for any reason, when lease-break-time cannot
    be read or is under _LEASE_BREAK_MINIMUM seconds, when its bytes changed,
    when a break began before T, when it has no name or more than one, or
    when the child did not truncate it. Should the truncation itself not be
    seen to end (by the child's clock reading after it, or else when this
    process saw the child exit) until within a second of T plus
    lease-break-time or later, with a break pending (the kernel holding the
    child inside it), the note says the bytes another process wrote may be
    gone; the second covers the kernel's rounding of the break to its tick.
    """
    try:
        descriptor = os.open(leaf, os.O_RDWR | os.O_NOFOLLOW | os.O_NONBLOCK | os.O_CLOEXEC, dir_fd=directory)
    except FileNotFoundError:
        return "gone", None
    except OSError as error:
        return "kept", f"could not be opened ({error.strerror})"
    try:
        status = os.fstat(descriptor)
        if not stat.S_ISREG(status.st_mode):
            return "kept", "is not a regular file"
        if status.st_size == 0:
            return "gone", None
        allowed = _lease_break_seconds()
        if allowed is None:
            return "kept", f"could not be emptied safely: {LEASE_BREAK_TIME} could not be read"
        if allowed < _LEASE_BREAK_MINIMUM:
            return "kept", (f"could not be emptied safely: {LEASE_BREAK_TIME} is {allowed} s, under "
                            f"{_LEASE_BREAK_MINIMUM} s")
        try:
            fcntl.fcntl(descriptor, fcntl.F_SETSIG, _LEASE_SIGNAL)
            fcntl.fcntl(descriptor, fcntl.F_SETLEASE, fcntl.F_WRLCK)
        except OSError as error:
            # EAGAIN: another descriptor or mapping has it open. Anything
            # else (EINVAL where the filesystem grants no lease, EACCES
            # without ownership or CAP_LEASE) says nothing about other
            # processes, so it proves nothing either.
            if error.errno in (errno.EAGAIN, errno.EBUSY):
                return "kept", f"is open in another process, which could still write to it (no write lease: {error.strerror})"
            return "kept", f"could not be leased (fcntl: {error.strerror}), so another process could still write to it"
        try:
            if _sha256_through(descriptor) != (digest, size):
                return "kept", "was written after it was moved there"
            # Read before the check: any break begins after this time.
            confirmed = time.monotonic()
            if fcntl.fcntl(descriptor, fcntl.F_GETLEASE) != fcntl.F_WRLCK:
                return "kept", "was opened by another process while it was compared"
            links = os.fstat(descriptor).st_nlink
            if links == 0:
                return "kept", "was replaced by another file while it was compared"
            if links != 1:
                return "kept", f"has {links} names, and emptying it would empty the others"
            exit_status, ended = _truncate_leased(descriptor, confirmed + allowed / 4)
            finished = time.monotonic()
            # The child cannot have begun a truncation after its timer, so an
            # empty file after any exit status was emptied in time.
            emptied = os.fstat(descriptor).st_size == 0
            if exit_status != 0 and not emptied:
                why = ("its deadline passed before the truncation began" if exit_status in (3, -signal.SIGALRM)
                       else f"the process that empties it failed (exit {exit_status})")
                return "kept", f"was not emptied: {why}"
            if fcntl.fcntl(descriptor, fcntl.F_GETLEASE) == fcntl.F_WRLCK:
                return "emptied", None
            # Review of a3f62cfee1: this process's own reading comes after
            # the child exited, later than the truncation took effect, so it
            # is used only when the child reported no time of its own.
            seen = (ended if ended is not None else finished) - confirmed
            if seen < allowed - 1:
                return "emptied", ("was opened by another process as it was emptied; what that process writes "
                                   "stays in the file, which no publication empties again")
            return "emptied", (f"was opened by another process while it was emptied, and the truncation was not "
                               f"seen to end until {seen:.1f} s after the lease was last confirmed, within 1 s of "
                               f"or past the {allowed} s lease-break-time: bytes that process wrote before the "
                               "truncation took effect may be gone; what it writes after it stays in the file")
        finally:
            # F_UNLCK fails (EAGAIN) where the lease was already broken.
            with contextlib.suppress(OSError):
                fcntl.fcntl(descriptor, fcntl.F_SETLEASE, fcntl.F_UNLCK)
    finally:
        os.close(descriptor)


def _unlink_if_empty(directory: int, leaf: str) -> tuple[bool, str | None]:
    """Remove LEAF in DIRECTORY if it is an empty regular file with one name that no other process has open.

    Returns (removed, note). Only a file _empty_if_unchanged emptied, or found
    empty, is offered here, once nothing in its retention directory's record
    is kept. The name is removed while this process holds a write lease on
    the file (F_SETLEASE F_WRLCK, granted only while no other descriptor or
    shared mapping has it open), after checking that the name still leads to
    the leased file. An open that begins before the removal, by the name or
    through a held handle, breaks the lease, which F_GETLEASE shows after the
    removal; an open by the name after it finds nothing. So the file goes
    only when it holds nothing and nothing has it open, and the note says
    when an open raced the removal: that process then has a file with no
    name, holding nothing.
    """
    try:
        descriptor = os.open(leaf, os.O_RDWR | os.O_NOFOLLOW | os.O_NONBLOCK | os.O_CLOEXEC, dir_fd=directory)
    except OSError:
        return False, None
    try:
        status = os.fstat(descriptor)
        if not stat.S_ISREG(status.st_mode) or status.st_size != 0 or status.st_nlink != 1:
            return False, None
        try:
            fcntl.fcntl(descriptor, fcntl.F_SETSIG, _LEASE_SIGNAL)
            fcntl.fcntl(descriptor, fcntl.F_SETLEASE, fcntl.F_WRLCK)
        except OSError:
            return False, None
        try:
            status = os.fstat(descriptor)
            if status.st_size != 0 or status.st_nlink != 1:
                return False, None
            try:
                named = os.stat(leaf, dir_fd=directory, follow_symlinks=False)
            except OSError:
                return False, None
            if not os.path.samestat(named, status):
                return False, None
            os.unlink(leaf, dir_fd=directory)
            if os.fstat(descriptor).st_nlink != 0:
                return True, "had another name when it was removed; that name still holds it"
            if fcntl.fcntl(descriptor, fcntl.F_GETLEASE) != fcntl.F_WRLCK:
                return True, "was opened by another process as it was removed; that process has it, empty, with no name"
            return True, None
        finally:
            with contextlib.suppress(OSError):
                fcntl.fcntl(descriptor, fcntl.F_SETLEASE, fcntl.F_UNLCK)
    finally:
        os.close(descriptor)


def _read_displaced_record(retention: int) -> tuple[float, list[tuple[str, str, int]]] | None:
    """The time and the files the record in the directory RETENTION names, or None for no usable record.

    A usable record is a regular file of at most _DISPLACED_RECORD_LIMIT
    bytes holding a JSON object whose displaced_at is a finite number and
    whose files are each a path (a string), a SHA-256 (64 lowercase hex
    digits) and a size (an integer, not negative). Whether a path stays
    inside files/ is checked where it is used.
    """
    try:
        descriptor = os.open(_DISPLACED_RECORD, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK | os.O_CLOEXEC,
                             dir_fd=retention)
    except OSError:
        return None
    data = bytearray()
    try:
        if not stat.S_ISREG(os.fstat(descriptor).st_mode):
            return None
        while len(data) <= _DISPLACED_RECORD_LIMIT and (chunk := os.read(descriptor, 1024 * 1024)):
            data += chunk
    except OSError:
        return None
    finally:
        os.close(descriptor)
    if len(data) > _DISPLACED_RECORD_LIMIT:
        return None
    try:
        record = json.loads(bytes(data))
    except (ValueError, RecursionError):
        return None
    if not isinstance(record, dict) or not isinstance(record.get("files"), list):
        return None
    displaced_at = record.get("displaced_at")
    if isinstance(displaced_at, bool) or not isinstance(displaced_at, (int, float)):
        return None
    try:
        displaced_at = float(displaced_at)
    except OverflowError:
        return None
    if not math.isfinite(displaced_at):
        return None
    files = []
    for item in record["files"]:
        if not isinstance(item, dict):
            return None
        path, digest, size = item.get("path"), item.get("sha256"), item.get("size")
        if (not isinstance(path, str) or not isinstance(digest, str) or not _SHA256_HEX.fullmatch(digest)
                or type(size) is not int or size < 0):
            return None
        files.append((path, digest, size))
    return displaced_at, files


def _prune_retention(parent: int, name: str, shown: Path, now: float) -> int:
    """Empty what the record in the retention directory NAME (in the directory PARENT; SHOWN in messages)
    names, where that is safe (_empty_if_unchanged), and return how many files were emptied.

    The retention directory, its files/ and each directory under that are
    opened without following a symbolic link and used only through those
    descriptors, so moving them or planting a link while this runs cannot
    lead outside it.
    """
    try:
        if not stat.S_ISDIR(os.lstat(name, dir_fd=parent).st_mode):
            return 0
        retention = os.open(name, _DIRECTORY_FLAGS, dir_fd=parent)
    except OSError:
        return 0
    try:
        recorded = _read_displaced_record(retention)
        if recorded is None:
            return 0
        displaced_at, files = recorded
        if now - displaced_at < DISPLACED_RETENTION_SECONDS:
            return 0
        try:
            root = os.open("files", _DIRECTORY_FLAGS, dir_fd=retention)
        except FileNotFoundError:
            os.unlink(_DISPLACED_RECORD, dir_fd=retention)
            _remove_empty_directories(name, parent)
            return 0
        emptied = 0
        kept: list[tuple[dict[str, Any], str]] = []
        notes: list[str] = []
        done: list[str] = []
        try:
            for relative, digest, size in files:
                item = {"path": relative, "sha256": digest, "size": size}
                if not _inside(relative):
                    kept.append((item, "is not a path inside its retention directory"))
                    continue
                try:
                    directory, leaf = _directory_under(root, relative)
                except FileNotFoundError:
                    continue
                except OSError as error:
                    kept.append((item, "is not reachable inside its retention directory through directories "
                                       f"alone, without following a symbolic link ({error.strerror})"))
                    continue
                try:
                    state, text = _empty_if_unchanged(directory, leaf, digest, size)
                finally:
                    os.close(directory)
                if state == "kept":
                    kept.append((item, text))
                    continue
                if state == "emptied":
                    emptied += 1
                done.append(relative)
                if text:
                    notes.append(f"{relative} {text}")
            # Nothing the record names is kept: remove the files it emptied,
            # so a retention directory does not stay behind holding empty
            # files (480 of them beside the writer on 2026-10-12).
            if not kept:
                for relative in done:
                    try:
                        directory, leaf = _directory_under(root, relative)
                    except OSError:
                        continue
                    try:
                        _removed, text = _unlink_if_empty(directory, leaf)
                    finally:
                        os.close(directory)
                    if text:
                        notes.append(f"{relative} {text}")
        finally:
            os.close(root)
        if kept:
            _write_displaced_record(retention, displaced_at, [item for item, _reason in kept])
            listed = "; ".join(f"{item['path']} {reason}" for item, reason in kept[:5])
            more = f"; and {len(kept) - 5} more" if len(kept) > 5 else ""
            print(f"ledger publication: KEPT {len(kept)} file(s) an earlier publication moved to "
                  f"{shown}/files: {listed}{more}", file=sys.stderr)
        else:
            os.unlink(_DISPLACED_RECORD, dir_fd=retention)
            # The directory goes only if nothing else is left in it: a file
            # the record did not name (bytes HEAD does not store) stays.
            _remove_empty_directories(name, parent)
        for note in notes:
            print(f"ledger publication: a file an earlier publication moved to {shown}/files: {note}",
                  file=sys.stderr)
        return emptied
    finally:
        os.close(retention)


def prune_displaced(writer: Path, now: float | None = None) -> None:
    """Empty the stored bytes earlier publications moved aside, where that is safe.

    _replace_writer_files and _restore_from_head move each file they replace
    into a retention directory beside WRITER (<writer>.displaced-*/files/)
    and record the ones holding stored bytes in displaced.json. A recorded file is emptied
    (truncated, never unlinked) once DISPLACED_RETENTION_SECONDS have passed
    since then, if it is unchanged and no other process has it open
    (_empty_if_unchanged), and leaves the record; the rest are kept, named on
    stderr, and kept in the record for the next publication. Once the record
    names nothing it is removed, the files it emptied are removed while no
    other process has them open (_unlink_if_empty), and so is each directory
    left empty, the retention directory included. Nothing a usable record does not name is touched: a
    file that held unstored bytes, a symbolic link, and a directory without
    a usable record (a publication killed before it wrote one, or an
    earlier version of this tool, which wrote none). Never raises: a kept
    file costs only disk.
    """
    now = time.time() if now is None else now
    prefix = f"{writer.name}{_DISPLACED_PREFIX}"
    emptied = 0
    try:
        parent = os.open(writer.parent, os.O_RDONLY | os.O_DIRECTORY | os.O_CLOEXEC)
        try:
            names = sorted(name for name in os.listdir(parent) if name.startswith(prefix))
        except BaseException:
            os.close(parent)
            raise
    except OSError as error:
        print(f"ledger publication: could not list {writer.parent} for files earlier publications moved "
              f"aside ({error.strerror})", file=sys.stderr)
        return
    try:
        for name in names:
            try:
                emptied += _prune_retention(parent, name, writer.parent / name, now)
            except Exception as error:  # noqa: BLE001 - a failure here must not stop the publication.
                print(f"ledger publication: could not finish emptying the files an earlier publication moved "
                      f"to {writer.parent / name}/files ({type(error).__name__}: {error}); they are kept",
                      file=sys.stderr)
    finally:
        os.close(parent)
    if emptied:
        print(f"ledger publication: emptied {emptied} unchanged file(s) earlier publications moved aside",
              file=sys.stderr)


def _replace_writer_files(writer: Path, writes: Mapping[str, bytes], removals: Sequence[str] = ()) -> None:
    """Write WRITES and remove REMOVALS (paths under WRITER), in that order,
    without destroying bytes HEAD does not store.

    The last check of the writer (_unstored_bytes in _copy_back) runs before
    these writes, and a process that does not hold the publication lock can
    still write into the writer after it. So each file already at a path is
    first renamed into a retention directory beside the writer
    (<writer>.displaced-*/files/<path>), an atomic move that no later open of
    the path reaches, and read there (_displaced_bytes). A file holding
    bytes HEAD does not store stops the publication: nothing more is
    written, and the error names where it was moved. A file holding stored
    bytes stays there too, recorded in displaced.json with its SHA-256 and
    size, because a process that still has it open, or reopens it through a
    handle, can write to it after any comparison; a later publication
    empties it once it is unchanged and no other process has it open
    (prune_displaced), and removes it, by then empty, only while no other
    process has it open (_unlink_if_empty). The
    directories under the retention directory are made and entered through
    descriptors, refusing symbolic links (_directory_under). A new file is
    written whole outside the writer and linked in exclusively
    (_install_exclusive), so one that appears at its path after the move is
    not replaced either, and a kill part way through a write leaves the path
    absent, not holding part of the file. What was already written, and
    such a file, are then set aside by set_aside_render_leftovers when the
    scorecard step ends.
    """
    names = [*writes, *removals]
    # Before any path changes, so whatever this leaves behind is on record
    # for set_aside_render_leftovers. A written path is absent between the
    # move below and the exclusive create, and stays absent when a refusal
    # comes in between, so its removal is recorded with its new bytes.
    _record_render_provenance(writer, [*((name, None) for name in names), *writes.items()])
    entries = _head_entries(writer, names)
    algorithm = _git_stored(writer, "rev-parse", "--show-object-format").decode().strip()
    retention = _Retention(writer)
    staging: Path | None = None
    try:
        for name in names:
            target = writer / name
            if os.path.lexists(target):
                directory, leaf, aside = retention.move(name)
                try:
                    stored = _displaced_bytes(writer, name, directory, leaf, entries.get(name), algorithm)
                except _DisplacedUnstored as error:
                    raise MirrorRefused(
                        f"ledger writer path {name} {error}; they were moved to {aside} and nothing more "
                        "was written; what was written is kept for recovery (render outputs under "
                        "refs/heads/recovery/render-*, anything else in the writer)") from None
                finally:
                    os.close(directory)
                if stored is not None:
                    retention.keep(name, *stored)
            if name not in writes:
                continue
            if staging is None:
                staging = _staging_directory(writer)
            try:
                _install_exclusive(staging, target, writes[name])
            except FileExistsError as error:
                raise MirrorRefused(
                    f"ledger writer path {name} was written by another process while this publication "
                    "replaced it; it was not replaced and nothing more was written; it and what was "
                    "written are kept for recovery (render outputs under refs/heads/recovery/render-*, "
                    "anything else in the writer)") from error
    finally:
        retention.close()
        if staging is not None:
            _remove_empty_directories(staging)


def _require_stored_bytes(writer: Path) -> None:
    """Refuse a writer holding bytes its HEAD does not store (_unstored_bytes).

    Run when a publication starts, under the lock and after the status,
    overlay and sparse checks and before the index bits are cleared
    (_clear_index_bits), so no later checkout, retry, set-aside or
    regeneration in that publication can lose bytes that are in no commit.
    _copy_back runs the same check again after the scorecard tool, the one
    long step, and the regeneration and latest.json then replace files only
    through _replace_writer_files, which moves aside and keeps any unstored
    bytes written after that check. The production writer holds none; its
    1 GB of stored files hash in about a second.
    """
    found = _unstored_bytes(writer)
    if found:
        shown = "; ".join(found[:20]) + (f"; and {len(found) - 20} more" if len(found) > 20 else "")
        raise MirrorRefused(
            f"ledger writer holds bytes its HEAD does not store, which a checkout or the scorecard "
            f"regeneration could overwrite with no ref left: {shown}; nothing was published; preserve "
            f"and remove them before retrying: {writer}")


@contextlib.contextmanager
def publication_writer(
    parent: Path, *, net_wrapper: str, lifecycle_deadline_ns: int | None = None,
    cleaned: list[str] | None = None,
) -> Iterator[tuple[Path, Path, int]]:
    if lifecycle_deadline_ns is not None:
        _verify_lifecycle_worker(lifecycle_deadline_ns)
    ledger = series.ledger_root(parent)
    common = Path(_git(ledger, "rev-parse", "--path-format=absolute", "--git-common-dir"))
    writer = series.shared_parent_root(parent) / WRITER_RELATIVE
    with (common / PUBLICATION_LOCK).open("a+b") as lock:
        # Direct library callers retain their existing finite wait. Only the
        # production worker with a verified whole-lifecycle reaper may wait for
        # its remaining shared deadline; this budget never restarts per retry.
        deadline = (lifecycle_deadline_ns / 1e9 if lifecycle_deadline_ns is not None
                    else time.monotonic() + 60)
        while True:
            try:
                fcntl.flock(lock.fileno(), fcntl.LOCK_EX | fcntl.LOCK_NB)
                break
            except BlockingIOError:
                if time.monotonic() >= deadline:
                    raise MirrorRefused("another ledger publisher still holds the publication lock")
                time.sleep(0.1)
        _network(["git", "fetch", "--quiet", "origin", "main", HISTORY_FETCH_REFSPEC], cwd=ledger, wrapper=net_wrapper)
        if not writer.exists():
            writer.parent.mkdir(parents=True, exist_ok=True)
            _git(ledger, "worktree", "add", "--detach", str(writer), "refs/remotes/origin/main")
        actual = Path(_git(writer, "rev-parse", "--path-format=absolute", "--git-common-dir"))
        if actual != common:
            raise MirrorRefused("ledger writer is not a linked worktree of the selected ledger repository")
        if Path(_git(writer, "rev-parse", "--show-toplevel")) != writer.resolve():
            raise MirrorRefused("ledger writer path is not its Git root")
        # A git command killed in the writer left its index.lock, which every
        # later command refuses.
        healed = clear_stale_writer_index_lock(writer, parent)
        if healed is not None and cleaned is not None:
            cleaned.append(healed)
        # A publisher killed after its render was copied back left only
        # render output; anything else is still refused below.
        outcome = set_aside_render_leftovers(writer, parent)
        if outcome is not None and cleaned is not None:
            cleaned.append(outcome)
        _require_clean(writer)
        _refuse_history_overlay(writer)
        _refuse_sparse_checkout(writer)
        _require_stored_bytes(writer)
        # Nothing in the writer differs from HEAD now, so nothing earlier
        # recorded can vouch for a later change.
        _forget_render_provenance(writer)
        _clear_index_bits(writer)
        set_aside_stranded_derived_commit(writer)
        prune_displaced(writer)
        yield ledger, writer, lock.fileno()
        # Review of a3f62cfee1: a successful publication left on record each
        # path it rewrote with the bytes HEAD already stored (no commit
        # changed it, so none dropped it), and two publications later an
        # operator's removal of SCORECARD.md was cleaned as render output. A
        # writer that ends the publication clean holds nothing the record
        # could vouch for. A failure keeps it for the next run's cleanup.
        with contextlib.suppress(MirrorRefused, OSError):
            if not _worktree_changes(writer):
                _forget_render_provenance(writer)


def recover_spooled_commit(writer: Path, parent: Path) -> str | None:
    """Retry a failed raw append only when the exact added rows remain spooled."""
    # First, before the ancestry check: a graft can make an unpublished commit
    # read as an ancestor of main, and the caller's checkout to main after a
    # None here would drop its rows with no ref left. The checks below also
    # mix views: the diff reads through replacements, the snapshots read
    # stored objects, so a commit whose stored tree adds nothing could pass
    # while the checkout held an unspooled row, which the reset would discard.
    _refuse_history_overlay(writer)
    head = _git(writer, "rev-parse", "HEAD")
    remote = _git(writer, "rev-parse", "refs/remotes/origin/main")
    # After a squash (snapshot_tips) a published HEAD is no ancestor of main,
    # and an unpublished one shares no history with it; both are answered
    # through the old tip the snapshot folds in.
    if _published(writer, head, remote):
        return None
    base = _published_base(writer, head, remote)
    if base is None:
        raise MirrorRefused(f"ledger writer HEAD {head} shares no history with main or any "
                            "snapshot main folds in; retained for recovery")
    paths = _git(writer, "diff", "--name-only", base, head).splitlines()
    def shard(path):
        # Any part of a month the publisher writes, not only the month's own file.
        return series.parse_shard_part_path(path) is not None
    def quarantine_shard(path):
        parts = Path(path).parts
        return (len(parts) == 5 and parts[:2] == ("quarantine", "series")
                and publisher._store_shard("series-quarantine", parts[2], parts[3],
                                           Path(parts[4]).stem) == path)
    def quarantine_input(path):
        # A file a v3 refusal names (series.quarantine_input_path).
        named = series._QUARANTINE_REF_RE.fullmatch(path)
        return named is not None and named.group(2).startswith(named.group(1))
    parity = series.parity_ledger_module()
    def parity_shard(path):
        # parity/, parity-runs/ and quarantine/retractions/: everything the
        # parity store's batches append to.
        return parity.is_shard_path(path)
    if not paths or any(not (shard(path) or quarantine_shard(path) or quarantine_input(path)
                             or parity_shard(path))
                        for path in paths):
        raise MirrorRefused(f"ledger writer has unpublished derived or unknown work at {head}; retained for recovery")
    if any(parity_shard(path) for path in paths):
        # A joint commit's parity half obeys the same rule: nothing published
        # changed, and every added event is still pending in the parity spool.
        # Parity batches are written whole and never change, so they are read
        # without the spool lock; a batch spooled meanwhile only adds events
        # that are waiting.
        #
        # Only the parity shards the commit changed are read, at both commits:
        # an unchanged shard holds the same events at both, so it cannot fail
        # either check. Reading the whole published parity ledger twice held
        # two complete namespaces at once, 28.3 GB of anonymous memory in the
        # publication of tickhub-ops-2 b62592b1 on 2026-10-11 at 1.68 GB of
        # parity text (https://github.com/rrnewton/dev-hermit/issues/554). One
        # case is stricter than before: an added event whose id is already
        # published in an unchanged shard now counts as added, so it must be
        # waiting in the spool or the commit is retained.
        changed = [path for path in paths if parity_shard(path)]
        try:
            was = parity.published_parity_shards(writer, base, changed).events
            now = parity.published_parity_shards(writer, head, changed).events
            batches = parity.batch_paths(series.parity_spool_dir(parent))
        except parity.ParityLedgerError as error:
            raise MirrorRefused(f"cannot check unpublished parity at {head}: {error}") from error
        waiting = {}
        for batch in batches:
            # A batch that cannot be read is skipped, as run_refusals does; the
            # publisher names it as refused and it stays spooled, so refusing
            # here would block every later run. This still fails closed: an
            # added event whose only copy is in an unreadable batch is not
            # waiting, so the check below refuses the commit.
            try:
                marker, rows = parity.read_batch(batch)
            except parity.ParityLedgerError:
                continue
            waiting.update((event["event_id"], event) for event in (marker, *rows))
        if (any(now.get(key) != value for key, value in was.items())
                or any(waiting.get(key) != value for key, value in now.items() if key not in was)):
            raise MirrorRefused(f"unpublished ledger commit {head} is not an unchanged spooled parity append; retained")
    if any(quarantine_shard(path) or quarantine_input(path) for path in paths):
        # Same rule for refused inputs: every published record is unchanged and
        # every added one is still pending in the local quarantine spool, as
        # the publisher writes it. A v3 row's input files are only added, and
        # each holds exactly the bytes of the spooled record it came from.
        stored = set()
        for line in _git(writer, "diff", "--name-status", "--no-renames", base, head,
                         "--", series.QUARANTINE_INPUTS_ROOT).splitlines():
            status, _, path = line.partition("\t")
            if status != "A" or not quarantine_input(path):
                raise MirrorRefused(
                    f"unpublished ledger commit {head} changes quarantine input {path}; retained")
            stored.add(path)
        try:
            with series.series_spool_transition_lock(
                series.quarantine_dir(parent), exclusive=True,
                timeout_seconds=series.QUARANTINE_LOCK_TIMEOUT_SECONDS,
            ):
                pending_quarantine, _ = publisher._store_snapshot(
                    "series-quarantine", series.quarantine_dir(parent))
            _, old_quarantine = series.published_quarantine_at_repo(writer, base)
            _, new_quarantine = series.published_quarantine_at_repo(writer, head)
        except series.LedgerError as error:
            raise MirrorRefused(f"cannot check unpublished quarantine at {head}: {error}") from error
        was = {record["event_id"]: record for record in old_quarantine}
        now = {record["event_id"]: record for record in new_quarantine}
        waiting = {record["event_id"]: record
                   for records in pending_quarantine.values() for record in records}
        expected: dict[str, bytes] = {}
        def spooled(key, value):
            record = waiting.get(key)
            if record is None:
                return False
            if value == record:
                return True
            slim, files = series.slim_quarantine_record(record)
            expected.update(files)
            return value == slim
        if (any(now.get(key) != value for key, value in was.items())
                or not all(spooled(key, value) for key, value in now.items() if key not in was)):
            raise MirrorRefused(f"unpublished ledger commit {head} is not an unchanged spooled quarantine append; retained")
        try:
            present = series._ledger_files(writer, head, expected)
        except (OSError, subprocess.SubprocessError, series.LedgerError) as error:
            raise MirrorRefused(f"cannot read quarantine input files at {head}: {error}") from error
        if not stored <= set(expected) or any(present.get(path) != content
                                              for path, content in expected.items()):
            raise MirrorRefused(
                f"unpublished ledger commit {head} does not hold exactly the input files "
                "its spooled quarantine rows name; retained")
    with contextlib.ExitStack() as held:
        old = series._published_snapshot_at_repo(writer, base, repository=REMOTE_URL)
        held.callback(old.close)
        current = series._published_snapshot_at_repo(writer, head, repository=REMOTE_URL)
        held.callback(current.close)
        before = {row["event_id"]: row for row in old.rows}
        after = {row["event_id"]: row for row in current.rows}
        pending, _ = publisher.read_series_spool(series.spool_dir(parent))
        spooled = {row["event_id"]: row for rows in pending.values() for row in rows}
        if (any(after.get(key) != value for key, value in before.items())
                or any(spooled.get(key) != value for key, value in after.items() if key not in before)):
            raise MirrorRefused(f"unpublished ledger commit {head} is not an unchanged spooled append; retained")
        # A durable ref keeps the refused attempt inspectable after the existing
        # publisher re-merges these same rows onto freshly fetched main.
        recovery = f"refs/heads/recovery/cell-series-{head}"
        existing = _run(["git", "-C", str(writer), "rev-parse", "--verify", "--quiet", recovery], check=False)
        if existing.returncode not in {0, 1} or (existing.returncode == 0 and existing.stdout.strip() != head):
            raise MirrorRefused(f"recovery ref {recovery} already names different work")
        old.verify()
        current.verify()
        _git(writer, "update-ref", recovery, head, head if existing.returncode == 0 else "0" * 40)
        return recovery


def _preserve_failed_series_commit(writer: Path, parent: Path) -> BaseException | None:
    """Keep a refused series commit inspectable before the writer moves on.

    A push that failed leaves the writer on an unpublished commit. Publishing
    the quarantine checks out published main, so the commit is first named by
    the same durable recovery ref a later run would give it. Returns the error
    if the commit is not an unchanged spooled append; the writer then must not
    move and nothing more publishes from it.
    """
    try:
        head = _git(writer, "rev-parse", "HEAD")
        if _published(writer, head):
            return None
        recover_spooled_commit(writer, parent)
    except (MirrorRefused, publisher.PublishRefused, subprocess.SubprocessError, OSError) as error:
        return error
    return None


def _parity_result(*, joint: bool) -> dict[str, Any]:
    return {"schema": PARITY_PUBLICATION_SCHEMA, "published": 0, "already_published": 0,
            "commit": None, "joint": joint, "batches": [], "refused": [], "error": None,
            "pending_batches": None, "historic_invalid": None, "run": None}


def _absorb_parity(result: dict[str, Any], outcome: Mapping[str, Any] | None,
                   commit: str | None) -> None:
    """Copy the publisher's parity sub-report into the mirror's parity record."""
    if outcome is None:
        return
    result["published"] = outcome.get("published", 0)
    result["already_published"] = outcome.get("already_published", 0)
    result["batches"] = list(outcome.get("batches", []))
    result["refused"] = list(outcome.get("refused", []))
    if outcome.get("historic_invalid") is not None:
        result["historic_invalid"] = outcome["historic_invalid"]
    if outcome.get("error"):
        result["error"] = str(outcome["error"])
    if result["published"]:
        result["commit"] = commit


def publish_parity_alone(ledger: Path, writer: Path, result: dict[str, Any], *,
                         parity_spool: Path, net_wrapper: str, attempts: int,
                         blocked: BaseException | str | None) -> None:
    """Publish the parity batches in their own commit after the series failed.

    The publisher takes the parity spool lock itself, only to read the
    batches and to move the published ones. A failure is recorded, never
    raised, and the writer is returned to published main, the same as
    ``publish_quarantine``.
    """
    result["joint"] = False
    if blocked is not None:
        result["error"] = f"not attempted: {blocked}"
        return
    try:
        outcome = publisher.publish(
            writer, parity_spool, source_root=ledger, store="parity",
            attempts=attempts, network_wrapper=net_wrapper,
        )
        _absorb_parity(result, outcome.get("parity"), outcome.get("commit"))
    except (publisher.PublishRefused, subprocess.SubprocessError, OSError) as error:
        result["error"] = f"{type(error).__name__}: {error}"
        try:
            _git(writer, "checkout", "--quiet", "--force", "--no-recurse-submodules", "--detach",
                 "refs/remotes/origin/main")
            _git(writer, "clean", "-fdq", "--", "parity", "parity-runs",
                 series.parity_ledger_module().QUARANTINE_RETRACTION_ROOT)
        except (subprocess.SubprocessError, OSError) as restore_error:
            result["error"] += f"; writer restore FAILED: {restore_error}"


def count_parity(result: dict[str, Any], parity_spool: Path, writer: Path | None,
                 run_id: str | None, *, deferred_dir: Path | None = None) -> None:
    """Pending parity batches, and where this validate run's parity stands.

    ``run.state`` is ``published`` (its run marker is on fetched main),
    ``pending`` (its batch is spooled, not published), ``deferred`` (an
    append-parity call for the run waits in ``deferred_dir``,
    ``series.parity_deferred_dir``, because the parity spool did not take it;
    the next series publication replays it), ``refused`` (append-parity
    refused the run and recorded a ``parity.refusal``, whose message is
    ``run.refusal``), or ``absent`` (no marker, no deferred call and no
    refusal: append-parity was never called for this run). ``absent`` covers
    an older hermit AND a run with no parity cells in scope, because hermit's
    callers skip append-parity on an empty selection; the two are not told
    apart. The state is ``null`` when the ledger could not be read.
    ``historic_invalid`` counts published parity entries the index could not
    use, among the shards this status read (see
    `parity_ledger.published_parity_for_batches`).
    """
    parity = series.parity_ledger_module()
    try:
        result["pending_batches"] = len(parity.batch_paths(parity_spool))
        if run_id is None:
            return
        identity = parity.run_event_id("validate", run_id)
        run = {"producer": "validate", "run_id": run_id, "state": None, "records": None,
               "refusal": None}
        result["run"] = run
        pending = sorted(Path(parity_spool).glob(f"*__{identity}.jsonl"))
        published_events = None
        if writer is not None:
            # The run's marker and refusals are run-level events; with the
            # run's own pending batches this reads what the run's status needs,
            # not the whole ledger (https://github.com/rrnewton/dev-hermit/issues/554).
            published = parity.published_parity_for_batches(writer, "refs/remotes/origin/main", pending)
            result["historic_invalid"] = len(published.invalid)
            published_events = published.events
            marker = published.events.get(identity)
            if marker is not None:
                run.update(state="published", records=marker["records"])
                return
        if pending:
            run.update(state="pending", records=parity.read_batch(pending[0])[0]["records"])
            return
        # A deferred call is named for its run (series.defer_parity_call), so
        # it counts here even when its file cannot be read.
        if deferred_dir is not None and any(Path(deferred_dir).glob(f"{identity}.*.jsonl")):
            run["state"] = "deferred"
            return
        refusals = parity.run_refusals(parity_spool, "validate", run_id, published=published_events)
        if refusals:
            run.update(state="refused", refusal=refusals[-1]["error"])
        elif writer is not None:
            run["state"] = "absent"
    except (OSError, parity.ParityLedgerError) as error:
        count_error = f"cannot count parity: {type(error).__name__}: {error}"
        result["error"] = count_error if result["error"] is None else f"{result['error']}; {count_error}"


def publish_series(parent: Path, ledger: Path, writer: Path, *, net_wrapper: str, attempts: int,
                   run_id: str | None = None, series_rows: bool = True) -> dict[str, Any]:
    """Publish the spooled series rows, then the refused inputs, whatever the rows did.

    With ``series_rows=False`` only the refused inputs publish (the backlog
    sweep's ``series-quarantine`` store). A refused series batch, or a series
    push that failed, no longer strands the quarantine: it still publishes, and
    the failure is raised afterwards as ``SeriesPublicationFailed`` carrying the
    quarantine outcome. The only case in which the quarantine does not publish
    is a writer holding work that this tool cannot account for, because
    publishing would move the writer off that work. Its records are still
    counted and reported as not attempted.

    Parity batches (``series.parity_spool_dir``) publish in the SAME commit as
    the series rows. A parity batch that cannot publish is listed in
    ``parity.refused`` and stays spooled; the series rows publish regardless.
    When the series rows fail, the parity batches publish alone. The
    ``--quarantine-only`` sweep leaves them alone. The outcome is
    ``report["parity"]`` (``parity-publication/v1``).

    Before it publishes rows, it replays the append-parity calls that the
    parity spool did not take (``series.replay_deferred_parity``), so their
    batches join this publication. The replay's outcome is
    ``report["parity_deferred"]``, or the raised failure's ``parity_deferred``
    when the rows fail; a call it could not replay stays deferred, is named in
    a warning, and is replayed by the next publication.

    A readmission of refused inputs (``series.readmit_quarantined``) that
    succeeds leaves its classification of what it still refuses for the
    health gate (``series.write_readmission_summary``); failing to write it
    is a warning.
    """
    report: dict[str, Any] = (
        {"published": 0, "commit": None, "attempts": 0, "skipped": "quarantine only"}
        if not series_rows else {})
    series_error: BaseException | None = None
    blocked: BaseException | None = None
    parity_spool = series.parity_spool_dir(parent)
    parity_result = _parity_result(joint=series_rows)
    readmission: dict[str, Any] | None = None
    parity_deferred: dict[str, Any] | None = None
    if series_rows:
        # Refused inputs that the current writer admits join this publication
        # (series.readmit_quarantined, which takes the spool lock itself). A
        # failure here is reported and does not stop the spooled rows from
        # publishing.
        phase_started = time.monotonic()
        try:
            readmission = series.readmit_quarantined(parent)
        except (series.LedgerError, OSError, subprocess.SubprocessError) as error:
            readmission = {"error": f"{type(error).__name__}: {error}"}
            print(f"mirror: WARNING: replaying refused series inputs failed: "
                  f"{readmission['error']}", file=sys.stderr)
        else:
            # What it left refused is what the series_quarantine health gate
            # reads. Unwritten, the last summary ages until the gate reports
            # that it cannot tell.
            try:
                series.write_readmission_summary(parent, readmission)
            except (series.LedgerError, OSError) as error:
                print(f"mirror: WARNING: keeping the readmission summary failed: "
                      f"{type(error).__name__}: {error}", file=sys.stderr)
        _phase("readmission", phase_started)
        # So do the append-parity calls the parity spool did not take. The
        # replay waits for the parity spool lock as long as the publisher does.
        phase_started = time.monotonic()
        try:
            parity_deferred = series.replay_deferred_parity(
                parent, timeout_seconds=publisher.SPOOL_LOCK_TIMEOUT_SECONDS)
        except (series.LedgerError, OSError, subprocess.SubprocessError) as error:
            parity_deferred = {"error": f"{type(error).__name__}: {error}"}
            print(f"mirror: WARNING: replaying deferred parity calls failed: "
                  f"{parity_deferred['error']}", file=sys.stderr)
        else:
            for kept in parity_deferred["kept"]:
                print(f"mirror: WARNING: the deferred parity call {kept['file']} stays deferred: "
                      f"{kept['reason']}", file=sys.stderr)
        _phase("parity-replay", phase_started)
    # The publisher takes the series and parity spool locks itself, only to read
    # the batches and to move the published ones (publisher.publish).
    phase_started = time.monotonic()
    try:
        recovery = recover_spooled_commit(writer, parent)
    except (MirrorRefused, publisher.PublishRefused, subprocess.SubprocessError) as error:
        series_error = blocked = error
    else:
        try:
            _git(writer, "checkout", "--no-recurse-submodules", "--detach", "refs/remotes/origin/main")
            if series_rows:
                report = publisher.publish(
                    writer, series.spool_dir(parent), source_root=ledger,
                    store="series", attempts=attempts, network_wrapper=net_wrapper,
                    parity_spool=parity_spool,
                )
                _absorb_parity(parity_result, report.pop("parity", None), report.get("commit"))
            if recovery:
                report["recovered_ref"] = recovery
        except (publisher.PublishRefused, subprocess.SubprocessError) as error:
            series_error = error
            blocked = _preserve_failed_series_commit(writer, parent)
    _phase("series-publish", phase_started)
    if not series_rows:
        parity_result["error"] = "not attempted: quarantine only"
    elif series_error is not None:
        publish_parity_alone(ledger, writer, parity_result, parity_spool=parity_spool,
                             net_wrapper=net_wrapper, attempts=attempts, blocked=blocked)
    count_parity(parity_result, parity_spool, writer if blocked is None else None, run_id,
                 deferred_dir=series.parity_deferred_dir(parent))
    phase_started = time.monotonic()
    quarantine = publish_quarantine(
        parent, ledger, writer, net_wrapper=net_wrapper, attempts=attempts, run_id=run_id,
        blocked=blocked,
    )
    _phase("quarantine-publish", phase_started)
    if series_error is not None:
        raise SeriesPublicationFailed(str(series_error), quarantine, parity_result,
                                      parity_deferred) from series_error
    report["quarantine"] = quarantine
    report["parity"] = parity_result
    if readmission is not None:
        report["readmission"] = readmission
    if parity_deferred is not None:
        report["parity_deferred"] = parity_deferred
    return report


def _quarantine_records(path: Path) -> list[dict]:
    return [record for file in sorted(path.glob("*.jsonl"))
            for record in series._read_quarantine_file(file)]


def publish_quarantine(parent: Path, ledger: Path, writer: Path | None, *, net_wrapper: str,
                       attempts: int, run_id: str | None = None,
                       blocked: BaseException | str | None = None) -> dict[str, Any]:
    """Publish refused series inputs and count them; never undo the series.

    The result is the machine-readable count that start_unit copies into the
    run's ``ledger_mirror.raw_publication.quarantine``, on success and on
    failure. A failure is recorded in ``error`` rather than raised. The refused
    inputs stay in the local spool (``pending_records``) for the next
    publication, and ``ledger_spool_backlog --store series-quarantine`` alarms
    on them. The writer is returned to published main so the scorecard never
    binds an unpushed quarantine commit. With ``blocked`` (or no writer),
    nothing is published and only the local counts are taken. ``run.published``
    is then None, because nothing was read from the ledger.
    """
    directory = series.quarantine_dir(parent)
    result: dict[str, Any] = {
        "schema": "series-quarantine-publication/v1", "published": 0,
        "already_published": 0, "commit": None, "error": None,
        "pending_records": None, "held_records": None, "corrupt_files": None,
        "set_aside": [], "run": None,
    }
    if writer is None or blocked is not None:
        result["error"] = f"not attempted: {blocked if blocked is not None else 'no ledger writer'}"
    else:
        try:
            with series.series_spool_transition_lock(
                directory, exclusive=True, timeout_seconds=series.QUARANTINE_LOCK_TIMEOUT_SECONDS,
            ):
                outcome = publisher.publish(
                    writer, directory, source_root=ledger, store="series-quarantine",
                    attempts=attempts, network_wrapper=net_wrapper,
                )
            result["published"] = outcome.get("published", 0)
            result["already_published"] = outcome.get("already_published", 0)
            result["commit"] = outcome.get("commit")
            result["set_aside"] = list(outcome.get("set_aside", []))
        except (publisher.PublishRefused, subprocess.SubprocessError, OSError, series.LedgerError) as error:
            result["error"] = f"{type(error).__name__}: {error}"
            try:
                _git(writer, "checkout", "--quiet", "--force", "--no-recurse-submodules", "--detach",
                     "refs/remotes/origin/main")
                _git(writer, "clean", "-fdq", "--", "quarantine")
            except (subprocess.SubprocessError, OSError) as restore_error:
                result["error"] += f"; writer restore FAILED: {restore_error}"
    try:
        pending = _quarantine_records(directory)
        result["pending_records"] = len(pending)
        result["held_records"] = len(_quarantine_records(directory / series.QUARANTINE_HELD_SUBDIR))
        corrupt = directory / series.QUARANTINE_CORRUPT_SUBDIR
        result["corrupt_files"] = len(list(corrupt.glob("*.jsonl"))) if corrupt.is_dir() else 0
        if run_id is not None:
            waiting = [record for record in pending if record.get("run_id") == run_id]
            if result["error"] is not None and result["error"].startswith("not attempted"):
                result["run"] = {"run_id": run_id, "published": None, "withheld": None,
                                 "unpublished": len(waiting),
                                 "records": len({series.quarantine_identity(record)
                                                 for record in waiting})}
            else:
                _, published = series.published_quarantine_at_repo(writer, "refs/remotes/origin/main")
                mine = [record for record in published if record.get("run_id") == run_id]
                result["run"] = {
                    "run_id": run_id,
                    "published": len(mine),
                    "withheld": sum(record.get("withheld") is not None for record in mine),
                    "unpublished": len(waiting),
                    "records": len({record["event_id"] for record in mine}
                                   | {series.quarantine_identity(record) for record in waiting}),
                }
    except (subprocess.SubprocessError, OSError, series.LedgerError) as error:
        count_error = f"cannot count quarantine: {type(error).__name__}: {error}"
        result["error"] = count_error if result["error"] is None else f"{result['error']}; {count_error}"
    return result


_PublishedRows = series.PublishedRows


def _rows_by_event_id(rows: Sequence[Any]) -> dict[bytes, bytes]:
    """Canonical text by event id, for rows held as text or as objects."""
    if isinstance(rows, _PublishedRows):
        return rows.by_event_id()
    return {row["event_id"].encode(): json.dumps(row, sort_keys=True, separators=(",", ":"),
                                                 ensure_ascii=False).encode()
            for row in rows}


def _max_emitted_at(rows: Sequence[Any]) -> str | None:
    if isinstance(rows, _PublishedRows):
        return rows.max_emitted_at
    stamps = [row["emitted_at"] for row in rows]
    return max(stamps) if stamps else None


def _write_snapshot(snapshot: Mapping[str, Any], path: Path) -> str:
    """Write series._snapshot_bytes(snapshot) to PATH and return its sha256.

    The same bytes, streamed: rows held as text are written as they are, and
    the payload (the ledger's size) is never one object in memory.
    """
    _verify_published_snapshot(snapshot)
    hasher = hashlib.sha256()
    with path.open("wb") as out:
        def write(data: bytes) -> None:
            hasher.update(data)
            out.write(data)

        write(b"{")
        for position, key in enumerate(sorted(snapshot)):
            write((b"," if position else b"") + json.dumps(key, ensure_ascii=False).encode() + b":")
            value = snapshot[key]
            if key == "rows" and isinstance(value, _PublishedRows):
                write(b"[")
                for index, text in enumerate(value.texts()):
                    if index:
                        write(b",")
                    write(text)
                write(b"]")
            else:
                write(json.dumps(value, sort_keys=True, separators=(",", ":"), ensure_ascii=False).encode())
        write(b"}\n")
    _verify_published_snapshot(snapshot)
    return hasher.hexdigest()


def _published_source(writer: Path, ref: str = "HEAD") -> dict[str, Any]:
    """The published series commit and tree at REF, without reading its rows.

    For a publish-only run, whose report names the commit and tree it left
    main at and nothing reads a row count from (ci-hub/ledger/publish_backlog.py,
    nightly_maintenance.py). _published_snapshot reads and checks every row:
    on the 2026-10-12 ledger (1,425,028 rows in 9,804 series blobs) that was
    191.6 s of a 218.7 s warm publish-only run, measured offline with
    cProfile, and each backlog sweep runs it once per store.
    """
    commit = _git(writer, "rev-parse", f"{ref}^{{commit}}")
    return {"source": {"repository": REMOTE_URL, "commit": commit,
                       "tree": series._series_tree_at(writer, commit)}}


class _PublishedSeriesSnapshot(dict):
    """The native wire mapping plus the held original-file authority.

    Ownership is a Python attribute, never a native snapshot field. An explicit
    archive handle belongs to its caller and is not closed by this mapping.
    Keep only that handle after the large row text is released: proof and
    metadata still depend on the exact authenticated input population.
    """

    def __init__(self, value: Mapping[str, Any], *, files: Any, owns_files: bool):
        super().__init__(value)
        self._files = files
        self._owns_files = owns_files

    def verify(self) -> None:
        self._files.verify()

    def close(self) -> None:
        if self._owns_files:
            self._owns_files = False
            self._files.close()


def _verify_published_snapshot(snapshot: Mapping[str, Any]) -> None:
    if isinstance(snapshot, _PublishedSeriesSnapshot):
        try:
            snapshot.verify()
        except (ValueError, OSError, subprocess.SubprocessError) as error:
            raise MirrorRefused(f"published archive inputs are unavailable: {error}") from error


def _close_published_snapshot(snapshot: Mapping[str, Any]) -> None:
    if isinstance(snapshot, _PublishedSeriesSnapshot):
        snapshot.close()


@contextlib.contextmanager
def _published_snapshot_scope(writer: Path, ref: str = "HEAD"):
    snapshot = _published_snapshot(writer, ref)
    try:
        yield snapshot
    finally:
        _close_published_snapshot(snapshot)


def _published_snapshot(writer: Path, ref: str = "HEAD", *, files: Any = None) -> dict[str, Any]:
    commit = _git(writer, "rev-parse", f"{ref}^{{commit}}")
    owned_files = files is None
    files = series.file_archive_snapshot(writer, commit, files=files)
    try:
        rows = series.published_rows_at_repo(writer, commit, repository=REMOTE_URL, files=files)
        return _PublishedSeriesSnapshot({
            "schema": series.SCORECARD_SNAPSHOT_SCHEMA,
            "source": {"repository": REMOTE_URL, "path": series.SCORECARD_SNAPSHOT_SOURCE,
                       "commit": commit, "tree": series._series_tree_at(writer, commit),
                       "published_rows": len(rows), "local_sources": []},
            "rows_read": len(rows), "rows_sha256": rows.digest(),
            "rows": rows,
        }, files=files, owns_files=owned_files)
    except BaseException:
        if owned_files:
            files.close()
        raise


def _committed_tool_source(tool_checkout: Path) -> str:
    """Bind the scorecard and its local Rust dependencies, not unrelated data.

    The paths are the ones the scorecard family compiles, shared with the
    rust tool cache that keys its stored programs on them.
    """
    inputs = rust_tool_cache.SCORECARD_INPUTS
    status = _git(tool_checkout, "status", "--porcelain", "--untracked-files=all", "--", *inputs)
    if status:
        raise MirrorRefused(f"scorecard tool inputs have uncommitted changes:\n{status}")
    # dagrun is a local path dependency in a separately versioned submodule.
    # Its unrelated Python/docs work does not change this Rust tool's identity.
    recorded = _git(tool_checkout, "ls-tree", "HEAD", "--", "agent-utils")
    if recorded:
        fields = recorded.split()
        dependency = tool_checkout / "agent-utils"
        if fields[:2] != ["160000", "commit"] or _git(dependency, "rev-parse", "HEAD^{commit}") != fields[2]:
            raise MirrorRefused("scorecard tool agent-utils dependency differs from its committed gitlink")
        status = _git(dependency, "status", "--porcelain", "--untracked-files=all", "--",
                      "rs/dagrun", "rs/Cargo.toml", "rs/Cargo.lock", "rs/.cargo",
                      "Cargo.toml", "Cargo.lock", ".cargo",
                      "rust-toolchain", "rust-toolchain.toml")
        if status:
            raise MirrorRefused(f"scorecard tool dependency has uncommitted changes:\n{status}")
    return _git(tool_checkout, "rev-parse", "HEAD^{commit}")


class _RowLines:
    """The ``out`` of ``validate_rows._stream_preserved_rows``: the bytes it writes."""

    def __init__(self) -> None:
        self.buffer = io.BytesIO()

    def flush(self) -> None:
        pass


def _row_cache_note(run_id: str, stats: Mapping[str, Any], answered: bool) -> str:
    """One "ledger publication:" line saying how the row replay cache read RUN_ID's rows.

    A publication unit repeats only lines with that prefix from this
    process's stderr, so this is how a launcher log shows whether the cache
    answered (and how much it had to parse) or the streamed reader ran.
    """
    path = stats.get("path", "unknown")
    head = f"ledger publication: row replay cache: {run_id}: {path}"
    if answered:
        parts = [f"{stats.get('new_lines', 0)} new line(s)",
                 f"{stats.get('new_bytes', 0)} new byte(s)",
                 f"{stats.get('shards', 0)} shard(s)"]
        refused = stats.get("refused_entries") or {}
        if refused:
            parts.append(f"{len(refused)} entry(ies) refused")
        if "seconds" in stats:
            parts.append(f"{stats['seconds']} s")
        return f"{head}: " + ", ".join(parts)
    reason = stats.get("reason")
    detail = f" ({reason})" if reason and reason != path else ""
    return f"{head}; the streamed reader reads the ledger{detail}"


def _run_rows(parent: Path, run_id: str) -> list[dict] | None:
    """The rows of ``validate_rows.rows(preserve_admission=True)`` naming RUN_ID, in order.

    First from the row replay cache (``row_replay_cache.run_row_lines``),
    which keeps each event's checks and projected row line by the bytes of
    its shard and recomputes the union, corrections and dedup every time, so
    only lines appended since its last read are parsed and checked. When the
    cache cannot vouch for the ledger, read with the streamed reader, which
    runs the same checks over every event, holds an index instead of the
    event union, and projects only the rows SELECT accepts. A row is stored
    with the workspace token in place of the state root, so one whose stored
    run id holds the token is kept too and compared once restored. None when
    that reader declines a source; the caller then reads the whole union.
    """
    import row_replay_cache
    import validate_rows

    hook = validate_rows.qualifying_receipt.AdmissionJsonObject
    stats: dict = {}
    lines = row_replay_cache.run_row_lines(parent, run_id, stats=stats)
    print(_row_cache_note(run_id, stats, lines is not None), file=sys.stderr)
    if lines is None:
        def select(row: Mapping[str, Any]) -> bool:
            stored = row.get("run_id")
            return stored == run_id or (isinstance(stored, str)
                                        and validate_rows.WORKSPACE_TOKEN in stored)

        out = _RowLines()
        if not validate_rows._stream_preserved_rows(out, root=parent, select=select):
            return None
        lines = out.buffer.getvalue().splitlines()
    parsed = (json.loads(line, object_pairs_hook=hook) for line in lines)
    return [row for row in parsed if row.get("run_id") == run_id]


def finalized_producer_row(parent: Path, run_id: str, target: str, checkout: Path) -> bytes:
    """Resolve and recheck one run through the existing canonical ledger reader."""
    import validate_rows

    # One read of the ledger serves both selections below, and both look only
    # at rows of RUN_ID: _select_finalized_row checks the correction group of
    # one run. Every ledger event is still checked: the row replay cache keeps
    # each line's shared-checker and per-event verdicts by the bytes of its
    # shard, checks the lines appended since, and recomputes the union and the
    # corrections; when it cannot vouch, the streamed reader checks every event
    # through the shared checker and refuses as rows() would. Holding the whole
    # union cost 5 to 9 GB in each publication
    # (https://github.com/rrnewton/dev-hermit/issues/554), and a publication
    # makes this read up to four times.
    every = _run_rows(parent, run_id)
    if every is None:
        every = validate_rows.rows(root=parent, preserve_admission=True)
    candidates = [row for row in every
                  if validate_rows._ledger_team(row.get("repo")) == "hermit"
                  and row.get("run_id") == run_id]
    current = validate_rows.current_validation_run_rows(candidates)
    if len(current) != 1:
        raise MirrorRefused("finalized producer run has no unique canonical terminal")
    candidate = current[0]
    if candidate.get("commit") != target or candidate.get("cwd") != str(checkout.resolve()):
        raise MirrorRefused("finalized producer run differs from the measured source identity")
    # The current-row projection alone is diagnostic: this exact query checks
    # the complete correction chain before accepting its selected terminal.
    selected = validate_rows._select_finalized_row(
        every, **{name: candidate.get(name) for name in
                  ("repo", "run_id", "record_id", "commit", "cwd", "started_at")},
    )
    return (validate_rows.qualifying_receipt.admission_json_dumps(
        selected, sort_keys=True) + "\n").encode()


def refuse_non_cargo_payload(encoded_row: bytes) -> None:
    """Keep a Buck full run's cells out of the scorecard.

    Such a run executes the release E2E payload, which has no debug assertions
    and no overflow checks. Its cells are not the debug observations the
    scorecard records, so the finalized publication refuses before any proof
    or scorecard work. Raw rows still publish. The builder comes from
    qualifying_receipt.release_builder_of: a row with neither release_builder
    nor e2e_payload is a Cargo run only if it is a Reverie row or a Hermit row
    finished before release_builder_required_from_finished_at, and every other
    row without a consistent pair names no builder and refuses as well.
    """
    import validate_rows
    receipt = validate_rows.qualifying_receipt
    row = _private_row(encoded_row)
    if receipt.release_builder_of(row, receipt.active()) != "cargo":
        raise MirrorRefused(
            "finalized run used a non-Cargo release payload "
            f"(release_builder={row.get('release_builder')!r}, "
            f"e2e_payload={row.get('e2e_payload')!r}); its cells are not scorecard observations"
        )


def _row_bytes(row: Mapping[str, Any]) -> bytes:
    import validate_rows
    return (validate_rows.qualifying_receipt.admission_json_dumps(row, sort_keys=True) + "\n").encode()


def _private_row(encoded: bytes) -> dict:
    import validate_rows
    return json.loads(encoded, object_pairs_hook=validate_rows.qualifying_receipt.AdmissionJsonObject)


_PARENT_TOOL_INPUTS = (
        "ci-hub/series", "ci-hub/ledger", "ci-hub/lib",
        "ci-hub/qualifying_receipt.py", "ci-hub/admission_paths.py",
        "ci-hub/ci-hub", "ci-hub/ci-hub.rs",
        "ci-hub/validate/gate_completeness.py", "ci-hub/validate/flake_class.py",
        "ci-hub/validate/flaky_cells.py", "ci-hub/validate/flaky-cells.json",
        "ci-hub/validate/green_class.py",
        "ci-hub/validate/failure_evidence.py", "ci-hub/validate/qualifying-receipt.json",
        "ci-hub/validate/retained-evidence-quarantine.json",
        "ci-hub/health/ledger_spool_backlog.py",
        "scripts/parent-main-write", "scripts/check_commit_attribution.py",
)


def _phase(name: str, started: float) -> None:
    """One stderr line per series-publication phase (publish_backlog.phase_seconds)."""
    print(f"mirror: phase {name} took {time.monotonic() - started:.1f} s",
          file=sys.stderr, flush=True)


def _parent_tool_commit() -> str:
    paths = _PARENT_TOOL_INPUTS
    if _git(TOOL_ROOT, "status", "--porcelain", "--untracked-files=all", "--", *paths):
        raise MirrorRefused("portable proof producer inputs have uncommitted changes")
    return _git(TOOL_ROOT, "rev-parse", "HEAD^{commit}")


def _parent_tool_source_unchanged(prepared_commit: str) -> bool:
    """Whether the producer's inputs are what they were at PREPARED_COMMIT.

    The check guards the producer's SOURCE, not the parent's HEAD. Run from the
    shared parent checkout, HEAD moves with every commit pushed to main (ledger
    appends land every few minutes), and a schema-5 carrier spends 356-643 s
    between preparation and staging. Comparing HEAD refused such a carrier on
    every backlog sweep with "portable proof producer source changed during
    publication" although no input had changed (validate ec16e766 on
    2026-10-11). A move that leaves every input path identical is accepted.
    The manifest still records PREPARED_COMMIT, whose inputs these are.
    """
    current = _parent_tool_commit()  # also refuses uncommitted input changes
    if current == prepared_commit:
        return True
    result = subprocess.run(
        ["git", "-C", str(TOOL_ROOT), "diff", "--quiet", prepared_commit, current,
         "--", *_PARENT_TOOL_INPUTS],
        capture_output=True, check=False)
    return result.returncode == 0


def _namespace_seed(namespace: Any):
    """NAMESPACE's advance seed, or None when it is not a namespace or cannot make one.

    A seed only saves the recheck's full build, so failing to make one costs
    that build and never fails the publication.
    """
    import validate_rows
    if not isinstance(namespace, validate_rows.CommittedValidationNamespace):
        return None
    try:
        return namespace.advance_seed()
    except Exception as error:  # noqa: BLE001 - the recheck builds in full instead
        print(f"ledger publication: committed namespace seed unavailable, the recheck builds in full: "
              f"{type(error).__name__}: {error}", file=sys.stderr)
        return None


def _report_namespace_advance(record: Mapping[str, Any]) -> None:
    """One stderr line per namespace advance, so a seed that always falls back is seen."""
    if record["outcome"] == "advanced":
        print(f"ledger publication: committed namespace advanced {record['from'][:12]} -> "
              f"{record['to'][:12]} in {record['seconds']} s", file=sys.stderr)
    else:
        print(f"ledger publication: committed namespace advance {record['from'][:12]} -> "
              f"{record['to'][:12]} refused after {record['seconds']} s, built in full: "
              f"{record['reason']}", file=sys.stderr)


def _select_published_terminal(source: Path, parent: Path, row: Mapping[str, Any], *,
                               net_wrapper: str, cache: dict[str, Any] | None = None):
    """Select ROW's canonical terminal in the parent's freshly fetched main.

    CACHE["namespace"] is a namespace an earlier selection of this attachment
    built for the same SOURCE and PARENT. A namespace is a function of nothing
    but the commit's ledger trees and PARENT, so when main has not moved since,
    it is selected from again instead of being rebuilt. Rebuilding it was a
    quarter of a carrier attachment: 47.7 s of 195.5 s measured on 2026-10-04
    (https://github.com/rrnewton/dev-hermit/issues/394). The namespace this
    selection used is stored back for the next one.

    A namespace holds the whole canonical validation ledger, about 5 GB on
    2026-10-11. A stale one is taken out of CACHE and dropped BEFORE its
    replacement is built: kept beside the new one, it set one publication's
    18.8 GB peak (https://github.com/rrnewton/dev-hermit/issues/554).

    When main has moved, the replacement is advanced from the earlier build
    instead (CommittedNamespaceSeed.advanced_to), which parses and checks
    only what the ledger gained and is otherwise built in full. Main moves
    every few minutes (ledger appends, this publication's own included), so
    the recheck rebuilt the namespace in nearly every publication: 321 s and
    a 5.7 GB peak at dev-hermit main 5bd40dc6 on 2026-10-12, under the
    publication lock. Advanced over the three ledger appends before it, it
    took 10.3 s, and the process RSS rose from 1.9 GB (the seed held) to a
    2.7 GB peak (measured on a read-only clone). The seed is the earlier
    build without its ledger bytes,
    so the stale namespace is still dropped first. CACHE["namespace_seed"]
    is the seed prepare_portable_proof() keeps in place of its namespace.
    """
    import validate_rows
    _network(["git", "fetch", "--quiet", "origin", "main"], cwd=source, wrapper=net_wrapper)
    commit = _git(source, "rev-parse", "refs/remotes/origin/main^{commit}")
    reuse = None if cache is None else cache.pop("namespace", None)
    seed = None if cache is None else cache.pop("namespace_seed", None)
    if isinstance(reuse, validate_rows.CommittedValidationNamespace) and reuse.commit == commit:
        namespace = reuse
    else:
        if isinstance(reuse, validate_rows.CommittedValidationNamespace):
            seed = _namespace_seed(reuse) or seed
        del reuse
        if (isinstance(seed, validate_rows.CommittedNamespaceSeed)
                and seed.root == Path(source).resolve() and seed.state_root == Path(parent).resolve()):
            latest = validate_rows.NAMESPACE_ADVANCES[-1] if validate_rows.NAMESPACE_ADVANCES else None
            namespace = seed.advanced_to(commit)
            if validate_rows.NAMESPACE_ADVANCES and validate_rows.NAMESPACE_ADVANCES[-1] is not latest:
                _report_namespace_advance(validate_rows.NAMESPACE_ADVANCES[-1])
        else:
            namespace = validate_rows.capture_committed_validation_namespace(source, commit, state_root=parent)
        del seed
    if cache is not None:
        cache["namespace"] = namespace
    selected = namespace.select(**{key: row.get(key) for key in
                                  ("repo", "run_id", "record_id", "commit", "cwd", "started_at")})
    if _row_bytes(selected["row"]) != _row_bytes(row):
        raise MirrorRefused("published canonical terminal differs from the finalized producer")
    return namespace, selected


def prepare_portable_proof(parent: Path, encoded_row: bytes, *, net_wrapper: str,
                           attempts: int) -> dict | None:
    """Publish owning parent authority before acquiring the cell-ledger lock.

    Legacy rows retain their existing path. A modern terminal must be in the
    complete published parent namespace, not merely in a supplied event subset.
    No writer clone is created here; unavailable preparation preserves raw rows.
    """
    row = _private_row(encoded_row)
    derived = row.get("schema_version") == 5 and "raw_result_input_census_v1" in row
    if row.get("schema_version") != 10 and not derived:
        return None
    if derived:
        finalized_proof.derived_row_binding(row)
    import publish_backlog
    import validate_rows
    tool_parent = _parent_tool_commit()
    source = series.shared_parent_root(parent) / publish_backlog.CLONE_RELATIVE
    if not (source / ".git").exists():
        raise MirrorRefused("existing dedicated parent publisher is unavailable; terminal remains unqualified")
    if _git(source, "remote", "get-url", "origin") not in {
        finalized_proof.PARENT_REPOSITORY, "git@github.com:rrnewton/dev-hermit.git",
    }:
        raise MirrorRefused("parent publisher has another repository identity")
    # This supported publisher owns its serialized parent-main lock and its
    # ordinary durability-proven spool retention. It runs before the cell lock.
    publisher.publish(
        source, validate_rows.live_spool_dir(parent), source_root=parent,
        store="ledger", attempts=attempts, network_wrapper=net_wrapper,
        serialized_writer=TOOL_ROOT / "scripts/parent-main-write",
        attribution=publish_backlog.publishing_attribution(),
    )
    namespace, selected = _select_published_terminal(source, parent, row, net_wrapper=net_wrapper)
    with namespace:
        namespace.verify_origin(selected["parent"], selected["terminal"])
    # Not kept for the selection before the proof is staged. The namespace is
    # the whole canonical validation ledger, about 5 GB on 2026-10-11, and
    # keeping it held that through the series append, the snapshot and the
    # scorecard render. Reusing it saved its 47.7 s rebuild only while dev-hermit
    # main did not move between the two selections. Main moved 150 times in the
    # 24 h before 2026-10-11T03:30Z (median gap 471 s), and the two selections
    # are 12 to 15 minutes apart, so it is rebuilt in most publications anyway
    # (https://github.com/rrnewton/dev-hermit/issues/554). Its seed, which
    # holds none of the ledger's bytes, is kept instead: the recheck advances
    # it to the main it fetches rather than rebuilding the namespace. The seed
    # is mostly the rows' compressed bytes, 0.33 GB at main 5bd40dc6.
    seed = _namespace_seed(namespace)
    del namespace
    artifacts, objects = ((None, None) if derived else finalized_proof.capture_artifacts(parent, row))
    # Neither proof depends on the scorecard, so either may publish without it.
    # A derived census is computed from the finalized row and raw results. A
    # schema-10 carrier transports the run's own plan/cell/test bytes, which
    # capture_artifacts binds to the row's digests; the native evidence check
    # verified them when the row was written (validate_rows.py, the
    # qualifying_receipt.validation_evidence call) and the website verifies
    # them again from the carrier when it reads the history.
    prepared = {"source": source, "selection": selected, "artifacts": artifacts,
                "objects": objects, "parent_tool_commit": tool_parent, "derived": derived}
    if seed is not None:
        prepared["namespace_seed"] = seed
    return prepared


def _cargo_target(state_root: Path | None) -> str | None:
    explicit = os.environ.get("CARGO_TARGET_DIR")
    if explicit is None:
        return str(state_root.resolve() / "target") if state_root is not None else None
    target = Path(explicit)
    if not explicit or not target.is_absolute():
        raise MirrorRefused("explicit CARGO_TARGET_DIR must be an existing absolute directory")
    try:
        target = target.resolve(strict=True)
    except (OSError, RuntimeError) as error:
        raise MirrorRefused(f"explicit CARGO_TARGET_DIR is unavailable: {error}") from error
    if not target.is_dir():
        raise MirrorRefused("explicit CARGO_TARGET_DIR must be an existing absolute directory")
    return str(target)


def emit_derived_portable_proof(prepared: dict, parent: Path, results: Path,
                                tool_checkout: Path | None, *, net_wrapper: str) -> str:
    """Obtain new typed public operands from the complete private raw census.

    A previously published scorecard is not authority for these operands. The
    native writer verifies all original raw bytes again before this capture.
    """
    tool_checkout = (tool_checkout or TOOL_ROOT / "hermit").resolve()
    tool_commit = _committed_tool_source(tool_checkout)
    if _committed_tool_source(TOOL_ROOT / "hermit") != tool_commit:
        raise MirrorRefused("native derived-census provider differs from --tool-checkout; use the parent's committed Hermit provider")
    if not _parent_tool_source_unchanged(prepared["parent_tool_commit"]):
        raise MirrorRefused("derived-census producer changed before emission; retry with committed tools")
    compiler = rust_toolchain.selected_toolchain(tool_checkout)
    row = prepared["selection"]["row"]
    with contextlib.ExitStack() as stack:
        stack.enter_context(rust_tool_cache.session())
        directory = Path(stack.enter_context(tempfile.TemporaryDirectory(prefix="ledger-derived-census-")))
        row_path, output = directory / "row.json", directory / "derived.json"
        encoded_row = _row_bytes(row)
        row_path.write_bytes(encoded_row)
        census = ["compatibility-website-derived-census", "--row-json", str(row_path),
                  "--results-root", str(results.resolve()), "--output-json", str(output)]
        environment = {"RUSTUP_TOOLCHAIN": compiler, "RUSTUP_AUTO_INSTALL": "0"}
        started = time.monotonic()
        try:
            built = rust_tool_cache.prepare(rust_tool_cache.CI_HUB, parent, TOOL_ROOT, compiler,
                                            net_wrapper=net_wrapper)
        except Exception as error:  # any cache failure takes the direct path
            print(f"ledger publication: ci-hub tool cache unavailable ({error}); "
                  "building on the direct path", file=sys.stderr)
            _network(["rust-script", "--force", "--toolchain", compiler,
                      str(TOOL_ROOT / "ci-hub/ci-hub.rs"), *census],
                     cwd=TOOL_ROOT, wrapper=net_wrapper,
                     environment={**environment, "CARGO_TARGET_DIR": _cargo_target(parent)})
        else:
            # The census reads local files only; the stored program needs no network.
            _run([str(built.programs["ci-hub"]), *census], cwd=TOOL_ROOT,
                 environment={**environment, **built.run_environment("ci-hub")})
        print(f"ledger publication: derived census took {time.monotonic() - started:.1f} s",
              file=sys.stderr)
        if row_path.read_bytes() != encoded_row:
            raise MirrorRefused("original finalized row changed during derived emission")
        encoded = finalized_proof._read_local(directory, output.name)
        artifacts, objects = finalized_proof.capture_derived_cells(row, encoded)
    if (not _parent_tool_source_unchanged(prepared["parent_tool_commit"])
            or _committed_tool_source(TOOL_ROOT / "hermit") != tool_commit
            or _committed_tool_source(tool_checkout) != tool_commit):
        raise MirrorRefused("derived-census producer changed during emission; retain inputs and retry")
    prepared.update(artifacts=artifacts, objects=objects)
    return tool_commit


def schema10_proof_tool(target: str) -> str:
    """The Hermit commit a schema-10 carrier names when no scorecard is rendered.

    A schema-10 carrier's operands are the measured run's own plan, cell and
    test artifacts; no tool derives them. They were produced by the validate
    of TARGET, so the carrier names TARGET, which is also what an ordinary
    publication names when the measured checkout renders its own scorecard.
    """
    return target


def published_scorecard_binding(writer: Path, snapshot: Mapping[str, Any], target: str) -> dict:
    """Require the existing public projection before a later proof attachment."""
    _verify_published_snapshot(snapshot)
    encoded = finalized_proof._read_local(writer, "latest.json")
    value = finalized_proof._json(encoded)
    if (not isinstance(value, dict) or value.get("schema") != METADATA_SCHEMA
            or value.get("source_repository") != REMOTE_URL
            or value.get("scorecard_hermit_commit") != target):
        raise MirrorRefused("published scorecard does not bind this measured source and raw series; run ordinary finalized scorecard publication first")
    source = finalized_proof._hash(value.get("source_commit"), "scorecard source commit", git=True)
    if value.get("series_commit") != source:
        raise MirrorRefused("published scorecard source and series commit differ")
    if not _published(writer, source, snapshot["source"]["commit"]):
        raise MirrorRefused(f"published scorecard source {source} is not in the current raw series history")
    # Raw publication can advance independently of the frozen scorecard. Bind
    # that projection to its own immutable input, then require every original
    # event to survive unchanged in the current proof attachment's input.
    with _published_snapshot_scope(writer, source) as projected:
        if (value.get("source_tree") != projected["source"]["tree"]
                or value.get("rows_sha256") != projected["rows_sha256"]
                or value.get("rows") != projected["rows_read"]):
            raise MirrorRefused("published scorecard metadata differs from its immutable raw source")
        current_rows = _rows_by_event_id(snapshot["rows"])
        if any(current_rows.get(event_id) != text for event_id, text in _rows_by_event_id(projected["rows"]).items()):
            raise MirrorRefused("published scorecard source events were changed or removed from current raw series")
        hashes = value.get("scorecard")
        if not isinstance(hashes, dict) or not all(path.as_posix() in hashes for path in MIRRORED_SCORECARD_PATHS):
            raise MirrorRefused("published scorecard omitted its bound files; run ordinary finalized publication first")
        for path, digest in hashes.items():
            finalized_proof._relative(path)
            finalized_proof._hash(digest, "scorecard file digest")
            if not path.startswith("scorecard/") or hashlib.sha256(_scorecard_bytes(writer, path)).hexdigest() != digest:
                raise MirrorRefused("published scorecard bytes differ from metadata; preserve the refusal and regenerate normally")
        _verify_published_snapshot(projected)
        _verify_published_snapshot(snapshot)
        return {"commit": snapshot["source"]["commit"],
                "latest_sha256": hashlib.sha256(encoded).hexdigest(), "files": hashes}


def _scorecard_bytes(writer: Path, path: str) -> bytes:
    """The bytes of scorecard file PATH in the writer; the history is reassembled.

    latest.json records the digest of the renderer's scorecard/cells.json, the
    document the ledger stores in parts (scorecard_cells).
    """
    if path == scorecard_cells.LOGICAL_PATH:
        return _stored_history(writer)
    return finalized_proof._read_local(writer, path)


def finish_portable_proof(prepared: dict[str, Any], parent: Path, writer: Path,
                          snapshot: Mapping[str, Any], tool_commit: str, *,
                          net_wrapper: str) -> dict[str, str]:
    """Stage the exact carrier for the selected terminal in the writer.

    The canonical terminal is selected again, and an identical carrier already
    on the fetched ledger keeps its original provenance (an idempotent retry).
    """
    _verify_published_snapshot(snapshot)
    selection = prepared["selection"]
    namespace, current = _select_published_terminal(
        prepared["source"], parent, selection["row"], net_wrapper=net_wrapper,
        cache=prepared)
    with namespace:
        if current["terminal"] != selection["terminal"]:
            raise MirrorRefused("canonical terminal changed before proof publication")
        # Read-only requery under the cell lock takes no parent writer lock.
        # A correction without its carrier remains unavailable, never stale credit.
        index = finalized_proof.read_carriers(writer, snapshot["source"]["commit"])
        existing = finalized_proof.carrier_for_row(index, current["row"], current["terminal"])
        if existing is not None:
            namespace.verify_origin(existing["parent"], existing["terminal"])
        if not _parent_tool_source_unchanged(prepared["parent_tool_commit"]):
            raise MirrorRefused("portable proof producer source changed during publication")
        manifest = finalized_proof.make_manifest(
            current, prepared["artifacts"], parent_commit=prepared["parent_tool_commit"],
            hermit_tool_commit=tool_commit, snapshot=snapshot)
        _verify_published_snapshot(snapshot)
        return finalized_proof.stage_carrier(writer, manifest, prepared["objects"], existing=existing)


def _scorecard_execution(
    source_checkout: Path, target: str, tool_checkout: Path | None,
    state_root: Path | None,
) -> tuple[Path, str, str, dict[str, str]]:
    if _git(source_checkout, "rev-parse", "HEAD^{commit}") != target:
        raise MirrorRefused("scorecard source HEAD no longer matches the validation target")
    # The measured checkout renders its own SCORECARD.md with its own
    # scorecard.rs, so a renderer change and the prose it produces travel in
    # one commit (https://github.com/rrnewton/dev-hermit/issues/392). The
    # parent's hermit gitlink is not consulted. build_scorecard confines the
    # child's writes to the exact generated paths.
    tool_checkout = (tool_checkout or source_checkout).resolve()
    tool_commit = _committed_tool_source(tool_checkout)
    tool = tool_checkout / "ci/compat-envelope/scorecard.rs"
    if not tool.is_file():
        raise MirrorRefused(f"missing scorecard tool: {tool}")
    try:
        compiler = rust_toolchain.selected_toolchain(tool_checkout)
    except rust_toolchain.ToolchainRefused as error:
        raise MirrorRefused(f"cannot select scorecard tool compiler: {error}") from error
    environment = {
        "RUSTUP_TOOLCHAIN": compiler, "RUSTUP_AUTO_INSTALL": "0",
    }
    cargo_target = _cargo_target(state_root)
    if cargo_target is not None:
        # The immutable tool owns code; the existing state target owns mutable
        # Cargo output, shared by recurring publications rather than each run.
        environment["CARGO_TARGET_DIR"] = cargo_target
    return tool, tool_commit, compiler, environment


def prepare_scorecard(
    source_checkout: Path, target: str, *, tool_checkout: Path | None,
    state_root: Path, net_wrapper: str,
) -> tuple[str, str]:
    """Prepare the real compiler before taking the shared publication lock.

    The scorecard's help returns before repository/history access. It receives
    neither the ledger root nor its lock descriptor. The actual transaction
    still uses --force and rechecks these source identities under the lock.
    """
    tool, commit, compiler, environment = _scorecard_execution(
        source_checkout, target, tool_checkout, state_root)
    started = time.monotonic()
    built = None
    if rust_tool_cache.active():
        # build_scorecard runs these stored programs while this session holds them.
        try:
            built = rust_tool_cache.prepare(rust_tool_cache.SCORECARD, state_root, tool.parents[2],
                                            compiler, net_wrapper=net_wrapper)
        except Exception as error:  # any cache failure takes the direct path
            print(f"ledger publication: scorecard tool cache unavailable ({error}); "
                  "building on the direct path", file=sys.stderr)
    if built is None:
        # derive() invokes this distinct Cargo binary, not the rust-script body.
        # Use its exact package/manifest/target prefix with its read-only help path.
        _network(
            ["cargo", "run", "--quiet", "-p", "hermit-manifest-plan",
             "--manifest-path", str(tool.parents[2] / "Cargo.toml"),
             "--target-dir", environment["CARGO_TARGET_DIR"], "--", "--help"],
            cwd=source_checkout, wrapper=net_wrapper, environment=environment,
        )
        _network(
            ["rust-script", "--force", "--toolchain", compiler, str(tool), "--help"],
            cwd=source_checkout, wrapper=net_wrapper, environment=environment,
        )
    print(f"ledger publication: scorecard tool preparation took {time.monotonic() - started:.1f} s",
          file=sys.stderr)
    prepared = _scorecard_execution(source_checkout, target, tool_checkout, state_root)
    if prepared[1:3] != (commit, compiler):
        raise MirrorRefused("scorecard tool changed during compiler preparation")
    return commit, compiler


def _copy_environment() -> dict[str, str]:
    return _environment(COPY_GIT_ENV, RENDERER_WITHHELD_ENV)


def _copy_git(copy: Path, *args: str) -> str:
    return _run(["git", "-C", str(copy), *args], environment=COPY_GIT_ENV,
                withhold=RENDERER_WITHHELD_ENV).stdout.strip()


def _copy_changes(copy: Path) -> dict[str, str]:
    return _worktree_changes(copy, environment=COPY_GIT_ENV, withhold=RENDERER_WITHHELD_ENV)


def _stored_history(root: Path) -> bytes:
    """The scorecard history stored in the files under ROOT, reassembled."""
    try:
        document = scorecard_cells.read_tree(root)
    except scorecard_cells.CellsRefused as error:
        raise MirrorRefused(f"cannot read the scorecard history in {root}: {error}") from error
    if document is None:
        raise MirrorRefused(f"{root} holds no {scorecard_cells.LOGICAL_PATH}")
    return document


def _blob_name(copy: Path, data: bytes) -> str:
    algorithm = _copy_git(copy, "rev-parse", "--show-object-format")
    return hashlib.new(algorithm, b"blob %d\0" % len(data) + data).hexdigest()


def _require_render_view(copy: Path, base: str, view: str | None, *, rendered: bool) -> None:
    """Require the copy to show the renderer exactly the history BASE stores.

    VIEW must be the copy's only replacement, for BASE, and `git show
    HEAD:scorecard/cells.json` there (the renderer's retirement check) must
    read the stored history. Before the render the checked-out file must be
    those bytes too; after it, the file is the renderer's output.
    """
    environment = _copy_environment()
    try:
        found = scorecard_cells.replacements(copy, environment)
        stored = scorecard_cells.read_commit(copy, base, environment=environment)
        checked_out = None if rendered else scorecard_cells.read_tree(copy)
    except scorecard_cells.CellsRefused as error:
        raise MirrorRefused(f"cannot verify the scorecard tool's view of {base}: {error}") from error
    if found != ({} if view is None else {base: view}):
        raise MirrorRefused(f"the ledger copy's replacement objects changed: {found}; nothing was copied "
                            "into the ledger writer")
    if not rendered and checked_out != stored:
        raise MirrorRefused(f"the ledger copy's checked-out scorecard history differs from {base}")
    if stored is not None and _copy_git(
            copy, "rev-parse", f"HEAD:{scorecard_cells.LOGICAL_PATH}") != _blob_name(copy, stored):
        raise MirrorRefused(f"the ledger copy does not show the scorecard history stored at {base}")


@contextlib.contextmanager
def _render_copy(writer: Path, base: str) -> Iterator[tuple[Path, str | None]]:
    """Yield a throwaway ledger checkout at BASE for the measured renderer, and its view.

    The copy reads the writer's objects through alternates and writes its own,
    so a commit, index, ref, config or hook change the renderer makes through
    DEV_HERMIT_TEST_LEDGER_ROOT stays in the copy and is discarded. Its origin
    URL is the ledger's (scorecard.rs checks it), its push URL is a path that
    does not exist, and its publication lock is a link to the real one, which
    the caller holds. Only the lock holder creates these copies, so one left
    behind by a killed publisher is removed by the next. Each copy has a fresh
    name, so a stale one that cannot be removed is inert and never reused.

    When BASE stores the scorecard history in parts, the copy also holds a
    replacement for BASE whose tree has the reassembled scorecard/cells.json
    in their place (scorecard_cells.install_render_view), so the renderer reads
    and rewrites the single file it has always used. That replacement is the
    yielded view; it is None when BASE stores the single file or no history.
    It exists only in the copy's refs, which nothing fetches or pushes.
    """
    common = Path(_git(writer, "rev-parse", "--path-format=absolute", "--git-common-dir"))
    for stale in writer.parent.glob(RENDER_COPY_PREFIX + "*"):
        if stale.is_dir() and not stale.is_symlink():
            shutil.rmtree(stale, ignore_errors=True)
    with tempfile.TemporaryDirectory(prefix=RENDER_COPY_PREFIX, dir=writer.parent,
                                     ignore_cleanup_errors=True) as temporary:
        copy = Path(temporary) / "hermit_test_ledger"
        _run(["git", "init", "--quiet", "--template=", str(copy)])
        (copy / ".git/objects/info").mkdir(parents=True, exist_ok=True)
        (copy / ".git/objects/info/alternates").write_text(str(common / "objects") + "\n")
        _copy_git(copy, "config", "remote.origin.url", REMOTE_URL)
        _copy_git(copy, "config", "remote.origin.pushurl", str(copy / ".git/push-disabled"))
        try:
            view = scorecard_cells.install_render_view(copy, base, _copy_environment())
        except scorecard_cells.CellsRefused as error:
            raise MirrorRefused(f"cannot show the scorecard history stored at {base} to the "
                                f"scorecard tool: {error}") from error
        _copy_git(copy, "checkout", "--quiet", "--detach", base)
        (copy / ".git" / PUBLICATION_LOCK).symlink_to(common / PUBLICATION_LOCK)
        if _copy_git(copy, "rev-parse", "HEAD^{commit}") != base or _copy_changes(copy):
            raise MirrorRefused(f"cannot prepare a clean ledger copy at {base} for the scorecard tool")
        _require_render_view(copy, base, view, rendered=False)
        yield copy, view


def _verify_file_inputs(copy: Path, file_inputs: Mapping[str, Any]) -> None:
    """Hash materialized source files through pinned ordinary directory fds."""
    directory_flags = os.O_RDONLY | os.O_DIRECTORY | os.O_CLOEXEC | os.O_NOFOLLOW
    for relative, descriptor in file_inputs.items():
        parts = relative.split("/")
        if (not parts or parts[0] not in {"series", "quarantine"}
                or any(piece in {"", ".", ".."} or "\0" in piece for piece in parts)
                or descriptor.path != relative or descriptor.mode != "100644"
                or type(descriptor.bytes) is not int
                or descriptor.bytes < 0 or not isinstance(descriptor.sha256, str)
                or re.fullmatch(r"[0-9a-f]{64}", descriptor.sha256) is None):
            raise MirrorRefused(f"invalid materialized original-file descriptor: {relative}")
        handles: list[int] = []
        try:
            handles.append(os.open(copy, directory_flags))
            for piece in parts[:-1]:
                handles.append(os.open(piece, directory_flags, dir_fd=handles[-1]))
            handles.append(os.open(parts[-1], os.O_RDONLY | os.O_CLOEXEC | os.O_NOFOLLOW
                                   | os.O_NONBLOCK, dir_fd=handles[-1]))
            leaf = handles[-1]
            before = os.fstat(leaf)
            if (not stat.S_ISREG(before.st_mode) or before.st_mode & 0o111
                    or before.st_size != descriptor.bytes):
                raise MirrorRefused(f"scorecard tool changed an archived input: {relative}")
            digest, size = hashlib.sha256(), 0
            while chunk := os.read(leaf, min(65536, descriptor.bytes - size + 1)):
                size += len(chunk)
                if size > descriptor.bytes:
                    raise MirrorRefused(f"scorecard tool grew an archived input: {relative}")
                digest.update(chunk)
            after = os.fstat(leaf)
            identity = lambda value: (value.st_dev, value.st_ino, value.st_mode, value.st_size,
                                      value.st_mtime_ns, value.st_ctime_ns)
            if (size != descriptor.bytes or digest.hexdigest() != descriptor.sha256
                    or identity(before) != identity(after)):
                raise MirrorRefused(f"scorecard tool changed an archived input: {relative}")
        except OSError as error:
            raise MirrorRefused(f"cannot verify archived scorecard input {relative}: {error}") from error
        finally:
            for handle in reversed(handles):
                os.close(handle)


def _copy_back(copy: Path, writer: Path, base: str, paths: Sequence[Path], view: str | None,
               optional: Sequence[Path] = (), *,
               inputs: Mapping[str, bytes | None] | None = None,
               file_inputs: Mapping[str, Any] | None = None,
               verify_inputs: Callable[[], None] | None = None,
               discarded: Sequence[Path] = ()) -> list[Path]:
    """Move exactly the renderer's generated files from COPY into the writer.

    The generated scorecard/cells.json is stored in parts (scorecard_cells).
    When the renderer generated it, every generated file and every part is
    checked against scorecard_cells.FILE_CEILING_BYTES before anything is
    written. Nothing else in the commit is checked against it: a legacy
    export (no history) keeps its archive whole, and _publish_scorecard later
    writes latest.json and, through finish_portable_proof, any finalized-proof
    carrier files.

    Every one of ``paths`` must exist. One of ``optional`` is copied only when
    the renderer changed it, and returned with ``paths`` as what was bound.
    """
    def verify_originals() -> None:
        try:
            if verify_inputs is not None:
                verify_inputs()
            _verify_file_inputs(copy, file_inputs or {})
        except (ValueError, OSError, subprocess.SubprocessError) as error:
            raise MirrorRefused(f"archived scorecard inputs are unavailable: {error}") from error

    verify_originals()
    if _copy_git(copy, "rev-parse", "HEAD^{commit}") != base:
        raise MirrorRefused("scorecard tool moved the HEAD of its ledger copy; nothing was copied "
                            "into the ledger writer")
    _require_render_view(copy, base, view, rendered=True)
    changed = set(_copy_changes(copy))
    # A retention render sees a proposed window only in its disposable copy.
    # These inputs are never copied back, and the renderer may not alter them.
    for path, expected_bytes in (inputs or {}).items():
        target = copy / path
        for parent in target.parents:
            if parent == copy:
                break
            if parent.is_symlink():
                raise MirrorRefused(f"scorecard tool replaced a retention input directory: {path}")
        if expected_bytes is None:
            if os.path.lexists(target):
                raise MirrorRefused(f"scorecard tool restored a retired input: {path}")
        elif (target.is_symlink() or not target.is_file()
              or target.read_bytes() != expected_bytes):
            raise MirrorRefused(f"scorecard tool changed a retention input: {path}")
    paths = [*paths, *(path for path in optional if path.as_posix() in changed)]
    expected = {path.as_posix() for path in paths}
    unexpected = sorted(changed - expected - set(inputs or {}) - set(file_inputs or {})
                        - {path.as_posix() for path in discarded})
    if unexpected:
        raise MirrorRefused(
            "scorecard tool changed ledger paths outside its generated output; nothing was copied "
            "into the ledger writer: " + ", ".join(unexpected))
    # The copy is the only ledger the renderer was given; a change to the
    # writer itself means something reached past it.
    if _git(writer, "rev-parse", "HEAD^{commit}") != base:
        raise MirrorRefused("the ledger writer HEAD moved while the scorecard tool ran; writer retained for recovery")
    touched = _worktree_changes(writer)
    if touched:
        # The publication found the writer clean under the lock before the
        # tool ran, so a scorecard file changed since is attributed to the
        # render (an operator writing one meanwhile is set aside with it, on
        # a recovery ref). Anything else stays in the writer, unrecorded.
        window: dict[str, bytes | None] = {}
        for path, status in touched.items():
            if _render_leftover(path, status) and _scorecard_path(path):
                target = writer / path
                if not os.path.lexists(target):
                    window[path] = None
                elif target.is_file() and not target.is_symlink():
                    window[path] = target.read_bytes()
        if window:
            _record_render_provenance(writer, list(window.items()))
        raise MirrorRefused("the ledger writer changed while the scorecard tool ran; kept for recovery "
                            "(render outputs under refs/heads/recovery/render-*, anything else in "
                            "the writer): " + ", ".join(sorted(touched)))
    generated = {}
    for path in paths:
        source = copy / path
        if source.is_symlink() or not source.is_file():
            raise MirrorRefused(f"scorecard tool output {path} is not a regular file")
        generated[path.as_posix()] = source.read_bytes()
    document = generated.pop(scorecard_cells.LOGICAL_PATH, None)
    stored = {}
    if document is not None:
        try:
            stored = scorecard_cells.split(document)
        except scorecard_cells.CellsRefused as error:
            raise MirrorRefused(f"cannot store the generated scorecard history: {error}; nothing was "
                                "copied into the ledger writer") from error
    if document is not None or discarded:
        # A compaction discards the newly rendered history, but every copied
        # output still has the ordinary render ceiling. A legacy-only export
        # keeps its whole archive, as before.
        oversized = sorted(path for path, encoded in {**generated, **stored}.items()
                           if len(encoded) > scorecard_cells.FILE_CEILING_BYTES)
        if oversized:
            raise MirrorRefused(f"scorecard output {', '.join(oversized)} exceeds "
                                f"{scorecard_cells.FILE_CEILING_BYTES} bytes; nothing was copied into "
                                "the ledger writer")
    for path in generated:
        if (writer / path).is_symlink():
            raise MirrorRefused(f"ledger writer path {path} is a symbolic link")
    # Last, just before the writes: the status check above lists no ignored
    # file and trusts filters and index bits, and the regeneration below
    # deletes a stale history file. A write after this check is moved aside
    # by _replace_writer_files, not lost.
    unstored = _unstored_bytes(writer)
    if unstored:
        raise MirrorRefused("the ledger writer gained bytes its HEAD does not store while the "
                            "scorecard tool ran; nothing was copied into the ledger writer, "
                            "which is retained for recovery: " + "; ".join(unstored[:20]))
    # The raw-row publication's own Git commands may have set assume-unchanged
    # bits, under which `git add` would not stage the files written below.
    verify_originals()
    _clear_index_bits(writer)
    if document is not None:
        try:
            install = lambda writes, removals: _replace_writer_files(writer, writes, removals)  # noqa: E731
            if EXTERNAL_HISTORY:
                # The parts go into a parentless commit (objects only); main
                # gets the manifest naming it, and both are pushed together.
                parts = scorecard_cells.store_parts_commit(writer, stored, environment=_environment())
                scorecard_cells.replace_tree_external(
                    writer, scorecard_cells.external_manifest(stored, parts), install=install)
            else:
                scorecard_cells.replace_tree(writer, stored, install=install)
        except scorecard_cells.CellsRefused as error:
            raise MirrorRefused(f"cannot store the scorecard history in the ledger writer: {error}") from error
    _replace_writer_files(writer, generated)
    if document is not None and _stored_history(writer) != document:
        raise MirrorRefused("the scorecard history stored in the ledger writer differs from the generated "
                            "document; kept for recovery under refs/heads/recovery/render-*")
    return paths


_SKIPPED_ROW_INDEX = re.compile(r"(0|[1-9][0-9]*)\]:")


def _skipped_row_label(row: Any) -> str | None:
    """The label scorecard.rs prints for ROW after its "<snapshot>:rows[<i>]:".

    SeriesRow::label (ci/manifest-plan/src/stress_series.rs) is "{event_id}:
    {cell} from {producer} run {run_id} repetition {run_index} @ {tree}".
    None when ROW lacks one of those fields with the type the renderer
    reads, or holds a newline anywhere: then its record need not be one line.
    """
    if not isinstance(row, Mapping) or _mentions(row, "\n"):
        return None
    values = row.get("series")
    if not isinstance(values, Mapping):
        return None
    texts = (row.get("event_id"), values.get("cell"), row.get("producer"), row.get("run_id"),
             values.get("tree"))
    run_index = values.get("run_index")
    if not all(isinstance(text, str) for text in texts) or type(run_index) is not int or run_index < 0:
        return None
    event_id, cell, producer, run_id, tree = texts
    return f"{event_id}: {cell} from {producer} run {run_id} repetition {run_index} @ {tree}"


def summarize_skipped_rows(output: str, *, prefix: str, source: str, rows: Sequence[Any]) -> str:
    """The renderer's standard output with its skipped snapshot rows grouped by reason.

    project-and-observe-results names every snapshot row it skips, one record
    each, "<PREFIX><index>]:<label>: <reason>" with PREFIX "  skipped
    <snapshot>:rows[", so that a fold that drops rows cannot pass for a thin
    one. On the ops tick's ledger that was 9,284 of a log's 11,619 lines,
    across four reasons. Each reason keeps its exact row count and one
    complete example line, and the total stays, so a high or new skip count is
    as visible as before. SOURCE names the snapshot, its digest, its ledger
    commit and the tool, from which the individual rows are reproducible.

    A line is folded only when it is that whole record: its index names a row
    of ROWS (the snapshot's rows), the text after it is exactly that row's
    label (_skipped_row_label) followed by ": " and a nonempty reason, and the
    row holds no newline anywhere, so neither its label nor a reason built
    from its fields continues on the next line (the renderer's own reason
    templates contain none; values are quoted with {:?}). Every other line,
    including any part of a record that spans lines, a skipped cell and a
    skip line of another snapshot, is relayed unchanged and in order, and
    the summary takes the place of the first folded line. Lines end at "\\n"
    only, as the renderer's println! writes them.
    """
    reasons: dict[str, list[Any]] = {}
    kept: list[str | None] = []
    pieces = output.split("\n")
    lines = [piece + "\n" for piece in pieces[:-1]] + ([pieces[-1]] if pieces[-1] else [])
    for line in lines:
        text = line[:-1] if line.endswith("\n") else line
        index = _SKIPPED_ROW_INDEX.match(text, len(prefix)) if text.startswith(prefix) else None
        label = None
        if index is not None and int(index.group(1)) < len(rows):
            label = _skipped_row_label(rows[int(index.group(1))])
        record = text[index.end():] if index is not None else ""
        if label is None or not record.startswith(label + ": ") or len(record) == len(label) + 2:
            kept.append(line)
            continue
        if not reasons:
            kept.append(None)
        entry = reasons.setdefault(record[len(label) + 2:], [0, text])
        entry[0] += 1
    if not reasons:
        return output
    total = sum(count for count, _ in reasons.values())
    summary = [f"  skipped {total} row(s) of {source}, for {len(reasons)} reason(s); each reason "
               f"below has its exact count and its first row:\n"]
    for reason, (count, example) in sorted(reasons.items(), key=lambda item: (-item[1][0], item[0])):
        summary.append(f"    {count} row(s): {reason}\n      first:{example[1:]}\n")
    return "".join("".join(summary) if line is None else line for line in kept)


def build_scorecard(
    source_checkout: Path, target: str, snapshot: Mapping[str, Any], writer: Path,
    *, lock_fd: int, net_wrapper: str, results: Path | None = None,
    scorecard_command: str | None = None, scorecard_args: Sequence[str] = (),
    tool_checkout: Path | None = None, finalized_run_id: str | None = None,
    state_root: Path | None = None, prepared_finalized_row: bytes | None = None,
    prepared_tool: tuple[str, str] | None = None, lifecycle_deadline_ns: int | None = None,
    retention_changes: Mapping[str, bytes | None] | None = None,
) -> tuple[dict[str, str], str]:
    _verify_published_snapshot(snapshot)
    tool_checkout = (tool_checkout or source_checkout).resolve()
    tool, tool_commit, compiler, environment = _scorecard_execution(
        source_checkout, target, tool_checkout, state_root)
    if prepared_tool is not None and prepared_tool != (tool_commit, compiler):
        raise MirrorRefused("scorecard tool changed after compiler preparation")
    base = snapshot["source"]["commit"]
    print(
        f"ledger publication: scorecard tool {tool_commit} uses Rust {compiler}; "
        f"measured source {target}", file=sys.stderr,
    )
    commands = []
    skipped_rows = None
    finalized_row = None
    with contextlib.ExitStack() as temporary:
        ledger, view = temporary.enter_context(_render_copy(writer, base))
        try:
            if scorecard_command == "export-legacy" and retention_changes is not None:
                raise MirrorRefused("a retention transaction requires a parity scorecard render")
            window = (None if scorecard_command == "export-legacy"
                      else retention_render.prepare(ledger, retention_changes))
            if window is not None:
                temporary.callback(window.close)
        except (ledger_retention.RetentionError, ValueError, OSError) as error:
            raise MirrorRefused(f"cannot prepare the retained parity window: {error}") from error
        # prepare_scorecard already fetched every dependency with the network;
        # this run needs none, and gets neither network routes nor credentials.
        environment.update({
            series.LEDGER_ROOT_ENV: str(ledger), LOCK_FD_ENV: str(lock_fd),
            "CARGO_NET_OFFLINE": "true", "GIT_TERMINAL_PROMPT": "0",
            "GIT_CONFIG_GLOBAL": os.devnull, "GIT_CONFIG_NOSYSTEM": "1",
        })
        if finalized_run_id is not None:
            if results is None or state_root is None or scorecard_command is not None:
                raise MirrorRefused("finalized run publication requires results and the canonical state root")
            finalized_row = (prepared_finalized_row if prepared_finalized_row is not None else
                             finalized_producer_row(state_root, finalized_run_id, target, source_checkout))
            row = json.loads(finalized_row)
            directory = Path(temporary.enter_context(tempfile.TemporaryDirectory(prefix="ledger-finalized-run-")))
            row_path, snapshot_path = directory / "row.json", directory / "snapshot.json"
            row_path.write_bytes(finalized_row)
            snapshot_sha256 = _write_snapshot(snapshot, snapshot_path)
            # Only this command's skipped snapshot rows are folded: each is
            # reproducible from the snapshot, the measured source and the tool.
            skipped_rows = (f"  skipped {snapshot_path}:rows[",
                            f"snapshot {snapshot_path} (sha256 {snapshot_sha256}, "
                            f"ledger commit {base}), reproducible with scorecard tool {tool_commit} "
                            f"on measured source {target}")
            commands.append([
                "project-and-observe-results", "--snapshot", str(snapshot_path),
                "--snapshot-sha256", snapshot_sha256,
                "--results", str(results.resolve()), "--expected-head", target,
                "--results-head", target, "--refreshed-at", row["started_at"],
                "--finalized-row", str(row_path),
                "--finalized-row-sha256", hashlib.sha256(finalized_row).hexdigest(),
                "--state-root", str(state_root.resolve()),
            ])
        elif scorecard_command:
            commands.append([scorecard_command, *scorecard_args])
        else:
            if results is not None:
                commands.append(["observe-results", "--results", str(results.resolve())])
            newest = _max_emitted_at(snapshot["rows"])
            refreshed_at = newest if newest is not None else str(snapshot["source"]["commit"])
            if window is not None and window.files is not None:
                # The original Git tree now contains an archive marker rather
                # than every logical shard. Keep that real commit/tree in the
                # strict native snapshot, with the authenticated union's rows.
                # An older native tool refuses the distinct command; it must
                # never fall back to a different input population.
                directory = Path(temporary.enter_context(tempfile.TemporaryDirectory(
                    prefix="ledger-series-projection-")))
                snapshot_path = directory / "snapshot.json"
                snapshot_sha256 = _write_snapshot(snapshot, snapshot_path)
                commands.append([
                    "project-observations-snapshot", "--snapshot", str(snapshot_path),
                    "--snapshot-sha256", snapshot_sha256, "--refreshed-at", refreshed_at,
                ])
            else:
                commands.append([
                    "project-observations", "--series-root", str(ledger / "series"),
                    "--refreshed-at", refreshed_at,
                ])
        built = (rust_tool_cache.prepared(rust_tool_cache.SCORECARD, tool_checkout, compiler)
                 if prepared_tool is not None else None)
        if built is not None:
            # The programs prepare_scorecard stored for this tool commit and
            # compiler; derive() runs the stored helper instead of Cargo.
            program = [str(built.programs["scorecard"])]
            environment.update(built.run_environment("scorecard"))
            environment["HERMIT_MANIFEST_PLAN_BIN"] = str(built.programs["hermit-manifest-plan"])
            print(f"ledger publication: scorecard renders with stored programs {built.key[:16]}",
                  file=sys.stderr)
        else:
            program = ["rust-script", "--force", "--toolchain", compiler, str(tool)]
        with _scorecard_git(state_root, environment):
            for command in commands:
                _verify_published_snapshot(snapshot)
                if window is not None:
                    try:
                        window.verify()
                        _verify_file_inputs(ledger, window.file_inputs)
                    except (ValueError, OSError, subprocess.SubprocessError) as error:
                        raise MirrorRefused(f"archived render inputs are unavailable: {error}") from error
                # The render is the long step and grows with the ledger; it gets
                # the lifecycle's remainder, not the 900 s per-command default.
                result = _run(
                    [*program, *command],
                    cwd=source_checkout, environment=environment, pass_fds=(lock_fd,),
                    withhold=RENDERER_WITHHELD_ENV, timeout=scorecard_timeout(lifecycle_deadline_ns),
                )
                _verify_published_snapshot(snapshot)
                if window is not None:
                    try:
                        window.verify()
                        _verify_file_inputs(ledger, window.file_inputs)
                    except (ValueError, OSError, subprocess.SubprocessError) as error:
                        raise MirrorRefused(f"archived render inputs changed during native generation: {error}") from error
                if result.stdout:
                    stdout = result.stdout
                    if skipped_rows is not None and command[0] == "project-and-observe-results":
                        stdout = summarize_skipped_rows(stdout, prefix=skipped_rows[0], source=skipped_rows[1],
                                                        rows=snapshot["rows"])
                    print(stdout, end="", file=sys.stderr)
                if result.stderr:
                    print(result.stderr, end="", file=sys.stderr)
        if finalized_row is not None and finalized_producer_row(
            state_root, finalized_run_id, target, source_checkout,
        ) != finalized_row:
            raise MirrorRefused("canonical finalized producer changed during scorecard generation")
        if _git(source_checkout, "rev-parse", "HEAD^{commit}") != target:
            raise MirrorRefused("scorecard source HEAD changed during generation")
        if _committed_tool_source(tool_checkout) != tool_commit:
            raise MirrorRefused("scorecard tool HEAD changed during generation")
        optional: Sequence[Path] = ()
        if scorecard_command == "export-legacy":
            blob = _git(source_checkout, "rev-parse", "HEAD:ci/compat-envelope/cells.json")
            paths = (Path(f"scorecard/legacy/{blob}.json"),
                     Path(f"scorecard/legacy/{blob}.identity.json"))
        else:
            paths = MIRRORED_SCORECARD_PATHS
            optional = OPTIONAL_SCORECARD_PATHS
        if window is not None:
            try:
                window.verify()
                _verify_file_inputs(ledger, window.file_inputs)
                window.promote(ledger)
            except (ValueError, OSError) as error:
                raise MirrorRefused(f"cannot bind the retained parity window: {error}") from error
        _verify_published_snapshot(snapshot)
        discarded: Sequence[Path] = ()
        if retention_changes is not None:
            # Compaction changes the parity window, not cell history. Keep the
            # current history and its ref; no parentless parts commit or forced
            # history-ref update is needed for this transaction.
            discarded = (Path(scorecard_cells.LOGICAL_PATH),)
            paths = tuple(path for path in paths if path not in discarded)
        # The renderer is the measured tree's code. Its write authority is
        # exactly the generated files, copied last so a failure above leaves
        # the writer untouched.
        if window is None and not discarded:
            paths = _copy_back(ledger, writer, base, paths, view, optional)
        else:
            paths = _copy_back(ledger, writer, base, paths, view, optional,
                               inputs=window.inputs if window else {},
                               file_inputs=window.file_inputs if window else {},
                               verify_inputs=window.verify if window else None,
                               discarded=discarded)
    return ({path.as_posix(): hashlib.sha256(
                _stored_history(writer) if path.as_posix() == scorecard_cells.LOGICAL_PATH
                else (writer / path).read_bytes()).hexdigest()
             for path in paths}, tool_commit)


def published_scorecard_head(writer: Path) -> dict[str, Any] | None:
    """Return the hermit commit (and recorded commit time) latest.json names at HEAD."""
    if not _git(writer, "ls-tree", "HEAD", "--", "latest.json"):
        return None
    value = finalized_proof._json(_run(["git", "-C", str(writer), "show", "HEAD:latest.json"]).stdout.encode())
    if not isinstance(value, dict) or value.get("schema") != METADATA_SCHEMA:
        raise MirrorRefused("published latest.json is not scorecard metadata; the scorecard head cannot be ordered")
    commit = value.get("scorecard_hermit_commit")
    if not isinstance(commit, str) or not _COMMIT.fullmatch(commit):
        raise MirrorRefused("published latest.json names no forty-hex scorecard head")
    recorded = value.get(SCORECARD_HEAD_TIME_FIELD)
    if recorded is not None and (type(recorded) is not int or recorded <= 0):
        raise MirrorRefused(f"published latest.json has an invalid {SCORECARD_HEAD_TIME_FIELD}")
    return {"commit": commit, "time": recorded}


def _commit_time(repo: Path, commit: str) -> int | None:
    """Committer time of COMMIT in REPO, or None when REPO does not have it."""
    found = _run(["git", "-C", str(repo), "rev-parse", "--verify", "--quiet", f"{commit}^{{commit}}"],
                 check=False)
    if found.returncode == 1:
        return None
    if found.returncode != 0 or found.stdout.strip() != commit:
        raise MirrorRefused(f"cannot resolve hermit commit {commit} in {repo}")
    return int(_git(repo, "show", "-s", "--format=%ct", commit))


def _is_ancestor(repo: Path, ancestor: str, descendant: str) -> bool:
    result = _run(["git", "-C", str(repo), "merge-base", "--is-ancestor", ancestor, descendant],
                  check=False)
    if result.returncode not in {0, 1}:
        raise MirrorRefused(f"cannot order hermit commits {ancestor} and {descendant}: "
                            + (result.stderr.strip() or f"exit {result.returncode}"))
    return result.returncode == 0


def scorecard_order(writer: Path, source_checkout: Path, target: str) -> dict[str, Any]:
    """Decide whether TARGET may become the public scorecard head.

    The head only moves forward. A descendant of the published head advances
    and an ancestor is superseded. Divergent heads compare committer time: the
    target advances only when strictly newer, so equal or older is superseded.
    A published head this repository cannot read is compared by the commit time
    latest.json records; without that record the order is unknown and refused.

    Eligibility is deliberately not limited to Hermit main. This mirror runs
    when a validation finishes, which is before a lander pushes the validated
    commit to main, so a main-only rule would withhold every pre-landing
    scorecard; and a lane rebased onto main is a descendant of main anyway, so
    a descendant-of-main rule would not exclude lane commits either. The
    consequence is that a validated lane-branch commit can own latest.json and
    the "Last regenerated" notice (on 2026-09-28 the published head was
    6580cd7386e7f91fcb05142dfa8695dfad563d4b, which is not on Hermit main)
    until a strictly newer commit replaces it. latest.json names that commit
    and its committer time, so a reader can always see which tree it was.
    """
    current = published_scorecard_head(writer)
    target_time = _commit_time(source_checkout, target)
    if target_time is None:
        raise MirrorRefused(f"measured source commit {target} is not readable in {source_checkout}")
    order = {"hermit_commit": target, "hermit_commit_time": target_time,
             "current": current and current["commit"], "current_time": current and current["time"]}
    if current is None:
        return {**order, "state": "first", "basis": "no published scorecard head"}
    head = current["commit"]
    if _git(source_checkout, "rev-parse", "--is-shallow-repository") != "false":
        raise MirrorRefused("measured checkout is shallow; scorecard head ancestry cannot be decided")
    head_time = _commit_time(source_checkout, head)
    if head_time is not None and current["time"] is not None and current["time"] != head_time:
        raise MirrorRefused(f"published scorecard head {head} records commit time "
                            f"{current['time']}, but the commit says {head_time}")
    if head == target:
        return {**order, "state": "same", "current_time": head_time, "basis": "published head is this commit"}
    if head_time is not None:
        order["current_time"] = head_time
        if _is_ancestor(source_checkout, head, target):
            return {**order, "state": "advance", "basis": "published head is an ancestor"}
        if _is_ancestor(source_checkout, target, head):
            return {**order, "state": "superseded", "basis": "this commit is an ancestor of the published head"}
        basis = "divergent history; committer time"
    elif current["time"] is not None:
        # An unreadable head cannot be an ancestor of a readable target in a
        # complete repository, so only the recorded time can order it.
        head_time = current["time"]
        basis = "published head unreadable here; recorded committer time"
    else:
        raise MirrorRefused(
            f"published scorecard head {head} is not in {source_checkout} and latest.json records "
            f"no {SCORECARD_HEAD_TIME_FIELD}; refusing to move the scorecard head without an order")
    if target_time > head_time:
        return {**order, "state": "advance", "basis": basis}
    return {**order, "state": "superseded", "basis": basis}


def write_metadata(repo: Path, snapshot: Mapping[str, Any], target: str,
                   series_commit: str, scorecard: Mapping[str, str], tool_commit: str,
                   order: Mapping[str, Any]) -> None:
    """Move latest.json to TARGET only when scorecard_order proved it is newer."""
    _verify_published_snapshot(snapshot)
    if order.get("state") not in {"first", "same", "advance"} or order.get("hermit_commit") != target:
        raise MirrorRefused(f"latest.json may not move to {target}: scorecard head order is "
                            f"{order.get('state')!r} against {order.get('current')}")
    current = published_scorecard_head(repo)
    if (current and current["commit"]) != order.get("current"):
        raise MirrorRefused("published scorecard head changed after it was ordered")
    source = snapshot["source"]
    value = {
        "schema": METADATA_SCHEMA, "source_repository": source["repository"],
        "source_commit": source["commit"], "source_tree": source["tree"],
        "rows": snapshot["rows_read"], "rows_sha256": snapshot["rows_sha256"],
        "local_sources": source["local_sources"], "series_commit": series_commit,
        "scorecard_hermit_commit": target, "scorecard": dict(sorted(scorecard.items())),
        "scorecard_tool_hermit_commit": tool_commit,
        SCORECARD_HEAD_TIME_FIELD: order["hermit_commit_time"],
    }
    _verify_published_snapshot(snapshot)
    _replace_writer_files(repo, {"latest.json": (json.dumps(value, indent=2, sort_keys=True) + "\n").encode()})


def _new_proof_paths(changes: Mapping[str, str], hashes: Mapping[str, str],
                     ignore: frozenset[str] = frozenset()) -> list[str]:
    """Proof attachment may only add the exact new files stage_carrier wrote."""
    paths = []
    for path, status in changes.items():
        if path in ignore:
            continue
        if status != "??" or path not in hashes or not path.startswith(finalized_proof.PREFIX + "/"):
            raise MirrorRefused(f"proof attachment changed an unexpected path: {path} ({status.strip()})")
        paths.append(path)
    return sorted(paths)


def unbound_proof_binding(writer: Path, snapshot: Mapping[str, Any], portable: Mapping[str, Any],
                          head: Mapping[str, Any] | None) -> dict[str, Any]:
    """Admit a proof-only attachment for a run whose scorecard never published.

    Only a commit-anchored, clean census terminal qualifies. The fetched
    ledger may hold no other carrier for its terminal event or its run. A
    carrier for this exact terminal (same repository, run, record, event and
    event digest) is a retry: it is named in ``retry_of``, and the attachment
    must then reproduce that carrier's path and every file byte for byte
    (stage_carrier refuses changed operands), so it reports unchanged.
    """
    row = portable["selection"]["row"]
    terminal = portable["selection"]["terminal"]
    if row.get("commit_anchored") is not True or row.get("tree_dirty") is not False:
        raise MirrorRefused("unbound proof-only attachment requires a commit-anchored run of a clean tree")
    commit = snapshot["source"]["commit"]
    identity = finalized_proof.carrier_identity({"terminal": terminal})
    retry_of = None
    for key, value in finalized_proof.read_carriers(writer, commit).items():
        if (value["terminal"]["event_id"] != terminal["event_id"]
                and value["terminal"]["run_id"] != terminal["run_id"]):
            continue
        if key != identity:
            raise MirrorRefused(
                f"ledger {commit} already carries a finalized proof for run {value['terminal']['run_id']} "
                f"(event {value['terminal']['event_id']}); refusing a second carrier for run "
                f"{terminal['run_id']} (event {terminal['event_id']})")
        retry_of = finalized_proof.carrier_path(value)
    return {"state": "unbound", "latest_hermit_commit": head and head["commit"], "commit": commit,
            "retry_of": retry_of}


def _mentions(value: Any, text: str) -> bool:
    if isinstance(value, str):
        return text in value
    if isinstance(value, dict):
        return any(_mentions(key, text) or _mentions(item, text) for key, item in value.items())
    if isinstance(value, list):
        return any(_mentions(item, text) for item in value)
    return False


def _rows_naming_run_since(writer: Path, base: str, head: str, run_id: str) -> bool | None:
    """Whether a ledger row mentioning RUN_ID was added after BASE, up to HEAD.

    The rows a render reads are append-only JSON Lines (series, parity and
    parity runs; quarantine too). Every change between the commits must be
    a derived scorecard output (latest.json or a file under scorecard/), or
    an ordinary .jsonl file that is new or keeps its old bytes, ending in a
    newline, as an exact prefix: Git's own blob identity of the prefix must
    equal the old blob id. Every appended line must be one JSON object, and
    one that mentions RUN_ID anywhere, in a key or a string value, answers
    True. A new refused-input file (series.quarantine_input_path), which
    holds the bytes a series-quarantine/v2 row held inline, must hash to its
    name within QUARANTINE_INPUT_FILE_CEILING_BYTES, and answers True when
    its bytes contain RUN_ID. Anything else (another kind of change, an
    edit, a removal, a file too large to read) returns None, meaning
    "cannot tell".
    """
    if base == head:
        return False
    changes = _raw_changes(writer, base, head)
    if changes is None:
        return None
    needle = run_id.encode()
    for change in changes:
        path = change.path
        if path == "latest.json" or path.startswith("scorecard/"):
            continue
        named = series._QUARANTINE_REF_RE.fullmatch(path)
        if named is not None:
            if change[:3] != _NEW_FILE or not named.group(2).startswith(named.group(1)):
                return None
            content = _stored_blob(writer, change.new, series.QUARANTINE_INPUT_FILE_CEILING_BYTES)
            if content is None or hashlib.sha256(content).hexdigest() != named.group(2):
                return None
            if needle in content:
                return True
            continue
        appended = _appended_bytes(writer, change) if path.endswith(".jsonl") else None
        if appended is None:
            return None
        for line in appended.splitlines():
            try:
                value = json.loads(line)
            except ValueError:
                return None
            if not isinstance(value, dict):
                return None
            if needle in line or _mentions(value, run_id):
                return True
    return False


def _receipt_path(parent: Path, run_id: str) -> Path:
    return parent / RECEIPTS_RELATIVE / f"{hashlib.sha256(run_id.encode()).hexdigest()}.json"


def completion_receipt(*, run_id: str, target: str, tool_commit: str, transaction: str,
                       series_commit: str, latest: bytes, finalized_row: bytes,
                       files: Mapping[str, str]) -> dict[str, Any]:
    """The receipt naming one scorecard transaction of a finalized run, exactly."""
    return {"schema": RECEIPT_SCHEMA, "run_id": run_id, "hermit_commit": target,
            "tool_hermit_commit": tool_commit, "transaction_commit": transaction,
            "series_commit": series_commit, "latest_sha256": hashlib.sha256(latest).hexdigest(),
            "finalized_row_sha256": hashlib.sha256(finalized_row).hexdigest(), "files": dict(files)}


def write_completion_receipt(parent: Path, writer: Path, *, run_id: str, target: str,
                             tool_commit: str, transaction: str, series_commit: str,
                             finalized_row: bytes, files: Mapping[str, str]) -> Path:
    """Record that this host rendered RUN_ID's scorecard and pushed it as TRANSACTION.

    Called only after build_scorecard's project-and-observe-results succeeded
    for this finalized run (the native renderer validates the history it
    writes: observation identities, attempt bindings, expected-output
    failures, and the generated files from the same cells), and
    _commit_and_push proved TRANSACTION on origin/main. The receipt binds the
    exact latest.json bytes TRANSACTION stores. It replaces any earlier
    receipt of the run, durably (written whole, fsynced, renamed).
    """
    oid = _blob_at(writer, transaction, "latest.json")
    latest = None if oid is None else _stored_blob(writer, oid, scorecard_cells.MANIFEST_MAX_BYTES)
    if latest is None:
        raise MirrorRefused(f"scorecard transaction {transaction} has no readable latest.json")
    receipt = completion_receipt(run_id=run_id, target=target, tool_commit=tool_commit,
                                 transaction=transaction, series_commit=series_commit,
                                 latest=latest, finalized_row=finalized_row, files=files)
    path = _receipt_path(parent, run_id)
    return series.write_quarantine_file(path.parent, path.name,
                                        json.dumps(receipt, indent=1, sort_keys=True) + "\n",
                                        replace=True)


def read_completion_receipt(parent: Path, run_id: str) -> tuple[Any, str]:
    """This host's completion receipt for RUN_ID as parsed JSON, or None and why."""
    path = _receipt_path(parent, run_id)
    try:
        descriptor = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_CLOEXEC | os.O_NONBLOCK)
    except FileNotFoundError:
        return None, f"{path} does not exist"
    except OSError as error:
        return None, f"{path} cannot be opened as a file: {error}"
    try:
        regular = stat.S_ISREG(os.fstat(descriptor).st_mode)
    except OSError as error:
        os.close(descriptor)
        return None, f"{path} cannot be examined: {error}"
    if not regular:
        os.close(descriptor)
        return None, f"{path} is not a regular file"
    with os.fdopen(descriptor, "rb") as handle:
        encoded = handle.read(_RECEIPT_MAX_BYTES + 1)
    if len(encoded) > _RECEIPT_MAX_BYTES:
        return None, f"{path} is larger than {_RECEIPT_MAX_BYTES} bytes"
    try:
        return json.loads(encoded), ""
    except ValueError as error:
        return None, f"{path} is not JSON: {error}"


def _record_completion(parent: Path, writer: Path, **receipt: Any) -> None:
    """write_completion_receipt, reporting rather than raising: the publication is already done."""
    try:
        path = write_completion_receipt(parent, writer, **receipt)
    except (*PUBLICATION_ERRORS, subprocess.SubprocessError) as error:
        print(f"ledger publication: SCORECARD COMPLETION RECEIPT NOT WRITTEN: {error}; a retry of run "
              f"{receipt['run_id']} renders the scorecard again", file=sys.stderr)
        return
    print(f"ledger publication: scorecard completion receipt {path} names transaction "
          f"{receipt['transaction']}", file=sys.stderr)


def superseded_receipt(*, run_id: str, target: str, current: str, tool_commit: str,
                       carrier_commit: str, series_commit: str, finalized_row: bytes,
                       files: Mapping[str, str]) -> dict[str, Any]:
    """The receipt naming a finalized run whose scorecard a newer head superseded, exactly."""
    return {"schema": SUPERSEDED_RECEIPT_SCHEMA, "outcome": "superseded", "run_id": run_id,
            "hermit_commit": target, "current_hermit_commit": current,
            "tool_hermit_commit": tool_commit, "carrier_commit": carrier_commit,
            "series_commit": series_commit,
            "finalized_row_sha256": hashlib.sha256(finalized_row).hexdigest(), "files": dict(files)}


def write_superseded_receipt(parent: Path, writer: Path, *, run_id: str, target: str, current: str,
                             tool_commit: str, carrier_commit: str, series_commit: str,
                             finalized_row: bytes, files: Mapping[str, str]) -> Path:
    """Record that a newer published head superseded RUN_ID's scorecard and its carrier is on main.

    "Superseded" is a complete outcome: scorecard_order found the published
    scorecard head newer than TARGET, and the head only moves forward, so no
    later publication of this run can render its scorecard; what the run still
    owes the ledger is its finalized carrier, which CARRIER_COMMIT holds.
    Without this receipt the outcome was never recorded
    (ledger_publication_outcome.record needs a receipt), so the unit's
    ExecStopPost finalization and the launcher each ran this whole publisher
    again only to find the scorecard superseded and the carrier unchanged.

    Called only after _commit_and_push proved CARRIER_COMMIT on origin/main,
    or, when the carrier was already published (an identical retry), with the
    published series commit it is in. Every file in FILES is read back from
    CARRIER_COMMIT and must have exactly its recorded SHA-256, and
    CARRIER_COMMIT must be an ancestor of the fetched origin/main. Replaces any
    earlier receipt of the run, durably, as write_completion_receipt does.
    """
    receipt = superseded_receipt(run_id=run_id, target=target, current=current, tool_commit=tool_commit,
                                 carrier_commit=carrier_commit, series_commit=series_commit,
                                 finalized_row=finalized_row, files=files)
    return write_carrier_receipt(parent, writer, receipt)


def write_carrier_receipt(parent: Path, writer: Path, receipt: Mapping[str, Any]) -> Path:
    """Write a superseded or deferred RECEIPT once its carrier files are on main.

    Every file the receipt names is read back from its carrier commit and must
    have exactly its recorded SHA-256, and that commit must be an ancestor of
    the fetched origin/main.
    """
    run_id, carrier_commit, files = receipt["run_id"], receipt["carrier_commit"], receipt["files"]
    if not files:
        raise MirrorRefused(f"{receipt['outcome']} run {run_id} names no carrier file")
    if not _published(writer, carrier_commit):
        raise MirrorRefused(f"carrier commit {carrier_commit} is not on the fetched main")
    for path, digest in sorted(files.items()):
        oid = _blob_at(writer, carrier_commit, path)
        encoded = None if oid is None else _stored_blob(writer, oid, _SUPERSEDED_FILE_MAX_BYTES)
        if encoded is None or hashlib.sha256(encoded).hexdigest() != digest:
            raise MirrorRefused(f"ledger commit {carrier_commit} does not hold carrier file {path} "
                                f"with SHA-256 {digest}")
    path = _receipt_path(parent, run_id)
    return series.write_quarantine_file(path.parent, path.name,
                                        json.dumps(receipt, indent=1, sort_keys=True) + "\n",
                                        replace=True)


def _record_superseded(parent: Path, writer: Path, **receipt: Any) -> None:
    """write_superseded_receipt, reporting rather than raising: the carrier is already published."""
    try:
        path = write_superseded_receipt(parent, writer, **receipt)
    except (*PUBLICATION_ERRORS, subprocess.SubprocessError) as error:
        print(f"ledger publication: SUPERSEDED RECEIPT NOT WRITTEN: {error}; a later caller of run "
              f"{receipt['run_id']} runs the publisher again", file=sys.stderr)
        return
    print(f"ledger publication: superseded receipt {path} names carrier commit "
          f"{receipt['carrier_commit']} (scorecard head {receipt['current']})", file=sys.stderr)


def deferred_receipt(*, run_id: str, target: str, current: str, tool_commit: str,
                     carrier_commit: str, series_commit: str, regenerated_at: int | None,
                     finalized_row: bytes, files: Mapping[str, str],
                     reason: str | None = None) -> dict[str, Any]:
    """The receipt naming a finalized run whose scorecard regeneration was deferred, exactly.

    REASON is scorecard_coalesce.REASON for a render deferred to a queued
    publication (queued_deferral); an interval deferral carries none, so its
    receipt is unchanged.
    """
    receipt = {"schema": DEFERRED_RECEIPT_SCHEMA, "outcome": "deferred", "run_id": run_id,
               "hermit_commit": target, "current_hermit_commit": current,
               "tool_hermit_commit": tool_commit, "carrier_commit": carrier_commit,
               "series_commit": series_commit, "regenerated_at": regenerated_at,
               "finalized_row_sha256": hashlib.sha256(finalized_row).hexdigest(), "files": dict(files)}
    if reason is not None:
        receipt["reason"] = reason
    return receipt


def _record_deferred(parent: Path, writer: Path, **arguments: Any) -> None:
    """Record that RUN_ID's regeneration was deferred and its carrier is on main.

    "Deferred" is a complete outcome for the run: its rows and its finalized
    carrier are on main, and the next regeneration projects its rows
    (SCORECARD_REGENERATION_INTERVAL_SECONDS). Reports rather than raises: the
    carrier is already published.
    """
    try:
        path = write_carrier_receipt(parent, writer, deferred_receipt(**arguments))
    except (*PUBLICATION_ERRORS, subprocess.SubprocessError) as error:
        print(f"ledger publication: DEFERRED RECEIPT NOT WRITTEN: {error}; a later caller of run "
              f"{arguments['run_id']} runs the publisher again", file=sys.stderr)
        return
    print(f"ledger publication: deferred receipt {path} names carrier commit "
          f"{arguments['carrier_commit']} (scorecard head {arguments['current']})", file=sys.stderr)


def last_regeneration_time(writer: Path, commit: str) -> int | None:
    """Committer time of the newest commit at or before COMMIT that changed latest.json, or None.

    Only a regenerated scorecard (or a legacy export) moves latest.json; a
    proof attached alone, superseded or deferred leaves it unchanged. 28 ms on
    the real ledger on 2026-10-11. Through a squash snapshot the time is the
    old tip's (ledger_snapshot.last_change_time), not the squash's.
    """
    def read(*args: str) -> str | None:
        result = _run(["git", "-C", str(writer), *args], check=False)
        return result.stdout if result.returncode == 0 else None
    return ledger_snapshot.last_change_time(read, commit, "latest.json")


def regeneration_deferral(writer: Path, commit: str, *, now: float | None = None) -> dict[str, int] | None:
    """When the last regeneration at COMMIT is younger than the interval, its time and the interval."""
    interval = SCORECARD_REGENERATION_INTERVAL_SECONDS
    if interval <= 0:
        return None
    last = last_regeneration_time(writer, commit)
    if last is None:
        return None
    elapsed = int(time.time() if now is None else now) - last
    # A regeneration time in the future (another host's clock) defers nothing.
    if not 0 <= elapsed < interval:
        return None
    return {"regenerated_at": last, "elapsed": elapsed, "interval": interval}


#: The transient timer and service a publication deferred by the regeneration
#: interval arms, so the deferred rows reach the scorecard once the interval has
#: passed even when no later publication comes. Before this (2026-10-20), rows
#: deferred late at night waited for ci-hub/ledger/nightly_maintenance.py at
#: 3:00 AM ET: the 075247ba publication of 03:58Z deferred 8,655 parity records
#: ("last regenerated 1889 s ago") and nothing landed after it.
CATCHUP_RENDER_UNIT = "hermit-ledger-catchup-render"
#: Past the interval, so nightly_maintenance's render_decision accepts the render.
CATCHUP_RENDER_MARGIN_SECONDS = 120
CATCHUP_RENDER_TIMEOUT_SECONDS = 5400
#: Set to 0 to arm nothing.
CATCHUP_RENDER_ENV = "DEV_HERMIT_LEDGER_CATCHUP_RENDER"


def catchup_render_delay_seconds(deferral: Mapping[str, int]) -> int:
    """Seconds from now until the deferred regeneration is due, plus the margin."""
    return max(60, deferral["interval"] - deferral["elapsed"] + CATCHUP_RENDER_MARGIN_SECONDS)


def catchup_render_command(parent: Path, systemd_run: str, deferral: Mapping[str, int],
                           home: str, python: str) -> list[str]:
    """`systemd-run` arming one `nightly_maintenance.py --render-only` at the due time.

    The environment is hermit-ledger-nightly.service's: python3 3.12 first, then
    ~/bin (gh, with-proxy) and ~/.cargo/bin (rust-script builds the renderer).
    """
    return [
        systemd_run, "--user", "--collect", f"--unit={CATCHUP_RENDER_UNIT}",
        f"--on-active={catchup_render_delay_seconds(deferral)}s", "--timer-property=AccuracySec=30s",
        f"--working-directory={parent}", f"--setenv=HOME={home}",
        f"--setenv=PATH=/usr/local/bin:{home}/bin:{home}/.cargo/bin:/usr/bin:/bin",
        f"--setenv=DEV_HERMIT_PARENT={parent}",
        f"--property=TimeoutStartSec={CATCHUP_RENDER_TIMEOUT_SECONDS}",
        python, str(parent / "ci-hub/ledger/nightly_maintenance.py"), "--parent", str(parent), "--render-only",
    ]


def arm_catchup_render(parent: Path, deferral: Mapping[str, int], *, run: Any = subprocess.run,
                       which: Any = shutil.which, environ: Mapping[str, str] = os.environ) -> str:
    """Arm the one-shot catch-up render, and say what happened. Never raises: the
    rows are published whatever becomes of it, and the nightly render remains."""
    if environ.get(CATCHUP_RENDER_ENV) == "0":
        return f"catch-up render not armed: {CATCHUP_RENDER_ENV}=0"
    # Only a real state root carries the job; a test's temporary parent does not,
    # so a test that defers never arms a real timer.
    if not (parent / "ci-hub/ledger/nightly_maintenance.py").is_file():
        return ("catch-up render not armed: no ci-hub/ledger/nightly_maintenance.py under "
                f"{parent}; the nightly render picks them up")
    systemd_run = which("systemd-run")
    home = environ.get("HOME", "")
    if systemd_run is None or not home:
        return "catch-up render not armed: no systemd-run or HOME; the nightly render picks them up"
    command = catchup_render_command(parent, systemd_run, deferral, home, sys.executable)
    try:
        result = run(command, capture_output=True, text=True, check=False, timeout=60)
    except (OSError, subprocess.SubprocessError) as error:
        return f"catch-up render not armed: {error}; the nightly render picks them up"
    if result.returncode == 0:
        return (f"catch-up render {CATCHUP_RENDER_UNIT} armed in "
                f"{catchup_render_delay_seconds(deferral)} s")
    text = " ".join(((result.stderr or "") + (result.stdout or "")).split())
    if "already exists" in text or "already loaded" in text:
        return f"catch-up render {CATCHUP_RENDER_UNIT} already armed"
    return f"catch-up render not armed: {text[-300:]}; the nightly render picks them up"


def coalesce_staleness_cap_seconds() -> int:
    """The age past which no render is deferred to a queued publication: interval + margin (3 h)."""
    return SCORECARD_REGENERATION_INTERVAL_SECONDS + COALESCE_CAP_MARGIN_SECONDS


def queued_deferral_staleness_bound_seconds() -> int:
    """How stale the published scorecard can get through queued deferrals, at most.

    Past the cap the next holder renders. A holder that deferred just before
    the cap still finishes its own publication, and the next one renders, each
    within PUBLICATION_HOLD_SECONDS: 10,800 + 2 x 3,000 = 16,800 s (4 h 40 min)
    with the production interval. When the queue empties instead,
    ci-hub/ledger/publish_backlog.py renders (ORPHANED_DEFERRAL_BOUND_SECONDS).
    """
    return coalesce_staleness_cap_seconds() + 2 * PUBLICATION_HOLD_SECONDS


def _coalescing_capped(parent: Path, writer: Path, series_commit: str, *,
                       now: float | None = None) -> str | None:
    """Why this render may not be deferred any longer, or None when it may."""
    cap = coalesce_staleness_cap_seconds()
    current = time.time() if now is None else now
    try:
        regenerated = last_regeneration_time(writer, series_commit)
    except (*PUBLICATION_ERRORS, subprocess.SubprocessError) as error:
        return f"the last regeneration time cannot be read: {error}"
    if regenerated is None:
        return "no scorecard regeneration is on the ledger"
    if current - regenerated >= cap:
        return (f"the scorecard was last regenerated {int(current - regenerated)} s ago, at or past the "
                f"{cap} s coalescing cap")
    try:
        pending = scorecard_coalesce.pending(parent)
    except OSError as error:
        return f"the pending deferrals cannot be read: {error}"
    for _path, value in pending:
        deferred = value.get("deferred_at_epoch")
        if type(deferred) is not int:
            return f"pending deferral of {value.get('run_id')} records no deferral time"
        if current - deferred >= cap:
            return (f"the deferral of {value['run_id']} has waited {int(current - deferred)} s, at or past "
                    f"the {cap} s coalescing cap")
    return None


def _renders_after(source_checkout: Path, target: str, queued: str | None) -> str | None:
    """Why a publication of QUEUED, rendering after TARGET, would still render; None when not provably.

    Serialized renders end with QUEUED's render only when scorecard_order
    would let QUEUED replace TARGET as the head: the same commit, a
    descendant, or a divergent commit with a strictly newer committer time.
    Deferring only to such a waiter keeps the burst's final scorecard the one
    serialized renders publish. An unknown commit, or one TARGET would
    supersede, is never deferred to.
    """
    if queued is None:
        return None
    if queued == target:
        return "the queued publication measured this commit"
    try:
        queued_time = _commit_time(source_checkout, queued)
        if queued_time is None:
            return None
        if _is_ancestor(source_checkout, target, queued):
            return "the queued publication measured a descendant of this commit"
        if _is_ancestor(source_checkout, queued, target):
            return None
        target_time = _commit_time(source_checkout, target)
    except (*PUBLICATION_ERRORS, subprocess.SubprocessError):
        return None
    if target_time is not None and queued_time > target_time:
        return "the queued publication measured a divergent commit with a newer committer time"
    return None


def queued_deferral(parent: Path, writer: Path, source_checkout: Path, *, run_id: str, target: str,
                    series_commit: str) -> dict[str, Any] | None:
    """Defer this render to a publication queued on publication.lock, or None to render now.

    Only for a caller that holds ignored/validate/publication.lock
    (--coalesce-scorecard, which start_unit's publication unit passes). The
    waiter's render reads the whole published history, including the rows this
    run just published, so this one would be overwritten minutes later
    (design A, ignored/tickhub-ops/pubscan/item3-scorecard-design.md). Defers
    only on positive evidence (scorecard_coalesce.live_waiters, _renders_after)
    and only after the pick-up marker is durable; anything unknown renders.
    Never past the coalescing cap (_coalescing_capped), whoever is waiting.
    """
    waiters = scorecard_coalesce.live_waiters(parent, exclude_run_id=run_id)
    if not waiters:
        return None
    capped = _coalescing_capped(parent, writer, series_commit)
    if capped is not None:
        print(f"ledger publication: rendering although publication {waiters[0]['run_id']} is queued: "
              f"{capped}", file=sys.stderr)
        return None
    for waiter in waiters:
        basis = _renders_after(source_checkout, target, waiter["target"])
        if basis is None:
            continue
        try:
            marker = scorecard_coalesce.record_deferral(parent, run_id=run_id, target=target,
                                                        series_commit=series_commit, waiter=waiter)
        except (OSError, ValueError) as error:
            print(f"ledger publication: rendering; the deferral to queued publication {waiter['run_id']} "
                  f"cannot be recorded: {error}", file=sys.stderr)
            return None
        try:
            regenerated_at = last_regeneration_time(writer, series_commit)
        except (*PUBLICATION_ERRORS, subprocess.SubprocessError):
            regenerated_at = None
        return {"reason": scorecard_coalesce.REASON, "waiter": waiter, "basis": basis,
                "marker": str(marker), "regenerated_at": regenerated_at}
    return None


def _mark_covered_deferrals(report: dict[str, Any], parent: Path, writer: Path, commit: str) -> None:
    """Record which queued deferrals the scorecard just pushed at COMMIT covers; never raises."""
    try:
        covered = scorecard_coalesce.mark_covered(parent, writer, commit)
    except (OSError, ValueError, subprocess.SubprocessError) as error:
        print(f"ledger publication: queued deferrals not marked covered ({error}); the backlog pass "
              "marks them", file=sys.stderr)
        return
    if covered:
        report["covered_deferrals"] = covered
        print(f"ledger publication: this render covers the deferred scorecard of {', '.join(covered)}",
              file=sys.stderr)


def published_run_scorecard(
    writer: Path, snapshot: Mapping[str, Any], *, parent: Path, source_checkout: Path,
    target: str, portable: dict[str, Any], finalized_row: bytes, finalized_run_id: str,
    tool_checkout: Path, results: Path | None, net_wrapper: str,
) -> tuple[dict[str, Any] | None, str]:
    """The scorecard outcome this finalized run already published, or None and why not.

    One ops tick runs this tool up to three times for the same run: inside the
    validation, then from the unit's stop-post finalization, then from the
    launcher. The in-validation publication's success is recorded only in the
    service result, not in the run record the later callers read, so each of
    them used to render the whole scorecard again. The render reads every row
    in the ledger (https://github.com/rrnewton/hermit/issues/3640): 34 minutes
    on 2026-10-03, so the second render was killed at the stop-post's 600 s
    and the third by the launcher's RuntimeMaxSec, and the run was refused
    retirement although its scorecard was public.

    The ledger alone cannot show that a transaction is complete: anyone who
    can push to it can write a transaction whose metadata, projection, carrier
    and digests pass every ledger check below, with a history or generated
    files the native renderer would refuse; the native validation of a
    history (scorecard.rs load_existing and validate_attempt_bindings) runs
    only inside a render. So the completion evidence is this host's receipt
    (write_completion_receipt), written only after this tool's render of
    this run succeeded and its transaction reached origin/main, and kept in
    this state root beside the writer, at the trust level of the tool itself.

    A second render is skipped only when all of these hold:

    - this host's completion receipt for the run names exactly this
      transaction, its source commit, its latest.json bytes and scorecard files,
      this measured commit, this tool commit and the prepared finalized row;
    - HEAD holds this run's finalized carrier, for the canonical terminal and
      parent event this invocation prepared (prepare_portable_proof,
      finalized_proof.carrier_for_row), and every operand object the carrier
      names is present at HEAD with its exact bytes (read_artifact_objects);
    - exactly one commit added that carrier, and it is one complete scorecard
      transaction this tool makes (derived_commit_base under the scorecard
      message: one parent, every change bound by digest in its latest.json),
      on fetched main and an ancestor of HEAD;
    - that latest.json names this measured commit, this scorecard tool
      commit, one source and series commit, the carrier and every scorecard
      file (MIRRORED_SCORECARD_PATHS), the carrier names the same series
      commit, and its row count, rows digest and series tree equal the
      projection recomputed from that source commit, whose every event
      survives unchanged at HEAD (the checks published_scorecard_binding
      applies). The source commit is the transaction's own parent, or an
      ancestor from which that parent differs only by appended rows and
      other runs' proofs (_appends_since): a transaction rebuilt over
      another host's appends after its push lost a race;
    - the history that transaction stored hashes to its recorded digest and
      is the projection of those rows refreshed at this run's start, has the
      schema number, cells list and pre_series_corpus flag the renderer
      writes (TrackedCells, ObservationProjection), and records
      at least one result of this run at this measured commit from those
      rows;
    - no ledger row mentioning this run was added after that source commit
      (_rows_naming_run_since), so the render read every row the run
      published, including any this invocation's raw publication added;
    - the operands are this run's: schema-10 operands equal the ones prepared
      from the run's artifacts, and derived operands equal the ones emitted
      again from its raw results, as a retry's stage_carrier requires; and
    - immediately before the answer, as before any publication: the
      canonical producer row, the measured source HEAD and the scorecard tool
      are unchanged, and the canonical terminal on fetched parent main is
      still the one prepared, with the carrier's origin verified
      (finish_portable_proof's recheck).

    Anything else returns None and the caller renders exactly as before.
    """
    selection = portable["selection"]
    terminal = selection["terminal"]
    if terminal.get("run_id") != finalized_run_id or terminal.get("hermit_commit") != target:
        return None, "the prepared terminal is not this run at this measured commit"
    head = snapshot["source"]["commit"]
    carrier = finalized_proof.carrier_for_row(
        finalized_proof.read_carriers(writer, head), selection["row"], terminal)
    if carrier is None:
        return None, f"ledger {head} holds no finalized carrier for this run"
    if carrier["parent"] != selection["parent"]:
        return None, "the published carrier names another parent event"
    if not portable.get("derived") and carrier["artifacts"] != portable["artifacts"]:
        return None, "the published carrier binds other operands"
    path = finalized_proof.carrier_path(carrier)
    added = _git_stored(writer, "log", "--full-history", "--no-renames", "--diff-filter=A",
                        "--format=%H", head, "--", path).decode().split()
    if len(added) != 1:
        return None, f"{len(added)} commits added the carrier {path}"
    transaction = added[0]
    classified = derived_commit_base(writer, transaction)
    if classified is None or classified[1] != SCORECARD_COMMIT_MESSAGE:
        return None, f"carrier commit {transaction} is not a complete scorecard transaction"
    if not (_stored_ancestor(writer, transaction, head) and _published(writer, head)):
        return None, f"scorecard transaction {transaction} is not on fetched main"
    oid = _blob_at(writer, transaction, "latest.json")
    encoded = None if oid is None else _stored_blob(writer, oid, scorecard_cells.MANIFEST_MAX_BYTES)
    latest = None if encoded is None else finalized_proof._json(encoded)
    if not isinstance(latest, dict):
        return None, f"scorecard transaction {transaction} has no readable latest.json"
    # A transaction rebuilt over another host's appends after a lost push race
    # (_push_rebuilt_over_appends) sits on a later main than the rows it was
    # rendered from, which latest.json names.
    base = latest.get("source_commit")
    if not isinstance(base, str) or not _COMMIT.fullmatch(base):
        return None, f"scorecard transaction {transaction} names no source commit"
    if base != classified[0] and _appends_since(
            writer, base, classified[0], runs={carrier["terminal"]["run_id"]},
            events={carrier["terminal"]["event_id"]})[0] is None:
        return None, (f"scorecard transaction {transaction} sits on {classified[0]}, which is not "
                      f"{base} plus appended rows and other runs' proofs")
    tool_commit = _committed_tool_source(tool_checkout.resolve())
    hashes = latest.get("scorecard")
    if (latest.get("schema") != METADATA_SCHEMA or latest.get("source_repository") != REMOTE_URL
            or latest.get("scorecard_hermit_commit") != target
            or latest.get("scorecard_tool_hermit_commit") != tool_commit
            or latest.get("series_commit") != base
            or carrier["series"]["commit"] != base
            or not isinstance(hashes, dict) or path not in hashes
            or not all(isinstance(hashes.get(mirrored.as_posix()), str)
                       for mirrored in MIRRORED_SCORECARD_PATHS)):
        return None, (f"scorecard transaction {transaction} does not bind this measured commit, "
                      "tool, carrier and rows")
    # Native completion: the ledger's own bytes cannot show it (see above).
    receipt, problem = read_completion_receipt(parent, finalized_run_id)
    expected = completion_receipt(run_id=finalized_run_id, target=target, tool_commit=tool_commit,
                                  transaction=transaction, series_commit=base, latest=encoded,
                                  finalized_row=finalized_row, files=hashes)
    if receipt is None:
        return None, f"this host holds no completion receipt of a render of run {finalized_run_id}: {problem}"
    if receipt != expected:
        differing = (sorted(key for key in expected.keys() | receipt.keys()
                            if key not in receipt or key not in expected or receipt[key] != expected[key])
                     if isinstance(receipt, dict) else ["the whole receipt"])
        return None, (f"this host's completion receipt of run {finalized_run_id} does not name scorecard "
                      f"transaction {transaction} as it is: {', '.join(differing)} differ")
    later = _rows_naming_run_since(writer, base, head, finalized_run_id)
    if later is not False:
        return None, (f"rows mentioning this run were added after {base}" if later
                      else f"changes after {base} cannot be read as appended JSON Lines rows")
    # The structure above is not completion: recompute what the rows were.
    projected_scope = (contextlib.nullcontext(snapshot) if base == head else
                       _published_snapshot_scope(writer, base))
    with projected_scope as projected:
        if (latest.get("source_tree") != projected["source"]["tree"]
                or latest.get("rows_sha256") != projected["rows_sha256"]
                or latest.get("rows") != projected["rows_read"]
                or carrier["series"].get("tree") != projected["source"]["tree"]
                or carrier["series"].get("rows_sha256") != projected["rows_sha256"]):
            return None, f"scorecard transaction {transaction} metadata differs from the rows of {base}"
        current_rows = _rows_by_event_id(snapshot["rows"])
        if any(current_rows.get(event_id) != text for event_id, text in _rows_by_event_id(projected["rows"]).items()):
            return None, f"rows of {base} were changed or removed by {head}"
        stored = scorecard_cells.read_commit(writer, transaction,
                                             environment=_environment({"GIT_NO_REPLACE_OBJECTS": "1"}))
        if stored is None or hashlib.sha256(stored).hexdigest() != hashes[scorecard_cells.LOGICAL_PATH]:
            return None, f"scorecard transaction {transaction} stores no history with its recorded digest"
        try:
            history = json.loads(stored)
        except ValueError:
            return None, f"the history in scorecard transaction {transaction} is not JSON"
        projection = history.get("projection") if isinstance(history, dict) else None
        bindings = (projection.get("comparison_attempt_bindings_v1") if isinstance(projection, dict)
                    else None)
        bindings = bindings.get("bindings") if isinstance(bindings, dict) else None
        if (not isinstance(bindings, list)
                or projection.get("source") != projected["source"]["path"]
                or projection.get("source_repository") != REMOTE_URL
                or projection.get("source_commit") != base
                or projection.get("source_tree") != projected["source"]["tree"]
                or projection.get("rows_read") != projected["rows_read"]
                or projection.get("refreshed_at") != selection["row"]["started_at"]):
            return None, (f"the history in scorecard transaction {transaction} is not the projection "
                          f"of {base} refreshed at this run's start")
        if (type(history.get("schema")) is not int or not isinstance(history.get("cells"), list)
                or type(projection.get("pre_series_corpus")) is not bool):
            return None, (f"the history in scorecard transaction {transaction} lacks the schema number, cells "
                          "list or pre_series_corpus flag the renderer writes")
        if not any(isinstance(binding, dict) and binding.get("run_id") == finalized_run_id
                   and binding.get("hermit_sha") == target and isinstance(binding.get("snapshot"), dict)
                   and binding["snapshot"].get("commit") == base
                   and binding["snapshot"].get("rows_sha256") == projected["rows_sha256"]
                   for binding in bindings):
            return None, (f"the history in scorecard transaction {transaction} records no result of "
                          f"this run at {target} from the rows of {base}")
        # The carrier's operands at HEAD, the bytes a later export reads.
        finalized_proof.read_artifact_objects(writer, head, carrier)
        if portable.get("derived"):
            if results is None:
                return None, "a derived carrier cannot be checked without the run's raw results"
            emit_derived_portable_proof(portable, parent, results, None, net_wrapper=net_wrapper)
            if carrier["artifacts"] != portable["artifacts"]:
                return None, "the published carrier binds other operands than this run's raw results"
        # The rechecks build_scorecard and finish_portable_proof make before publishing.
        if finalized_producer_row(parent, finalized_run_id, target, source_checkout) != finalized_row:
            return None, "the canonical finalized producer changed after preparation"
        if (_git(source_checkout, "rev-parse", "HEAD^{commit}") != target
                or _committed_tool_source(tool_checkout.resolve()) != tool_commit):
            return None, "the measured source or the scorecard tool changed during the check"
        namespace, current = _select_published_terminal(portable["source"], parent, selection["row"],
                                                        net_wrapper=net_wrapper,
                                                        cache=portable)
        with namespace:
            if current["terminal"] != terminal:
                return None, "the canonical terminal changed after preparation"
            namespace.verify_origin(carrier["parent"], carrier["terminal"])
        _verify_published_snapshot(projected)
        _verify_published_snapshot(snapshot)
        return ({"state": "published", "files": dict(hashes), "hermit_commit": target,
                 "tool_hermit_commit": tool_commit, "transaction_commit": transaction},
                f"published at {transaction} from rows through {base}")


def _in_rust_tool_cache_session(function):
    """Hold any rust tool cache slot a publication prepares until it returns."""
    @functools.wraps(function)
    def publication(*args: Any, **kwargs: Any) -> Any:
        with rust_tool_cache.session():
            return function(*args, **kwargs)
    return publication


@_in_rust_tool_cache_session
def mirror(
    parent: Path, source_checkout: Path | None, target: str | None, remote_url: str,
    *, net_wrapper: str, attempts: int = DEFAULT_ATTEMPTS, results: Path | None = None,
    publish_only: bool = False, scorecard_command: str | None = None,
    scorecard_args: Sequence[str] = (),
    tool_checkout: Path | None = None, finalized_run_id: str | None = None,
    lifecycle_deadline_ns: int | None = None,
    portable_proof_only: bool = False,
    quarantine_only: bool = False,
    coalesce_scorecard: bool = False,
) -> dict[str, Any]:
    if remote_url != REMOTE_URL:
        raise MirrorRefused("unexpected ledger repository")
    if quarantine_only and not publish_only:
        raise MirrorRefused("quarantine-only publication requires --publish-only")
    if portable_proof_only and (finalized_run_id is None or results is None or publish_only or scorecard_command):
        raise MirrorRefused("proof-only attachment requires --finalized-run-id/--results and no scorecard override")
    if coalesce_scorecard and (finalized_run_id is None or publish_only or portable_proof_only or scorecard_command):
        raise MirrorRefused("--coalesce-scorecard applies only to an ordinary finalized publication")
    # A scorecard command run in a Hermit checkout without the publication lock
    # hands itself here (scorecard.rs publish_history_command), with that
    # checkout's HEAD as the target. From a scratch clone that is a commit no
    # validation measured, and the render goes to the shared writer and the
    # public ledger: on 2026-10-11 12:11Z files of hermit 6c0043760f71 (on no
    # remote, in no run record) were left in the writer and stopped every
    # publication for 1 h 45 min. Refuse before touching the writer.
    if (scorecard_command is not None and not publish_only and target is not None
            and not _known_hermit_commit(parent, target)):
        raise MirrorRefused(
            f"scorecard command for hermit {target}, which no validation measured (in no run record "
            "and not in the parent's Hermit checkout): a scratch checkout must not publish to the "
            "shared ledger writer; nothing was changed")
    legacy_export = scorecard_command == "export-legacy"
    if not legacy_export:
        # Raw publication and snapshots use a different native package from
        # the scorecard. Prepare it in its own actual caller environment; the
        # real rows are still validated again inside the writer transaction.
        series.prepare_shared_checker()
    finalized_row = None
    portable = None
    preparation_error = None
    prepared_tool = None
    tool_preparation_error = None
    if finalized_run_id is not None and not publish_only:
        try:
            if source_checkout is None or target is None:
                raise MirrorRefused("finalized publication requires measured source identity")
            finalized_row = finalized_producer_row(parent, finalized_run_id, target, source_checkout)
            refuse_non_cargo_payload(finalized_row)
            portable = prepare_portable_proof(parent, finalized_row, net_wrapper=net_wrapper, attempts=attempts)
        except (MirrorRefused, OSError, ValueError, KeyError, series.SeriesError,
                publisher.PublishRefused, subprocess.SubprocessError) as error:
            # Raw cell rows still publish through the normal transaction below.
            preparation_error = error
    if not publish_only and not portable_proof_only and preparation_error is None:
        try:
            if source_checkout is None or target is None:
                raise MirrorRefused("scorecard preparation requires source checkout and target")
            prepared_tool = prepare_scorecard(
                source_checkout, target, tool_checkout=tool_checkout,
                state_root=parent, net_wrapper=net_wrapper,
            )
        except (MirrorRefused, OSError, ValueError) as error:
            # Compilation cannot acquire publication authority or prevent the
            # independent, durable raw-row append below.
            tool_preparation_error = error
    cleaned: list[str] = []
    with publication_writer(parent, net_wrapper=net_wrapper, lifecycle_deadline_ns=lifecycle_deadline_ns,
                            cleaned=cleaned) as (ledger, writer, lock_fd), contextlib.ExitStack() as snapshots:
        if legacy_export or portable_proof_only:
            # Preserving legacy bytes must neither drain unrelated pending rows
            # nor claim that the existing scorecard was regenerated.
            if not _published(writer, _git(writer, "rev-parse", "HEAD^{commit}")):
                raise MirrorRefused("ledger writer HEAD is not on the fetched main")
            _git(writer, "checkout", "--no-recurse-submodules", "--detach", "refs/remotes/origin/main")
            commit = _git(writer, "rev-parse", "HEAD^{commit}")
            publication = {"published": 0, "attempts": 0, "skipped":
                           "portable proof attachment only" if portable_proof_only else "legacy archive only"}
            snapshot = (_published_snapshot(writer) if portable_proof_only else
                        {"source": {"repository": REMOTE_URL, "commit": commit,
                                    "tree": series._series_tree_at(writer, commit)}})
        else:
            publication = publish_series(parent, ledger, writer, net_wrapper=net_wrapper, attempts=attempts,
                                         run_id=finalized_run_id, series_rows=not quarantine_only)
            # A publish-only run reports where the series now is, not its rows
            # (_published_source); reading every row only to count them was
            # 191.6 s of a 218.7 s warm publish-only run on 2026-10-12.
            snapshot = _published_source(writer) if publish_only else _published_snapshot(writer)
        snapshots.callback(_close_published_snapshot, snapshot)
        series_commit = snapshot["source"]["commit"]
        parity_published = (publication.get("parity") or {}).get("published")
        report = {
            "state": "published" if publication.get("published") or parity_published else "unchanged",
            "repository": REMOTE_URL, "commit": series_commit,
            "source_repository": REMOTE_URL, "source_commit": series_commit,
            "source_tree": snapshot["source"]["tree"],
            "series_commit": series_commit, "raw_publication": publication,
            "published": publication.get("published", 0),
        }
        if "rows_read" in snapshot:
            report["rows"] = snapshot["rows_read"]
        if cleaned:
            report["writer_cleaned"] = cleaned
        if publish_only:
            _verify_published_snapshot(snapshot)
            return report
        try:
            if portable_proof_only:
                _attach_proof_only(report, parent=parent, writer=writer, snapshot=snapshot, target=target,
                                   portable=portable, preparation_error=preparation_error,
                                   results=results, tool_checkout=tool_checkout, net_wrapper=net_wrapper)
                _verify_published_snapshot(snapshot)
                return report
            if (portable is not None and finalized_row is not None and finalized_run_id is not None
                    and target is not None and source_checkout is not None and scorecard_command is None):
                try:
                    published, why = published_run_scorecard(
                        writer, snapshot, parent=parent, source_checkout=source_checkout,
                        target=target, portable=portable, finalized_row=finalized_row,
                        finalized_run_id=finalized_run_id,
                        tool_checkout=tool_checkout or source_checkout, results=results,
                        net_wrapper=net_wrapper)
                except (*PUBLICATION_ERRORS, *_UNPROVABLE, publisher.PublishRefused,
                        subprocess.SubprocessError) as error:
                    published, why = None, f"cannot be decided: {error}"
                if published is not None:
                    _verify_published_snapshot(snapshot)
                    report["scorecard"] = published
                    print(f"ledger publication: SCORECARD ALREADY PUBLISHED by run {finalized_run_id}: "
                          f"{why}; not rendered again", file=sys.stderr)
                    return report
                print(f"ledger publication: rendering the scorecard; this run's publication is not "
                      f"complete on main: {why}", file=sys.stderr)
            _publish_scorecard(
                report, parent=parent, writer=writer, snapshot=snapshot, lock_fd=lock_fd,
                source_checkout=source_checkout, target=target, portable=portable,
                preparation_error=preparation_error, tool_preparation_error=tool_preparation_error,
                prepared_tool=prepared_tool, finalized_row=finalized_row, results=results,
                scorecard_command=scorecard_command, scorecard_args=scorecard_args,
                tool_checkout=tool_checkout, finalized_run_id=finalized_run_id, net_wrapper=net_wrapper,
                lifecycle_deadline_ns=lifecycle_deadline_ns, coalesce=coalesce_scorecard)
            return report
        finally:
            # A render copied back and then not published (a refused proof,
            # metadata or push) is set aside now, not by the next run's
            # start, so the writer is left clean whatever the outcome.
            try:
                outcome = set_aside_render_leftovers(writer)
            except PUBLICATION_ERRORS as error:
                report["writer_cleanup_error"] = str(error)
                print(f"ledger publication: WRITER NOT CLEANED: {error}", file=sys.stderr)
            else:
                if outcome is not None:
                    cleaned.append(outcome)
                    report["writer_cleaned"] = cleaned


PUBLICATION_ERRORS = (MirrorRefused, OSError, ValueError, KeyError, series.SeriesError)


def _attach_proof_only(report: dict[str, Any], *, parent: Path, writer: Path,
                       snapshot: Mapping[str, Any], target: str | None, portable: dict | None,
                       preparation_error: BaseException | None, results: Path | None,
                       tool_checkout: Path | None, net_wrapper: str) -> None:
    """Attach a run's finalized carrier; never touch the scorecard or latest.json.

    The run is an original schema-5 census terminal (its operands are emitted
    by the derived census) or a schema-10 terminal (its operands are its own
    retained plan/cell/test bytes; see schema10_proof_tool). A run whose
    published scorecard names it keeps the strict binding to that scorecard.
    A run whose scorecard never published (it failed, or a newer head
    superseded it) attaches unbound, under unbound_proof_binding's guards.
    ci-hub/ledger/publish_backlog.py runs this for every finalized run whose
    carrier is missing, so a carrier lost to any failure is recovered.
    """
    try:
        if preparation_error is not None:
            raise MirrorRefused(f"finalized proof preparation failed: {preparation_error}")
        if target is None:
            raise MirrorRefused("proof-only attachment requires the measured target")
        schema = None if portable is None else portable["selection"]["row"].get("schema_version")
        derived = portable is not None and portable.get("derived") is True
        if not ((derived and schema == 5) or (not derived and schema == 10)):
            raise MirrorRefused("proof-only attachment requires an original schema-5 census "
                                "or a schema-10 terminal")
        head = published_scorecard_head(writer)
        bound = head is not None and head["commit"] == target
        binding = (published_scorecard_binding(writer, snapshot, target) if bound
                   else unbound_proof_binding(writer, snapshot, portable, head))
        report["existing_scorecard"] = binding
        tool_commit = (emit_derived_portable_proof(portable, parent, results, tool_checkout,
                                                   net_wrapper=net_wrapper)
                       if derived else schema10_proof_tool(target))
        hashes = finish_portable_proof(portable, parent, writer, snapshot, tool_commit,
                                       net_wrapper=net_wrapper)
        again = (published_scorecard_binding(writer, snapshot, target) if bound
                 else unbound_proof_binding(writer, snapshot, portable, published_scorecard_head(writer)))
        if again != binding:
            raise MirrorRefused("published scorecard changed during proof-only attachment")
        paths = _new_proof_paths(_worktree_changes(writer), hashes)
        if not bound and binding["retry_of"] is not None and (paths or binding["retry_of"] not in hashes):
            raise MirrorRefused(
                f"retry of published carrier {binding['retry_of']} would add "
                + (", ".join(paths) or "a different carrier") + "; the published carrier is incomplete")
        _verify_published_snapshot(snapshot)
        commit = _commit_and_push(writer, paths, PROOF_COMMIT_MESSAGE,
                                  net_wrapper=net_wrapper, base=snapshot["source"]["commit"],
                                  verify_inputs=lambda: _verify_published_snapshot(snapshot))
        _verify_published_snapshot(snapshot)
        if commit is not None:
            report["commit"] = commit
            report["state"] = "published"
        report["portable_proof"] = {"state": "published" if commit else "unchanged",
                                    "commit": report["commit"], "files": hashes,
                                    "hermit_commit": target, "tool_hermit_commit": tool_commit}
        if not bound:
            print(f"ledger publication: FINALIZED PROOF {report['portable_proof']['state'].upper()} "
                  f"for {target} without a scorecard: latest.json and SCORECARD.md stay at "
                  f"{binding['latest_hermit_commit']}", file=sys.stderr)
    except PUBLICATION_ERRORS as error:
        report["state"] = "derived-failed"
        report["portable_proof"] = {"state": "failed", "error": str(error)}


def _release_rows(snapshot: Mapping[str, Any]) -> None:
    """Drop the published rows once nothing later in this publication reads them.

    They are every series row in the ledger, 1,130,787 on 2026-10-11 and
    about 7 GB as Python objects. The finalized proof and metadata that follow
    read only the snapshot's source, row count and row digest, and the proof
    rebuilds the canonical validation namespace while they would otherwise
    still be held (https://github.com/rrnewton/dev-hermit/issues/554). None,
    not an empty list, so a later reader fails instead of seeing no rows.
    """
    assert isinstance(snapshot, dict)
    snapshot["rows"] = None


def _publish_scorecard(report: dict[str, Any], *, parent: Path, writer: Path,
                       snapshot: Mapping[str, Any], lock_fd: int, source_checkout: Path | None,
                       target: str | None, portable: dict | None,
                       preparation_error: BaseException | None,
                       tool_preparation_error: BaseException | None,
                       prepared_tool: tuple[str, str] | None, finalized_row: bytes | None,
                       results: Path | None, scorecard_command: str | None,
                       scorecard_args: Sequence[str], tool_checkout: Path | None,
                       finalized_run_id: str | None, net_wrapper: str,
                       lifecycle_deadline_ns: int | None = None, coalesce: bool = False) -> None:
    """Regenerate the scorecard, and publish the run's carrier whatever it does.

    The carrier is a derived census (schema 5) or the run's own artifacts
    (schema 10); both are independent of the scorecard
    (https://github.com/rrnewton/dev-hermit/issues/394). Outcomes, all
    recorded in REPORT:
    - scorecard regenerated: scorecard files, latest.json and any carrier
      publish in one commit (a proof failure withholds all of them, as before);
    - scorecard failed, including a head order that cannot be decided (a
      shallow checkout, an unreadable head with no recorded time, a time
      mismatch): the carrier still publishes alone, and the state stays
      derived-failed with both facts recorded separately;
    - scorecard superseded by a newer published head: rows and any carrier
      publish, latest.json and SCORECARD.md do not move;
    - scorecard deferred, for a finalized run only, because the last
      regeneration is younger than SCORECARD_REGENERATION_INTERVAL_SECONDS:
      rows and the carrier publish, nothing renders, and latest.json and the
      scorecard files do not move; the next regeneration projects the rows;
    - scorecard deferred with reason "queued", for a finalized run whose caller
      holds publication.lock (COALESCE) while another publication waits for it
      and would render after it (queued_deferral): as above, and the pick-up
      marker names the run until a render covers its rows; if the waiter never
      renders, ci-hub/ledger/publish_backlog.py does;
    - derived census refused before the render (it is emitted first, since
      its refusal withholds the scorecard): nothing renders, rows stay
      published, and the state is derived-failed.
    """
    legacy_export = scorecard_command == "export-legacy"
    operation = "legacy_export" if legacy_export else "scorecard"
    series_commit = snapshot["source"]["commit"]
    derived = portable is not None and portable.get("derived") is True
    # Without a prepared proof or a measured identity there is nothing to
    # attach; every other refusal below still reaches the derived carrier.
    try:
        if preparation_error is not None:
            raise MirrorRefused(f"finalized proof preparation failed: {preparation_error}")
        if source_checkout is None or target is None:
            raise MirrorRefused("scorecard generation requires source checkout and target")
    except PUBLICATION_ERRORS as error:
        report["state"] = "derived-failed"
        report[operation] = {"state": "failed", "error": str(error)}
        return
    order = None
    scorecard_error = None
    if not legacy_export:
        try:
            order = scorecard_order(writer, source_checkout, target)
        except PUBLICATION_ERRORS as error:
            # An unknown order withholds the scorecard, not the proof.
            scorecard_error = MirrorRefused(f"scorecard head order cannot be decided: {error}")
    superseded = order is not None and order["state"] == "superseded"
    deferral = None
    if (scorecard_error is None and order is not None and order["state"] in {"same", "advance"}
            and finalized_run_id is not None and scorecard_command is None):
        try:
            deferral = regeneration_deferral(writer, series_commit)
        except PUBLICATION_ERRORS as error:
            # Not knowing when the scorecard was last regenerated renders it.
            print(f"ledger publication: regeneration time unknown, rendering: {error}", file=sys.stderr)
        if deferral is None and coalesce:
            deferral = queued_deferral(parent, writer, source_checkout, run_id=finalized_run_id,
                                       target=target, series_commit=series_commit)
    skipped = superseded or deferral is not None
    hashes: dict[str, str] = {}
    tool_commit = None
    proof_tool = None
    if derived and not skipped and scorecard_error is None and tool_preparation_error is None:
        # The derived census reads only the finalized row and the raw results,
        # and its refusal withholds the scorecard, so emit it before the render
        # rather than after: a run that can never publish one (a validate that
        # is not a full run; devbig030, 2026-10-03 18:10-18:40Z, an
        # rr-compat-only validate spent 30m17s rendering before this refusal)
        # skips the render.
        try:
            proof_tool = emit_derived_portable_proof(portable, parent, results, None,
                                                     net_wrapper=net_wrapper)
        except PUBLICATION_ERRORS as error:
            report["state"] = "derived-failed"
            report[operation] = {"state": "failed", "error": str(error)}
            report["portable_proof"] = {"state": "failed", "error": str(error)}
            print(f"ledger publication: SCORECARD NOT RENDERED: the derived census was refused, "
                  f"which withholds the scorecard: {error}", file=sys.stderr)
            return
    if not skipped and scorecard_error is None:
        try:
            if tool_preparation_error is not None:
                raise MirrorRefused(f"scorecard tool preparation failed: {tool_preparation_error}")
            hashes, tool_commit = build_scorecard(
                source_checkout, target, snapshot, writer, lock_fd=lock_fd,
                net_wrapper=net_wrapper, results=results,
                scorecard_command=scorecard_command, scorecard_args=scorecard_args,
                tool_checkout=tool_checkout, finalized_run_id=finalized_run_id,
                state_root=parent, prepared_finalized_row=finalized_row,
                prepared_tool=prepared_tool, lifecycle_deadline_ns=lifecycle_deadline_ns,
            )
        except PUBLICATION_ERRORS as error:
            scorecard_error = error
    _release_rows(snapshot)
    if scorecard_error is None and not skipped:
        try:
            if portable is not None:
                hashes.update(finish_portable_proof(
                    portable, parent, writer, snapshot, proof_tool if derived else tool_commit,
                    net_wrapper=net_wrapper))
            if not legacy_export:
                write_metadata(writer, snapshot, target, series_commit, hashes, tool_commit, order)
            changes = _worktree_changes(writer)
            for path in changes:
                # The renderer's scorecard/cells.json is committed as its parts.
                allowed = path in hashes or (not legacy_export and (
                    path == "latest.json" or (scorecard_cells.LOGICAL_PATH in hashes
                                              and scorecard_cells.is_storage_path(path))))
                if not allowed:
                    raise MirrorRefused(f"scorecard changed an unexpected path: {path}")
            message = ("Preserve exact legacy Hermit cell history" if legacy_export
                       else SCORECARD_COMMIT_MESSAGE)
            _verify_published_snapshot(snapshot)
            commit = _commit_and_push(writer, sorted(changes), message, net_wrapper=net_wrapper,
                                      base=series_commit,
                                      verify_inputs=lambda: _verify_published_snapshot(snapshot))
            _verify_published_snapshot(snapshot)
            if commit is not None:
                report["commit"] = commit
                report["state"] = "published"
                if not legacy_export:
                    _mark_covered_deferrals(report, parent, writer, commit)
                if (scorecard_command is None and portable is not None and finalized_row is not None
                        and finalized_run_id is not None):
                    _record_completion(parent, writer, run_id=finalized_run_id, target=target,
                                       tool_commit=tool_commit, transaction=commit,
                                       series_commit=series_commit, finalized_row=finalized_row,
                                       files=hashes)
            report[operation] = {"state": "published", "files": hashes,
                                 "hermit_commit": target, "tool_hermit_commit": tool_commit}
        except PUBLICATION_ERRORS as error:
            report["state"] = "derived-failed"
            report[operation] = {"state": "failed", "error": str(error)}
        return
    # No scorecard publishes from here on: it failed, a newer head owns it, or
    # its regeneration was deferred.
    if superseded:
        detail = (f"published scorecard head {order['current']} is newer than {target} "
                  f"({order['basis']}); latest.json and SCORECARD.md were left unchanged")
        report[operation] = {"state": "superseded", "hermit_commit": target,
                             "current_hermit_commit": order["current"], "detail": detail}
        print(f"ledger publication: SCORECARD SUPERSEDED: {detail}", file=sys.stderr)
    elif deferral is not None and deferral.get("reason") == scorecard_coalesce.REASON:
        waiter = deferral["waiter"]
        detail = (f"publication {waiter['run_id']} is queued on publication.lock and renders the whole "
                  f"published history after this one ({deferral['basis']}); this run's rows are "
                  f"published and that render projects them (pick-up marker {deferral['marker']}; "
                  f"ci-hub/ledger/publish_backlog.py renders if no publication does); latest.json "
                  f"stays at {order['current']}")
        report[operation] = {"state": "deferred", "reason": scorecard_coalesce.REASON, "hermit_commit": target,
                             "current_hermit_commit": order["current"], "detail": detail}
        print(f"ledger publication: SCORECARD DEFERRED (queued): {detail}", file=sys.stderr)
    elif deferral is not None:
        catchup = arm_catchup_render(parent, deferral)
        detail = (f"the scorecard was last regenerated {deferral['elapsed']} s ago, less than the "
                  f"{deferral['interval']} s interval; this run's rows are published and the next "
                  f"regeneration projects them ({catchup}); latest.json stays at {order['current']}")
        report[operation] = {"state": "deferred", "hermit_commit": target,
                             "current_hermit_commit": order["current"], "detail": detail,
                             "catchup_render": catchup}
        print(f"ledger publication: SCORECARD DEFERRED: {detail}", file=sys.stderr)
    else:
        report["state"] = "derived-failed"
        report[operation] = {"state": "failed", "error": str(scorecard_error)}
    if portable is None:
        return
    try:
        # The carrier is computed from the published series commit; a writer
        # that has moved (for instance a renderer that committed in it) is
        # retained and attaches nothing.
        head = _git(writer, "rev-parse", "HEAD^{commit}")
        if head != series_commit:
            raise MirrorRefused(f"ledger writer HEAD {head} is not the published series commit "
                                f"{series_commit}; no finalized proof attached; writer retained for recovery")
        # Whatever a failed renderer left behind stays unstaged and retained;
        # none of it may sit where the carrier is written.
        leftover = frozenset(_worktree_changes(writer))
        if any(path.startswith(finalized_proof.PREFIX + "/") for path in leftover):
            raise MirrorRefused("the failed scorecard left changes under the finalized proof directory: "
                                + ", ".join(sorted(path for path in leftover
                                                   if path.startswith(finalized_proof.PREFIX + "/"))))
        if not derived:
            proof_tool = schema10_proof_tool(target)
        elif proof_tool is None:
            proof_tool = emit_derived_portable_proof(portable, parent, results, None, net_wrapper=net_wrapper)
        files = finish_portable_proof(portable, parent, writer, snapshot, proof_tool,
                                      net_wrapper=net_wrapper)
        paths = _new_proof_paths(_worktree_changes(writer), files, ignore=leftover)
        message = (PROOF_SUPERSEDED_COMMIT_MESSAGE if superseded
                   else PROOF_DEFERRED_COMMIT_MESSAGE if deferral is not None
                   else PROOF_FALLBACK_COMMIT_MESSAGE)
        _verify_published_snapshot(snapshot)
        commit = _commit_and_push(writer, paths, message, net_wrapper=net_wrapper, base=series_commit,
                                  verify_inputs=lambda: _verify_published_snapshot(snapshot))
        _verify_published_snapshot(snapshot)
        if commit is not None:
            report["commit"] = commit
            if skipped:
                report["state"] = "published"
        report["portable_proof"] = {"state": "published" if commit else "unchanged",
                                    "commit": report["commit"], "files": files,
                                    "hermit_commit": target, "tool_hermit_commit": proof_tool}
        print(f"ledger publication: FINALIZED PROOF {report['portable_proof']['state'].upper()} "
              f"at {report['commit']}: {', '.join(path for path in sorted(files) if '/runs/' in path)}",
              file=sys.stderr)
        if (superseded and scorecard_command is None and finalized_row is not None
                and finalized_run_id is not None):
            # The superseded outcome is complete; record it as a published
            # scorecard's is, so the run's later callers adopt it.
            _record_superseded(parent, writer, run_id=finalized_run_id, target=target,
                               current=order["current"], tool_commit=proof_tool,
                               carrier_commit=report["commit"], series_commit=series_commit,
                               finalized_row=finalized_row, files=files)
        if deferral is not None and finalized_row is not None and finalized_run_id is not None:
            # Deferred is complete for this run too; its later callers adopt it.
            _record_deferred(parent, writer, run_id=finalized_run_id, target=target,
                             current=order["current"], tool_commit=proof_tool,
                             carrier_commit=report["commit"], series_commit=series_commit,
                             regenerated_at=deferral["regenerated_at"],
                             finalized_row=finalized_row, files=files, reason=deferral.get("reason"))
    except PUBLICATION_ERRORS as error:
        report["state"] = "derived-failed"
        report["portable_proof"] = {"state": "failed", "error": str(error)}
        if superseded:
            report[operation] = {"state": "failed", "error":
                                 f"finalized proof failed while the scorecard was superseded by "
                                 f"{order['current']}: {error}"}
        elif deferral is not None:
            report[operation] = {"state": "failed", "error":
                                 f"finalized proof failed while the scorecard regeneration was "
                                 f"deferred: {error}"}
    if not skipped:
        print(f"ledger publication: SCORECARD FAILED: {scorecard_error}; finalized proof "
              f"{report['portable_proof']['state']}", file=sys.stderr)


def _verify_lifecycle_worker(deadline_ns: int) -> None:
    """Bind the internal worker to the real inherited cleanup domain."""
    try:
        group = Path(os.environ["HERMIT_BOX_LIFECYCLE_CGROUP"])
        monitor = int(os.environ["HERMIT_BOX_LIFECYCLE_MONITOR_PID"])
        declared = int(os.environ["HERMIT_BOX_LIFECYCLE_DEADLINE_NS"])
        actual = Path("/sys/fs/cgroup" + Path("/proc/self/cgroup").read_text().strip().split("::", 1)[1])
        ancestor = Path("/sys/fs/cgroup" + Path(f"/proc/{monitor}/cgroup").read_text().strip().split("::", 1)[1])
        command = Path(f"/proc/{monitor}/cmdline").read_bytes().split(b"\0")
        expected_runner = str(TOOL_ROOT / "scripts/hermit-box-run").encode()
        if (declared != deadline_ns or group != actual or group.parent != ancestor
                or not group.name.startswith(f"hermit-box-run-{monitor}-")
                or expected_runner not in command
                or b"--inherited-deadline-ns" not in command
                or str(deadline_ns).encode() not in command
                or not (group / "cgroup.kill").is_file()):
            raise ValueError("worker does not belong to the declared live cleanup domain")
    except (KeyError, OSError, ValueError, IndexError) as error:
        raise MirrorRefused(f"unverified publication lifecycle: {error}") from error


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--parent", type=Path, required=True)
    parser.add_argument("--source-checkout", type=Path)
    parser.add_argument("--tool-checkout", type=Path, default=None,
                        help="scorecard renderer checkout; defaults to --source-checkout")
    parser.add_argument("--target")
    parser.add_argument("--results", type=Path)
    parser.add_argument("--finalized-run-id", help="canonical completed validation run; row and proof are read from STATE")
    parser.add_argument("--publish-only", action="store_true")
    parser.add_argument("--quarantine-only", action="store_true",
                        help="with --publish-only: publish only the refused series inputs, "
                             "never the series rows")
    parser.add_argument("--portable-proof-only", action="store_true",
                        help="attach newly verified public census operands without regenerating the scorecard; "
                             "a run the published scorecard names stays bound to it, and any other run must be "
                             "commit-anchored, clean and not yet carried; scorecard files, latest.json and the "
                             "original validation result are preserved")
    parser.add_argument("--coalesce-scorecard", action="store_true",
                        help="the caller holds ignored/validate/publication.lock: defer this finalized "
                             "run's render when another publication is queued on it and renders after "
                             "it (scorecard_coalesce)")
    parser.add_argument("--scorecard-command", choices=SCORECARD_COMMANDS)
    parser.add_argument("--scorecard-arg", action="append", default=[])
    parser.add_argument("--net-wrapper", default=os.environ.get("WITH_PROXY", "with-proxy"))
    parser.add_argument("--attempts", type=int, default=DEFAULT_ATTEMPTS)
    parser.add_argument("--lifecycle-deadline-ns", type=int,
                        help=f"absolute monotonic caller deadline; capped at the {LIFECYCLE_TIMEOUT_SECONDS}-second "
                             "lifecycle ceiling")
    parser.add_argument("--bounded-worker", action="store_true", help=argparse.SUPPRESS)
    return parser


def failure_report(parent: Path, error: BaseException, *, run_id: str | None,
                   count: bool) -> dict[str, Any] | None:
    """The machine-readable failure a caller records, with the quarantine count.

    A series failure already carries the quarantine outcome (it was
    published anyway), the parity outcome, and the replay of the kept
    append-parity calls as ``parity_deferred``, which the backlog's parity
    store fails on (``publish_backlog.parity_outcome``). Any earlier failure,
    before a writer was available, is reported with local counts only when
    ``count`` asks for them: a finalized run's record, or the quarantine-only
    backlog store. Otherwise None, and the failure is reported on stderr only,
    as before.
    """
    quarantine = getattr(error, "quarantine", None)
    parity = getattr(error, "parity", None)
    parity_deferred = getattr(error, "parity_deferred", None)
    if quarantine is None and count:
        try:
            quarantine = publish_quarantine(
                parent, parent, None, net_wrapper="", attempts=1, run_id=run_id,
                blocked=f"{type(error).__name__}: {error}")
        except (OSError, series.LedgerError) as count_error:
            quarantine = {"schema": "series-quarantine-publication/v1", "published": 0,
                          "already_published": 0, "commit": None,
                          "error": f"not attempted: {type(error).__name__}: {error}; "
                                   f"cannot count quarantine: {type(count_error).__name__}: {count_error}",
                          "pending_records": None, "held_records": None, "corrupt_files": None,
                          "set_aside": [], "run": None}
    if quarantine is None:
        return None
    publication = {"quarantine": quarantine}
    if parity is not None:
        publication["parity"] = parity
    if parity_deferred is not None:
        publication["parity_deferred"] = parity_deferred
    return {"state": "failed", "repository": REMOTE_URL,
            "error": f"{type(error).__name__}: {error}",
            "raw_publication": publication}


def main(argv: Sequence[str] | None = None) -> int:
    started_ns = time.monotonic_ns()
    actual_argv = list(sys.argv[1:] if argv is None else argv)
    parser = build_parser()
    args = parser.parse_args(actual_argv)
    if args.attempts <= 0 or (not args.publish_only and (args.source_checkout is None or args.target is None)):
        parser.error("positive attempts and either --publish-only or --source-checkout/--target are required")
    if args.finalized_run_id is not None and (args.results is None or args.publish_only or args.scorecard_command):
        parser.error("--finalized-run-id requires --results and ordinary scorecard publication")
    if args.portable_proof_only and (args.finalized_run_id is None or args.publish_only or args.scorecard_command):
        parser.error("--portable-proof-only requires --finalized-run-id/--results without --publish-only or --scorecard-command")
    if args.quarantine_only and not args.publish_only:
        parser.error("--quarantine-only requires --publish-only")
    if args.coalesce_scorecard and (args.finalized_run_id is None or args.portable_proof_only):
        parser.error("--coalesce-scorecard requires --finalized-run-id without --portable-proof-only")
    deadline_ns = min(started_ns + LIFECYCLE_TIMEOUT_SECONDS * 1_000_000_000,
                      args.lifecycle_deadline_ns if args.lifecycle_deadline_ns is not None
                      else started_ns + LIFECYCLE_TIMEOUT_SECONDS * 1_000_000_000)
    if deadline_ns <= time.monotonic_ns() + 2_000_000_000:
        print("ledger publication: TIMEOUT: lifecycle deadline exhausted before preparation", file=sys.stderr)
        return 124
    if not args.bounded_worker:
        # The child cgroup is below this caller, so preparation, lock waiting,
        # every held child (including setsid descendants), and cleanup retain
        # the caller's original resource accounting and one absolute deadline.
        command = [str(TOOL_ROOT / "scripts/hermit-box-run"), "--passthrough",
                   "--inherited-deadline-ns", str(deadline_ns), "--diag-fd", "2", "--",
                   sys.executable, str(Path(__file__).resolve()), *actual_argv,
                   "--lifecycle-deadline-ns", str(deadline_ns), "--bounded-worker"]
        os.execv(command[0], command)
    try:
        _verify_lifecycle_worker(deadline_ns)
        report = mirror(
            args.parent.resolve(), args.source_checkout.resolve() if args.source_checkout else None,
            args.target, submodule_remote(args.parent.resolve()), net_wrapper=args.net_wrapper,
            attempts=args.attempts, results=args.results, publish_only=args.publish_only,
            scorecard_command=args.scorecard_command, scorecard_args=args.scorecard_arg,
            tool_checkout=args.tool_checkout, finalized_run_id=args.finalized_run_id,
            lifecycle_deadline_ns=deadline_ns,
            portable_proof_only=args.portable_proof_only,
            quarantine_only=args.quarantine_only,
            coalesce_scorecard=args.coalesce_scorecard,
        )
    except (MirrorRefused, OSError, KeyError, TypeError, series.SeriesError) as error:
        print(f"ledger publication: FAILED: {error}", file=sys.stderr)
        failure = failure_report(args.parent.resolve(), error, run_id=args.finalized_run_id,
                                 count=args.finalized_run_id is not None or args.quarantine_only)
        if failure is not None:
            print(json.dumps(failure, sort_keys=True, separators=(",", ":")))
        return 1
    if args.finalized_run_id is not None and not args.portable_proof_only:
        record_publication_outcome(args.parent.resolve(), args.finalized_run_id, args.target, report)
    print(json.dumps(report, sort_keys=True, separators=(",", ":")))
    return 1 if report["state"] == "derived-failed" else 0


def record_publication_outcome(parent: Path, run_id: str, target: str, report: Mapping[str, Any]) -> Path | None:
    """Keep a finalized publication's report for the run's later callers.

    The validation payload, the unit's ExecStopPost finalization and the
    launcher each call this publisher for one finalized run. Only the first
    needs to publish; ``start_unit.finalize_ledger_mirror`` adopts what is
    recorded here instead of running the publisher again (see
    ``ci-hub/lib/ledger_publication_outcome.py`` for the conditions). Nothing
    is recorded unless this publisher's spools are the ones that module checks,
    so a run whose inputs could still be waiting elsewhere is published again
    exactly as before.
    """
    expected = ledger_publication_outcome.spool_dirs(parent)
    actual = (series.spool_dir(parent), series.parity_spool_dir(parent), series.quarantine_dir(parent),
              series.parity_deferred_dir(parent))
    if tuple(path.resolve() for path in actual) != tuple(path.resolve() for path in expected):
        print(f"ledger publication: no publication outcome recorded: the spools {', '.join(map(str, actual))} "
              f"are not the ones a later caller checks under {parent}", file=sys.stderr)
        return None
    path, why = ledger_publication_outcome.record(parent, run_id, target, report)
    if path is None:
        print(f"ledger publication: no publication outcome recorded; a later caller publishes again: {why}",
              file=sys.stderr)
        return None
    print(f"ledger publication: publication outcome {path} recorded for run {run_id}; later callers adopt it",
          file=sys.stderr)
    return path


if __name__ == "__main__":
    raise SystemExit(main())
