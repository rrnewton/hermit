"""Download and publish mergeable profile summaries through pluggable storage backends.

The planner learns estimates from accumulated profile data. A :class:`SyncBackend` can load the
current summary before a run and merge the run's contribution afterward. Each summary is scoped to
one ``(machine_id, container_class)`` identity so measurements from unlike runners remain separate.

Backends
--------
* :class:`LocalDirBackend` — a directory on the local filesystem (``local:<dir>``), with atomic
  read-merge-write under ``flock``.
* :class:`GitBranchBackend` — a dedicated git branch (``git:<url>#<branch>[#<subdir>]``), with
  retry-on-conflict for concurrent publishers.
* :class:`GitHubArtifactsBackend` — GitHub Actions artifacts (``github-artifacts:<name>[#<repo>]``).
  Downloads the latest summary artifact via the ``gh`` CLI, merges, and stages the merged summary
  for the workflow's ``upload-artifact`` step. There is no atomic read-modify-write here: two
  runners finishing concurrently each download the same "latest" and one upload wins, so a
  contribution can occasionally be dropped. Choose the git-branch backend when exact accounting
  matters.
"""

from __future__ import annotations

import json
import os
import shutil
import subprocess
import tempfile
from collections.abc import Callable, Mapping, Sequence
from pathlib import Path
from typing import Protocol, runtime_checkable

from dagrun import summary as summarylib
from dagrun.perflog import _profile_file_lock
from dagrun.summary import (
    DEFAULT_MAX_BUCKETS,
    DEFAULT_RESERVOIR_K,
    Summary,
    SummaryError,
)

__all__ = [
    "SyncError",
    "SyncBackend",
    "LocalDirBackend",
    "GitBranchBackend",
    "GitHubArtifactsBackend",
    "parse_backend",
    "summary_object_name",
]

#: Default wall-clock ceiling (seconds) for any single git/network subprocess a backend runs, so a
#: hung remote cannot wedge a run (the caller still degrades loudly on the resulting failure).
_DEFAULT_NET_TIMEOUT = 120
#: How many times the git-branch backend re-fetches + re-merges + re-pushes on a rejected push.
_DEFAULT_GIT_RETRIES = 6


class SyncError(Exception):
    """A sync backend could not complete an upload/download (network, auth, or malformed data). Raised
    LOUDLY so a caller degrades visibly rather than silently losing the feedback loop."""


def summary_object_name(machine_id: str, container_class: str) -> str:
    """The per-identity summary object name a backend reads/writes, mirroring the CSV store's
    per-machine + per-container file naming (``step_profiles_<machine>_<container>.csv``)."""
    return f"summary_{machine_id}_{container_class}.json"


@runtime_checkable
class SyncBackend(Protocol):
    """Download + upload of the mergeable summary, behind one pluggable seam.

    A backend is stateless w.r.t. the run: :meth:`download` fetches the current stored summary for an
    identity (an EMPTY summary when nothing has been published yet), and :meth:`publish` merges this
    run's ``delta`` into the stored summary and returns the merged result. How atomically
    :meth:`publish` does that read-modify-write is backend-specific (see the class docstrings)."""

    def describe(self) -> str:
        """A short human label for logs (No Silent Failure: the caller prints where it synced)."""
        ...

    def download(self, machine_id: str, container_class: str) -> Summary:
        """The current stored summary for ``(machine_id, container_class)``, or an EMPTY summary of
        that identity when none exists. Raises :class:`SyncError` on a real failure (never silently
        returns empty to hide a broken backend)."""
        ...

    def publish(
        self,
        delta: Summary,
        *,
        reservoir_cap: int = DEFAULT_RESERVOIR_K,
        max_buckets: int = DEFAULT_MAX_BUCKETS,
    ) -> Summary:
        """Merge ``delta`` into the stored summary of its identity and return the merged summary.
        Raises :class:`SyncError` on failure."""
        ...


# --------------------------------------------------------------------------- local directory


class LocalDirBackend:
    """Store the summary as a file in a local directory (``local:<dir>``).

    ``publish`` is an atomic read-merge-write under the same persistent ``flock`` sidecar protocol
    as the profile store, so concurrent compatible runs never lose a contribution."""

    def __init__(self, root: str | Path) -> None:
        self.root = Path(root)

    def describe(self) -> str:
        """Return the local-directory backend label used in logs."""
        return f"local:{self.root}"

    def _path(self, machine_id: str, container_class: str) -> Path:
        return self.root / summary_object_name(machine_id, container_class)

    def download(self, machine_id: str, container_class: str) -> Summary:
        """Load one identity's summary, or return an empty summary when absent."""
        path = self._path(machine_id, container_class)
        if not path.is_file():
            return summarylib.empty(machine_id, container_class)
        try:
            return summarylib.from_json(path.read_text(encoding="utf-8"))
        except (OSError, SummaryError) as exc:
            raise SyncError(f"local backend: cannot read {path}: {exc}") from exc

    def publish(
        self,
        delta: Summary,
        *,
        reservoir_cap: int = DEFAULT_RESERVOIR_K,
        max_buckets: int = DEFAULT_MAX_BUCKETS,
    ) -> Summary:
        """Atomically merge and persist ``delta`` under an exclusive file lock."""
        try:
            self.root.mkdir(parents=True, exist_ok=True)
        except OSError as exc:
            raise SyncError(f"local backend: cannot create {self.root}: {exc}") from exc
        path = self._path(delta.machine_id, delta.container_class)
        with _profile_file_lock(path):
            base = self.download(delta.machine_id, delta.container_class)
            merged = summarylib.merge(
                base, delta, reservoir_cap=reservoir_cap, max_buckets=max_buckets
            )
            tmp = path.with_suffix(path.suffix + ".tmp")
            tmp.write_text(summarylib.to_json(merged), encoding="utf-8")
            os.replace(tmp, path)
        return merged


# --------------------------------------------------------------------------- git branch (atomic)


def _run_git(
    args: Sequence[str], cwd: str | Path, *, timeout: int, check: bool
) -> subprocess.CompletedProcess[str]:
    """Run a git command with an explicit timeout, capturing text output. When ``check`` is set a
    non-zero exit becomes a :class:`SyncError` carrying git's stderr (never a bare CalledProcess)."""
    try:
        proc = subprocess.run(
            ["git", *args],
            cwd=str(cwd),
            capture_output=True,
            text=True,
            timeout=timeout,
            check=False,
        )
    except (OSError, subprocess.TimeoutExpired) as exc:
        raise SyncError(f"git {' '.join(args)} failed to run: {exc}") from exc
    if check and proc.returncode != 0:
        raise SyncError(f"git {' '.join(args)} exited {proc.returncode}: {proc.stderr.strip()}")
    return proc


class GitBranchBackend:
    """Store the summary atomically on a dedicated git branch.

    The specification format is ``git:<url>#<branch>[#<subdir>]``.

    ``publish`` does a correct concurrent read-modify-write: it works in a private throwaway checkout
    per attempt, fetches the branch, merges the remote summary with this run's ``delta``, commits,
    and pushes. If the push is REJECTED (a concurrent contributor advanced the branch), it retries
    from a fresh fetch of the NEW tip and re-merges the SAME delta — so a concurrent contribution is
    never clobbered and this run's delta is never double-counted. The delta is fixed across retries;
    only the remote base changes."""

    def __init__(
        self,
        url: str,
        branch: str,
        subdir: str = ".",
        *,
        timeout: int = _DEFAULT_NET_TIMEOUT,
        retries: int = _DEFAULT_GIT_RETRIES,
        before_push: Callable[[int], None] | None = None,
    ) -> None:
        self.url = url
        self.branch = branch
        self.subdir = subdir.strip("/") or "."
        self.timeout = timeout
        self.retries = retries
        # Test-only hook invoked with the attempt index immediately BEFORE each push, so a test can
        # inject a conflicting push to exercise the retry-on-conflict path deterministically.
        self._before_push = before_push

    def describe(self) -> str:
        """Return the remote branch and optional subdirectory used by this backend."""
        loc = self.branch if self.subdir == "." else f"{self.branch}#{self.subdir}"
        return f"git-branch:{self.url}#{loc}"

    def _rel(self, machine_id: str, container_class: str) -> str:
        name = summary_object_name(machine_id, container_class)
        return name if self.subdir == "." else f"{self.subdir}/{name}"

    def _fresh_checkout(self) -> str:
        """A private throwaway git repo wired to the remote (empty working tree). Caller removes it."""
        work = tempfile.mkdtemp(prefix="dagrun-sync-git-")
        _run_git(["init", "-q"], work, timeout=self.timeout, check=True)
        _run_git(["remote", "add", "origin", self.url], work, timeout=self.timeout, check=True)
        _run_git(
            ["config", "user.email", "dagrun@example.invalid"],
            work,
            timeout=self.timeout,
            check=True,
        )
        _run_git(
            ["config", "user.name", "dagrun"], work, timeout=self.timeout, check=True
        )
        return work

    def _fetch_branch(self, work: str) -> bool:
        """Fetch the data branch into FETCH_HEAD; ``True`` if it exists on the remote, else ``False``
        (a not-yet-created branch is not an error — the summary simply starts empty)."""
        proc = _run_git(
            ["fetch", "-q", "--depth", "1", "origin", self.branch],
            work,
            timeout=self.timeout,
            check=False,
        )
        return proc.returncode == 0

    def download(self, machine_id: str, container_class: str) -> Summary:
        """Fetch and decode one identity's summary from the configured branch."""
        work = self._fresh_checkout()
        try:
            if not self._fetch_branch(work):
                return summarylib.empty(machine_id, container_class)
            rel = self._rel(machine_id, container_class)
            proc = _run_git(
                ["show", f"FETCH_HEAD:{rel}"], work, timeout=self.timeout, check=False
            )
            if proc.returncode != 0:
                return summarylib.empty(machine_id, container_class)
            try:
                return summarylib.from_json(proc.stdout)
            except SummaryError as exc:
                raise SyncError(f"git backend: malformed summary on {self.branch}: {exc}") from exc
        finally:
            shutil.rmtree(work, ignore_errors=True)

    def publish(
        self,
        delta: Summary,
        *,
        reservoir_cap: int = DEFAULT_RESERVOIR_K,
        max_buckets: int = DEFAULT_MAX_BUCKETS,
    ) -> Summary:
        """Merge and push ``delta``, retrying when another publisher advances the branch."""
        rel = self._rel(delta.machine_id, delta.container_class)
        last_err = ""
        for attempt in range(self.retries):
            work = self._fresh_checkout()
            try:
                had = self._fetch_branch(work)
                if had:
                    _run_git(
                        ["checkout", "-q", "-f", "FETCH_HEAD"],
                        work,
                        timeout=self.timeout,
                        check=True,
                    )
                    base = self.download(delta.machine_id, delta.container_class)
                else:
                    base = summarylib.empty(delta.machine_id, delta.container_class)
                merged = summarylib.merge(
                    base, delta, reservoir_cap=reservoir_cap, max_buckets=max_buckets
                )
                target = Path(work) / rel
                target.parent.mkdir(parents=True, exist_ok=True)
                target.write_text(summarylib.to_json(merged), encoding="utf-8")
                _run_git(["add", rel], work, timeout=self.timeout, check=True)
                _run_git(
                    [
                        "commit",
                        "-q",
                        "-m",
                        f"profile-summary: {delta.machine_id}/{delta.container_class}",
                    ],
                    work,
                    timeout=self.timeout,
                    check=True,
                )
                if self._before_push is not None:
                    self._before_push(attempt)
                push = _run_git(
                    ["push", "origin", f"HEAD:{self.branch}"],
                    work,
                    timeout=self.timeout,
                    check=False,
                )
                if push.returncode == 0:
                    return merged
                last_err = push.stderr.strip()
            finally:
                shutil.rmtree(work, ignore_errors=True)
        raise SyncError(
            f"git backend: push to {self.branch} rejected after {self.retries} attempts "
            f"(last: {last_err})"
        )


# --------------------------------------------------------------------------- github actions artifacts


def _select_latest_artifact(
    artifacts: Sequence[Mapping[str, object]], name: str
) -> Mapping[str, object] | None:
    """Pick the most-recently-created, non-expired artifact whose ``name`` matches ``name`` from a
    GitHub artifacts-API listing. Pure (no I/O) so it is unit-testable without a live runner. Returns
    ``None`` when no live artifact matches."""
    best: Mapping[str, object] | None = None
    best_created = ""
    for art in artifacts:
        if art.get("name") != name:
            continue
        if bool(art.get("expired", False)):
            continue
        created = art.get("created_at")
        created_s = created if isinstance(created, str) else ""
        if best is None or created_s > best_created:
            best, best_created = art, created_s
    return best


class GitHubArtifactsBackend:
    """Store the summary as a GitHub Actions artifact (``github-artifacts:<name>[#<repo>]``).

    Downloads the latest summary artifact (via the ``gh`` CLI + the artifacts REST API) and merges
    it; ``publish`` writes the merged summary to a local staging file (:attr:`staging_dir`) for the
    workflow's ``actions/upload-artifact`` step to upload — GitHub Actions has no in-run artifact
    write API, so the actual upload is the workflow YAML's job, not this backend's.

    NON-ATOMIC by design: two runners finishing concurrently each download the same "latest" and one
    upload wins, so a contribution can occasionally be dropped. Acceptable for a statistical summary
    (the next run re-contributes); choose :class:`GitBranchBackend` when exact accounting matters."""

    def __init__(
        self,
        name: str,
        repo: str | None = None,
        *,
        staging_dir: str | Path | None = None,
        timeout: int = _DEFAULT_NET_TIMEOUT,
    ) -> None:
        self.name = name
        self.repo = repo or os.environ.get("GITHUB_REPOSITORY")
        self.staging_dir = Path(staging_dir) if staging_dir is not None else Path(".")
        self.timeout = timeout

    def describe(self) -> str:
        """Return the artifact name and optional repository used by this backend."""
        return f"github-artifacts:{self.name}" + (f"#{self.repo}" if self.repo else "")

    def staging_path(self, machine_id: str, container_class: str) -> Path:
        """Return the local upload-staging path for one identity's summary."""
        return self.staging_dir / summary_object_name(machine_id, container_class)

    def _gh(self, args: Sequence[str]) -> subprocess.CompletedProcess[str]:
        try:
            return subprocess.run(
                ["gh", *args],
                capture_output=True,
                text=True,
                timeout=self.timeout,
                check=False,
            )
        except (OSError, subprocess.TimeoutExpired) as exc:
            raise SyncError(f"github-artifacts backend: `gh {' '.join(args)}` failed: {exc}") from exc

    def _gh_download(self, api_path: str, dest: Path) -> None:
        """Fetch a BINARY GitHub API response (an artifact zip, which comes as a redirect to raw
        bytes) straight to ``dest`` — captured as bytes, not text, so the zip is not corrupted."""
        try:
            with open(dest, "wb") as out:
                proc = subprocess.run(
                    ["gh", "api", api_path],
                    stdout=out,
                    stderr=subprocess.PIPE,
                    timeout=self.timeout,
                    check=False,
                )
        except (OSError, subprocess.TimeoutExpired) as exc:
            raise SyncError(f"github-artifacts backend: `gh api {api_path}` failed: {exc}") from exc
        if proc.returncode != 0:
            raise SyncError(
                f"github-artifacts backend: downloading {api_path} failed: "
                f"{proc.stderr.decode('utf-8', 'replace').strip()}"
            )

    def _require_repo(self) -> str:
        if not self.repo:
            raise SyncError(
                "github-artifacts backend: no repo — pass github-artifacts:<name>#<owner/repo> "
                "or set $GITHUB_REPOSITORY"
            )
        return self.repo

    def download(self, machine_id: str, container_class: str) -> Summary:
        """Download the newest live matching artifact for one runner identity."""
        repo = self._require_repo()
        listing = self._gh(["api", f"repos/{repo}/actions/artifacts", "--paginate"])
        if listing.returncode != 0:
            raise SyncError(
                f"github-artifacts backend: listing artifacts failed: {listing.stderr.strip()}"
            )
        try:
            payload: object = json.loads(listing.stdout)
        except json.JSONDecodeError as exc:
            raise SyncError(f"github-artifacts backend: bad artifacts JSON: {exc}") from exc
        artifacts = payload.get("artifacts") if isinstance(payload, dict) else None
        if not isinstance(artifacts, list):
            return summarylib.empty(machine_id, container_class)
        typed = [a for a in artifacts if isinstance(a, dict)]
        latest = _select_latest_artifact(typed, self.name)
        if latest is None:
            return summarylib.empty(machine_id, container_class)
        artifact_id = latest.get("id")
        if not isinstance(artifact_id, int):
            return summarylib.empty(machine_id, container_class)
        with tempfile.TemporaryDirectory(prefix="dagrun-sync-gha-") as tmp:
            zip_path = Path(tmp) / "artifact.zip"
            self._gh_download(f"repos/{repo}/actions/artifacts/{artifact_id}/zip", zip_path)
            try:
                shutil.unpack_archive(str(zip_path), tmp, "zip")
            except (OSError, shutil.ReadError) as exc:
                raise SyncError(f"github-artifacts backend: unzip failed: {exc}") from exc
            member = Path(tmp) / summary_object_name(machine_id, container_class)
            if not member.is_file():
                return summarylib.empty(machine_id, container_class)
            try:
                return summarylib.from_json(member.read_text(encoding="utf-8"))
            except SummaryError as exc:
                raise SyncError(f"github-artifacts backend: malformed summary: {exc}") from exc

    def publish(
        self,
        delta: Summary,
        *,
        reservoir_cap: int = DEFAULT_RESERVOIR_K,
        max_buckets: int = DEFAULT_MAX_BUCKETS,
    ) -> Summary:
        """Merge ``delta`` and write the result to the workflow upload-staging directory."""
        base = self.download(delta.machine_id, delta.container_class)
        merged = summarylib.merge(base, delta, reservoir_cap=reservoir_cap, max_buckets=max_buckets)
        path = self.staging_path(delta.machine_id, delta.container_class)
        try:
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(summarylib.to_json(merged), encoding="utf-8")
        except OSError as exc:
            raise SyncError(f"github-artifacts backend: staging write failed: {exc}") from exc
        return merged


# --------------------------------------------------------------------------- spec parsing


def parse_backend(spec: str) -> SyncBackend:
    """Construct a :class:`SyncBackend` from a ``--profile-sync`` spec string.

    Grammar (scheme prefix picks the backend)::

        local:<dir>                        # LocalDirBackend
        git:<url>#<branch>[#<subdir>]      # GitBranchBackend (atomic RMW)
        github-artifacts:<name>[#<repo>]   # GitHubArtifactsBackend (non-atomic)

    Raises :class:`SyncError` on an unknown scheme or malformed specification."""
    scheme, sep, rest = spec.partition(":")
    if not sep:
        raise SyncError(
            f"invalid --profile-sync spec {spec!r}: expected '<scheme>:<...>' "
            "(local:, git:, github-artifacts:)"
        )
    if scheme == "local":
        if not rest:
            raise SyncError("invalid local spec: expected 'local:<dir>'")
        return LocalDirBackend(rest)
    if scheme == "git":
        parts = rest.split("#")
        if len(parts) < 2 or not parts[0] or not parts[1]:
            raise SyncError("invalid git spec: expected 'git:<url>#<branch>[#<subdir>]'")
        url, branch = parts[0], parts[1]
        subdir = parts[2] if len(parts) >= 3 and parts[2] else "."
        return GitBranchBackend(url, branch, subdir)
    if scheme == "github-artifacts":
        name, _, repo = rest.partition("#")
        if not name:
            raise SyncError(
                "invalid github-artifacts spec: expected 'github-artifacts:<name>[#<owner/repo>]'"
            )
        return GitHubArtifactsBackend(name, repo or None)
    raise SyncError(
        f"unknown --profile-sync scheme {scheme!r}: supported schemes are "
        "local:, git:, github-artifacts:"
    )
