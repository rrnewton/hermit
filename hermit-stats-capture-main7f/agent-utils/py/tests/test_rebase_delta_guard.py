"""Brackets for ``scripts/rebase-delta-guard``.

The guard exists because a conflict-free ``git rebase`` can silently rewrite a
file: an untracked ``$GIT_DIR/info/attributes`` binding sends the file through a
re-serializing merge driver, the rebase reports success, ``git status`` stays
clean, and the delta is no longer the one you wrote.

Every case here is **planted**. A check that has only been seen not to misbehave
is not evidence, and a guard that fails on everything looks vigilant while being
worthless -- so the positive cases are asserted just as hard as the negative
ones. Both were live risks: the first bracket run of this guard's predecessor
reported the *positive* case failing, because the test recovered a pre-rebase
SHA by searching the log instead of recording it.

Every SHA below is captured at the moment the commit is created, never
recovered afterwards by searching. That is the habit the tool encodes.
"""

from __future__ import annotations

import json
import os
import shlex
import shutil
import subprocess
from pathlib import Path
from typing import NamedTuple

import pytest

GUARD = Path(__file__).resolve().parents[2] / "scripts" / "rebase-delta-guard"

# A merge driver representative of the class: it performs a real, correct 3-way
# merge and then rewrites the whole file. Faithful to the observed incident,
# where the intended change survived intact and rode along with a whole-file
# re-serialization. Deliberately dependency-free so the bracket cannot be
# skipped by a missing library.
RESERIALIZING_DRIVER = '''\
import json, subprocess, sys
base, ours, theirs = sys.argv[1], sys.argv[2], sys.argv[3]
rc = subprocess.call(["git", "merge-file", ours, base, theirs])
text = open(ours).read()
try:
    obj = json.loads(text)
except Exception:
    # Not JSON: double every line's leading indentation. Semantically inert for
    # the test's purposes, and it rewrites nearly every line.
    out = []
    for line in text.splitlines(True):
        stripped = line.lstrip(" ")
        out.append(" " * (2 * (len(line) - len(stripped))) + stripped)
    text = "".join(out)
else:
    text = json.dumps(obj, indent=4, sort_keys=True) + "\\n"
open(ours, "w").write(text)
sys.exit(rc)
'''


def git(repo: Path, *args: str, check: bool = True) -> str:
    result = subprocess.run(
        ["git", "-C", str(repo), *args],
        capture_output=True,
        text=True,
        check=False,
    )
    if check and result.returncode != 0:
        raise AssertionError(f"git {' '.join(args)} failed: {result.stderr}")
    return result.stdout.strip()


def guard(
    repo: Path, *args: str, env: dict[str, str] | None = None
) -> subprocess.CompletedProcess[str]:
    # Captured rather than piped. Piping this into `head` under `pipefail`
    # yields rc=141 from SIGPIPE, which reads as a crash in the tool.
    run_env = dict(os.environ)
    if env is not None:
        run_env.update(env)
    return subprocess.run(
        [str(GUARD), "--repo", str(repo), *args],
        capture_output=True,
        text=True,
        check=False,
        env=run_env,
    )


def state_files(repo: Path) -> tuple[Path, Path]:
    state = Path(git(repo, "rev-parse", "--absolute-git-dir")) / "rebase-delta-guard.state"
    return state, Path(f"{state}.subjects")


def tool_path(tmp_path: Path, name: str, body: str) -> tuple[Path, dict[str, str]]:
    """Plant one PATH-preferred executable and return its environment."""
    bin_dir = tmp_path / f"fake-{name}-bin"
    bin_dir.mkdir()
    executable = bin_dir / name
    executable.write_text("#!/usr/bin/env bash\nset -eu\n" + body)
    executable.chmod(0o755)
    return executable, {"PATH": f"{bin_dir}{os.pathsep}{os.environ['PATH']}"}


class Fixture(NamedTuple):
    """The three commits a rebase bracket needs, captured when each is created.

    Typed rather than a dict so a caller cannot quietly pass the wrong field, and
    so the SHAs travel as the strings they are.
    """

    repo: Path
    old_base: str
    old_head: str
    new_base: str


@pytest.fixture()
def fixture_repo(tmp_path: Path) -> Fixture:
    """A repository whose feature branch and main both touch one JSON file.

    Both sides touching the same file is what forces git to run a per-file
    3-way merge during the rebase, which is what gives a merge driver its
    opening. Without that, the driver never fires and the bracket would be
    inert.
    """
    repo = tmp_path / "repo"
    repo.mkdir()
    git(repo, "init", "-q", "-b", "main")
    git(repo, "config", "user.email", "guard@test")
    git(repo, "config", "user.name", "guard test")

    payload = {f"key_{i:03d}": {"nested": [i, i * 2], "name": f"item {i}"} for i in range(60)}
    target = repo / "registry.json"
    target.write_text(json.dumps(payload, indent=2) + "\n")
    git(repo, "add", "-A")
    git(repo, "commit", "-q", "-m", "base")
    old_base = git(repo, "rev-parse", "HEAD")  # recorded, not searched

    git(repo, "checkout", "-q", "-b", "feature")
    text = target.read_text().replace('"name": "item 7"', '"name": "item 7 PLANTED"', 1)
    target.write_text(text)
    git(repo, "commit", "-q", "-am", "feature: one field")
    old_head = git(repo, "rev-parse", "HEAD")  # recorded, not searched

    git(repo, "checkout", "-q", "main")
    text = target.read_text().replace('"name": "item 41"', '"name": "item 41 UNRELATED"', 1)
    target.write_text(text)
    git(repo, "commit", "-q", "-am", "main: an unrelated field")
    new_base = git(repo, "rev-parse", "HEAD")  # recorded, not searched
    git(repo, "branch", "--set-upstream-to=main", "feature")

    return Fixture(repo=repo, old_base=old_base, old_head=old_head, new_base=new_base)


def install_driver(repo: Path, tmp_path: Path, pattern: str = "registry.json") -> None:
    driver = tmp_path / "reserialize.py"
    driver.write_text(RESERIALIZING_DRIVER)
    git_dir = Path(git(repo, "rev-parse", "--absolute-git-dir"))
    (git_dir / "info").mkdir(exist_ok=True)
    (git_dir / "info" / "attributes").write_text(f"{pattern} merge=reserialize\n")
    git(repo, "config", "merge.reserialize.driver", f"python3 {driver} %O %A %B")


def rebase(repo: Path, onto: str, *flags: str) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        ["git", "-C", str(repo), "rebase", *flags, onto],
        capture_output=True,
        text=True,
        check=False,
    )


def init_repo(repo: Path) -> None:
    repo.mkdir()
    git(repo, "init", "-q", "-b", "main")
    git(repo, "config", "user.email", "guard@test")
    git(repo, "config", "user.name", "guard test")


def set_gitlink(repo: Path, path: str, commit: str) -> None:
    git(repo, "update-index", "--add", "--cacheinfo", f"160000,{commit},{path}")


def stopped_rebase_with_file_conflict(tmp_path: Path) -> Path:
    repo = tmp_path / "ordinary-conflict"
    init_repo(repo)
    target = repo / "shared.txt"
    target.write_text("base\n")
    git(repo, "add", "shared.txt")
    git(repo, "commit", "-q", "-m", "base")

    git(repo, "checkout", "-q", "-b", "feature")
    target.write_text("feature\n")
    git(repo, "commit", "-q", "-am", "feature")
    git(repo, "checkout", "-q", "main")
    target.write_text("main\n")
    git(repo, "commit", "-q", "-am", "main")
    git(repo, "checkout", "-q", "feature")
    assert rebase(repo, "main").returncode != 0
    return repo


def stopped_rebase_with_file_and_gitlink_conflicts(tmp_path: Path) -> Path:
    child = tmp_path / "gitlink-source"
    init_repo(child)
    payload = child / "payload"
    payload.write_text("base\n")
    git(child, "add", "payload")
    git(child, "commit", "-q", "-m", "child base")
    child_base = git(child, "rev-parse", "HEAD")
    git(child, "checkout", "-q", "-b", "child-feature")
    payload.write_text("feature\n")
    git(child, "commit", "-q", "-am", "child feature")
    child_feature = git(child, "rev-parse", "HEAD")
    git(child, "checkout", "-q", "main")
    payload.write_text("main\n")
    git(child, "commit", "-q", "-am", "child main")
    child_main = git(child, "rev-parse", "HEAD")

    repo = tmp_path / "mixed-conflict"
    init_repo(repo)
    target = repo / "shared.txt"
    target.write_text("base\n")
    git(repo, "add", "shared.txt")
    set_gitlink(repo, "agent-utils", child_base)
    git(repo, "commit", "-q", "-m", "base")

    git(repo, "checkout", "-q", "-b", "feature")
    target.write_text("feature\n")
    git(repo, "add", "shared.txt")
    set_gitlink(repo, "agent-utils", child_feature)
    git(repo, "commit", "-q", "-m", "feature")
    git(repo, "checkout", "-q", "main")
    target.write_text("main\n")
    git(repo, "add", "shared.txt")
    set_gitlink(repo, "agent-utils", child_main)
    git(repo, "commit", "-q", "-m", "main")

    git(repo, "checkout", "-q", "feature")
    assert rebase(repo, "main").returncode != 0
    # Reproduce the dangerous shape: a gitlink is a directory on disk, so a
    # file-oriented tool handed this path may recurse into unrelated contents.
    (repo / "agent-utils").mkdir(exist_ok=True)
    (repo / "agent-utils" / "nested.txt").write_text("must not be inspected\n")
    return repo


def test_conflict_files_emits_an_ordinary_conflict(tmp_path: Path) -> None:
    repo = stopped_rebase_with_file_conflict(tmp_path)

    result = guard(repo, "conflict-files")

    assert result.returncode == 0, result.stderr
    assert result.stdout == "shared.txt\n"
    assert result.stderr == ""


def test_help_surfaces_safe_conflict_iteration(tmp_path: Path) -> None:
    repo = stopped_rebase_with_file_conflict(tmp_path)

    result = guard(repo, "--help")

    assert result.returncode == 0, result.stderr
    assert "rebase-delta-guard conflict-files [-z|--null]" in result.stdout
    assert "unsafe input to grep, sed" in result.stdout


def test_conflict_files_skips_gitlink_without_mutating_stopped_rebase(
    tmp_path: Path,
) -> None:
    repo = stopped_rebase_with_file_and_gitlink_conflicts(tmp_path)
    assert set(git(repo, "diff", "--name-only", "--diff-filter=U").splitlines()) == {
        "agent-utils",
        "shared.txt",
    }
    assert (repo / "agent-utils").is_dir()
    before_status = git(repo, "status", "--porcelain=v2")
    before_head = git(repo, "rev-parse", "HEAD")

    result = guard(repo, "conflict-files")

    assert result.returncode == 0, result.stderr
    assert result.stdout == "shared.txt\n"
    assert "skipped conflicted gitlink agent-utils" in result.stderr
    assert "mode 160000" in result.stderr
    assert all(not (repo / path).is_dir() for path in result.stdout.splitlines())
    assert git(repo, "status", "--porcelain=v2") == before_status
    assert git(repo, "rev-parse", "HEAD") == before_head

    nul = guard(repo, "conflict-files", "--null")
    assert nul.returncode == 0, nul.stderr
    assert nul.stdout == "shared.txt\0"


def test_positive_clean_rebase_passes(fixture_repo: Fixture, tmp_path: Path) -> None:
    """The guard must PASS an honest rebase. A guard that never passes is worthless."""
    repo = fixture_repo.repo
    git(repo, "checkout", "-q", "feature")

    recorded = guard(repo, "record", "--base", fixture_repo.old_base)
    assert recorded.returncode == 0, recorded.stderr

    assert rebase(repo, fixture_repo.new_base).returncode == 0

    verified = guard(repo, "verify")
    assert verified.returncode == 0, f"{verified.stdout}\n{verified.stderr}"
    assert "OK" in verified.stdout


def test_negative_reserializing_driver_is_caught(
    fixture_repo: Fixture, tmp_path: Path
) -> None:
    """The planted driver rewrites the file; the guard must refuse, loudly."""
    repo = fixture_repo.repo
    git(repo, "checkout", "-q", "feature")
    install_driver(repo, tmp_path)

    recorded = guard(repo, "record", "--base", fixture_repo.old_base)
    assert recorded.returncode == 0, recorded.stderr

    result = rebase(repo, fixture_repo.new_base)
    # The whole point: the rebase itself reports success and leaves a clean tree.
    assert result.returncode == 0, result.stderr
    assert git(repo, "status", "--porcelain") == ""

    verified = guard(repo, "verify")
    assert verified.returncode == 1, f"guard did not catch the rewrite: {verified.stdout}"
    assert "FAIL" in verified.stdout


def test_remedy_apply_backend_preserves_bytes(
    fixture_repo: Fixture, tmp_path: Path
) -> None:
    """With the driver STILL bound, ``rebase --apply`` must preserve the delta.

    This is the documented workaround, so it is bracketed rather than asserted
    in prose.
    """
    repo = fixture_repo.repo
    git(repo, "checkout", "-q", "feature")
    install_driver(repo, tmp_path)

    assert guard(repo, "record", "--base", fixture_repo.old_base).returncode == 0
    assert rebase(repo, fixture_repo.new_base, "--apply").returncode == 0
    assert git(repo, "check-attr", "merge", "--", "registry.json").endswith("reserialize")

    verified = guard(repo, "verify")
    assert verified.returncode == 0, f"{verified.stdout}\n{verified.stderr}"


def test_missing_recording_is_indeterminate_not_pass(fixture_repo: Fixture) -> None:
    """No recording must not read as success. Fail-closed on absence of evidence."""
    repo = fixture_repo.repo
    git(repo, "checkout", "-q", "feature")
    verified = guard(repo, "verify")
    assert verified.returncode == 3
    assert "INDETERMINATE" in verified.stderr


def test_record_without_upstream_requires_an_explicit_onto(fixture_repo: Fixture) -> None:
    repo = fixture_repo.repo
    git(repo, "checkout", "-q", "feature")
    git(repo, "branch", "--unset-upstream")

    recorded = guard(repo, "record", "--base", fixture_repo.old_base)

    assert recorded.returncode == 2
    assert "--onto" in recorded.stderr


def test_missing_subject_identity_is_indeterminate(fixture_repo: Fixture) -> None:
    """The integrity sidecar is part of the record, never an optional enhancement."""
    repo = fixture_repo.repo
    git(repo, "checkout", "-q", "feature")
    assert guard(repo, "record", "--base", fixture_repo.old_base).returncode == 0
    state, subjects = state_files(repo)
    subjects.unlink()

    verified = guard(repo, "verify")

    assert verified.returncode == 3
    assert "subjects are missing" in verified.stderr
    assert state.exists(), "an indeterminate verification must retain its record for retry"


def test_malformed_state_is_indeterminate(fixture_repo: Fixture) -> None:
    """Extra, duplicate, reordered, or malformed fields cannot be silently accepted."""
    repo = fixture_repo.repo
    git(repo, "checkout", "-q", "feature")
    assert guard(repo, "record", "--base", fixture_repo.old_base).returncode == 0
    state, _subjects = state_files(repo)
    state.write_text(
        f"old_base={fixture_repo.old_base}\n"
        f"old_head={fixture_repo.old_head}\n"
        "series=1\n"
        f"onto={fixture_repo.new_base}\n"
        "unexpected=accepted-by-source\n"
    )

    verified = guard(repo, "verify")

    assert verified.returncode == 3
    assert "exactly old_base, old_head, series, and onto" in verified.stderr


def test_state_command_injection_is_data_not_code(
    fixture_repo: Fixture, tmp_path: Path
) -> None:
    """A planted command in an otherwise valid record must never execute."""
    repo = fixture_repo.repo
    git(repo, "checkout", "-q", "feature")
    assert guard(repo, "record", "--base", fixture_repo.old_base).returncode == 0
    state, _subjects = state_files(repo)
    marker = tmp_path / "STATE-WAS-EXECUTED"
    state.write_text(
        f"old_base={fixture_repo.old_base}\n"
        f"old_head={fixture_repo.old_head}\n"
        "series=1\n"
        f"touch {marker}\n"
    )

    verified = guard(repo, "verify")

    assert verified.returncode == 3
    assert not marker.exists(), "verify sourced and executed attacker-controlled state"


def test_state_and_subjects_must_be_real_regular_files(
    fixture_repo: Fixture, tmp_path: Path
) -> None:
    repo = fixture_repo.repo
    git(repo, "checkout", "-q", "feature")
    assert guard(repo, "record", "--base", fixture_repo.old_base).returncode == 0
    state, subjects = state_files(repo)
    saved_state = tmp_path / "saved-state"
    state.replace(saved_state)
    state.symlink_to(saved_state)

    verified = guard(repo, "verify")
    assert verified.returncode == 3
    assert "not a regular file" in verified.stderr

    state.unlink()
    saved_state.replace(state)
    saved_subjects = tmp_path / "saved-subjects"
    subjects.replace(saved_subjects)
    subjects.symlink_to(saved_subjects)
    verified = guard(repo, "verify")
    assert verified.returncode == 3
    assert "not a regular file" in verified.stderr


def test_malformed_subject_record_is_indeterminate(fixture_repo: Fixture) -> None:
    repo = fixture_repo.repo
    git(repo, "checkout", "-q", "feature")
    assert guard(repo, "record", "--base", fixture_repo.old_base).returncode == 0
    _state, subjects = state_files(repo)
    # Preserve the expected line count: strictness covers the record/sidecar pairing, not shape.
    subjects.write_text("forged but correctly counted subject\n")

    verified = guard(repo, "verify", "--new-base", fixture_repo.old_base)

    assert verified.returncode == 3
    assert "subjects sidecar does not match" in verified.stderr


def test_explicit_new_base_cannot_override_the_recorded_onto(
    fixture_repo: Fixture,
) -> None:
    repo = fixture_repo.repo
    git(repo, "checkout", "-q", "feature")
    assert guard(repo, "record", "--base", fixture_repo.old_base).returncode == 0

    verified = guard(repo, "verify", "--new-base", fixture_repo.old_base)

    assert verified.returncode == 3
    assert "contradicts recorded onto" in verified.stderr


def test_dropped_commit_is_compared_against_the_exact_recorded_onto(
    fixture_repo: Fixture,
) -> None:
    """A count is not an identity; the exact recorded onto commit is.

    Record a two-commit series, drop one, then rebase onto a base that is itself
    one commit further along. Count/subject derivation can name the wrong base;
    using the recorded onto SHA compares the real post-rebase delta and reports
    that one feature commit was dropped.
    """
    repo = fixture_repo.repo
    git(repo, "checkout", "-q", "feature")
    (repo / "extra.txt").write_text("extra\n")
    git(repo, "add", "-A")
    git(repo, "commit", "-q", "-m", "feature: second commit")
    assert guard(repo, "record", "--base", fixture_repo.old_base).returncode == 0

    git(repo, "reset", "-q", "--hard", "HEAD~1")
    assert rebase(repo, fixture_repo.new_base).returncode == 0

    verified = guard(repo, "verify")
    assert verified.returncode == 1
    assert "FAIL" in verified.stdout


def test_cherry_picked_upstream_subject_cannot_manufacture_a_false_pass(
    tmp_path: Path,
) -> None:
    """An upstream cherry-pick can preserve both patch and subject but not identity."""
    repo = tmp_path / "cherry-pick-repo"
    repo.mkdir()
    git(repo, "init", "-q", "-b", "main")
    git(repo, "config", "user.email", "guard@test")
    git(repo, "config", "user.name", "guard test")
    (repo / "base.txt").write_text("base\n")
    git(repo, "add", "--", "base.txt")
    git(repo, "commit", "-q", "-m", "base")
    old_base = git(repo, "rev-parse", "HEAD")

    git(repo, "checkout", "-q", "-b", "feature")
    (repo / "feature.txt").write_text("feature payload\n")
    git(repo, "add", "--", "feature.txt")
    git(repo, "commit", "-q", "-m", "same subject")
    old_head = git(repo, "rev-parse", "HEAD")

    git(repo, "checkout", "-q", "main")
    (repo / "upstream.txt").write_text("unrelated upstream\n")
    git(repo, "add", "--", "upstream.txt")
    git(repo, "commit", "-q", "-m", "upstream change")
    git(repo, "cherry-pick", old_head)
    onto = git(repo, "rev-parse", "HEAD")

    git(repo, "checkout", "-q", "feature")
    recorded = guard(
        repo, "record", "--base", old_base, "--onto", onto
    )
    assert recorded.returncode == 0, recorded.stderr
    rebased = rebase(repo, onto)
    assert rebased.returncode == 0, rebased.stderr
    assert git(repo, "rev-parse", "HEAD") == onto, "Git should skip the already-upstream patch"

    verified = guard(repo, "verify")

    assert verified.returncode == 1, verified.stdout + verified.stderr
    assert "FAIL" in verified.stdout


def test_shortened_history_is_indeterminate(fixture_repo: Fixture) -> None:
    """The plainer case: HEAD has fewer ancestors than the recorded series."""
    repo = fixture_repo.repo
    git(repo, "checkout", "-q", "feature")
    assert guard(repo, "record", "--base", fixture_repo.old_base).returncode == 0
    git(repo, "checkout", "-q", "--detach", fixture_repo.old_base)

    verified = guard(repo, "verify")
    assert verified.returncode == 3
    assert "INDETERMINATE" in verified.stderr


def test_record_rejects_a_base_that_is_not_an_ancestor(
    fixture_repo: Fixture,
) -> None:
    """A nonempty ``BASE..HEAD`` is not evidence that BASE underlies HEAD."""
    repo = fixture_repo.repo
    git(repo, "checkout", "-q", "feature")

    recorded = guard(repo, "record", "--base", fixture_repo.new_base)

    assert recorded.returncode == 2
    assert "is not an ancestor" in recorded.stderr
    explicit = guard(
        repo,
        "check",
        fixture_repo.new_base,
        fixture_repo.old_head,
        fixture_repo.old_base,
        fixture_repo.old_head,
    )
    assert explicit.returncode == 2
    assert "old base" in explicit.stderr


def test_record_rejects_a_merge_containing_series(
    fixture_repo: Fixture,
) -> None:
    """The stateful workflow deliberately accepts only a linear recorded series."""
    repo = fixture_repo.repo
    git(repo, "checkout", "-q", "feature")
    git(repo, "checkout", "-q", "-b", "side")
    (repo / "side.txt").write_text("side\n")
    git(repo, "add", "--", "side.txt")
    git(repo, "commit", "-q", "-m", "feature: side")
    git(repo, "checkout", "-q", "feature")
    (repo / "mainline.txt").write_text("mainline\n")
    git(repo, "add", "--", "mainline.txt")
    git(repo, "commit", "-q", "-m", "feature: mainline")
    git(repo, "merge", "-q", "--no-ff", "side", "-m", "feature: merge side")

    recorded = guard(repo, "record", "--base", fixture_repo.old_base)

    assert recorded.returncode == 2
    assert "contains merge commits" in recorded.stderr
    merge_head = git(repo, "rev-parse", "HEAD")
    explicit = guard(
        repo,
        "check",
        fixture_repo.old_base,
        merge_head,
        fixture_repo.old_base,
        merge_head,
    )
    assert explicit.returncode == 0, explicit.stdout + explicit.stderr


def test_verify_rejects_a_merge_containing_current_series(
    fixture_repo: Fixture,
) -> None:
    """A merge introduced after recording is outside the stateful workflow's scope."""
    repo = fixture_repo.repo
    git(repo, "checkout", "-q", "feature")
    assert guard(repo, "record", "--base", fixture_repo.old_base).returncode == 0

    git(repo, "checkout", "-q", "-b", "rebased", fixture_repo.new_base)
    git(repo, "cherry-pick", fixture_repo.old_head)
    git(repo, "checkout", "-q", "-b", "rebased-side")
    (repo / "side.txt").write_text("side\n")
    git(repo, "add", "--", "side.txt")
    git(repo, "commit", "-q", "-m", "feature: merge-side payload")
    git(repo, "checkout", "-q", "rebased")
    git(
        repo,
        "merge",
        "-q",
        "--no-ff",
        "rebased-side",
        "-m",
        "feature: merge current series",
    )

    verified = guard(repo, "verify")

    assert verified.returncode == 2
    assert "current series contains merge commits" in verified.stderr


def test_state_is_per_worktree_not_shared(fixture_repo: Fixture, tmp_path: Path) -> None:
    """Two worktrees of one repository must not overwrite each other's recording.

    Linked worktrees share ``$GIT_COMMON_DIR``; putting the state there would
    reintroduce the shared-mutable-state class this tool responds to.
    """
    repo = fixture_repo.repo
    git(repo, "checkout", "-q", "feature")
    linked = tmp_path / "linked"
    git(repo, "worktree", "add", "-q", "-b", "other", str(linked), fixture_repo.new_base)

    assert guard(repo, "record", "--base", fixture_repo.old_base).returncode == 0
    # The linked worktree has its own state, so it sees no recording at all.
    assert guard(linked, "status").returncode == 3
    # ... and the original recording is untouched.
    assert guard(repo, "status").returncode == 0


def test_check_subcommand_is_usable_without_recording(fixture_repo: Fixture) -> None:
    """The stateless form still works, for a rebase somebody else already did."""
    repo = fixture_repo.repo
    git(repo, "checkout", "-q", "feature")
    assert rebase(repo, fixture_repo.new_base).returncode == 0
    new_head = git(repo, "rev-parse", "HEAD")

    result = guard(
        repo,
        "check",
        fixture_repo.old_base,
        fixture_repo.old_head,
        fixture_repo.new_base,
        new_head,
    )
    assert result.returncode == 0, f"{result.stdout}\n{result.stderr}"


def test_failed_git_diff_is_indeterminate_and_retains_record(
    fixture_repo: Fixture, tmp_path: Path
) -> None:
    """Two empty files produced by two failed diffs must never compare as OK."""
    repo = fixture_repo.repo
    git(repo, "checkout", "-q", "feature")
    assert guard(repo, "record", "--base", fixture_repo.old_base).returncode == 0
    assert rebase(repo, fixture_repo.new_base).returncode == 0
    state, subjects = state_files(repo)

    real_git = shutil.which("git")
    assert real_git is not None
    _fake, env = tool_path(
        tmp_path,
        "git",
        """\
for argument in "$@"; do
    if [[ $argument == diff ]]; then
        exit 86
    fi
done
exec %s "$@"
"""
        % shlex.quote(real_git),
    )
    verified = guard(repo, "verify", env=env)

    assert verified.returncode == 3
    assert "could not generate" in verified.stderr
    assert "OK" not in verified.stdout
    assert state.exists() and subjects.exists()
    retry = guard(repo, "verify")
    assert retry.returncode == 0, retry.stdout + retry.stderr


def test_failed_diff_measurement_command_is_indeterminate(
    fixture_repo: Fixture, tmp_path: Path
) -> None:
    """A failed command later in a pipe is visible through pipefail and cannot pass."""
    repo = fixture_repo.repo
    git(repo, "checkout", "-q", "feature")
    assert rebase(repo, fixture_repo.new_base).returncode == 0
    new_head = git(repo, "rev-parse", "HEAD")
    _fake, env = tool_path(tmp_path, "wc", "exit 87\n")

    checked = guard(
        repo,
        "check",
        fixture_repo.old_base,
        fixture_repo.old_head,
        fixture_repo.new_base,
        new_head,
        env=env,
    )

    assert checked.returncode == 3
    assert "could not measure" in checked.stderr
    assert "OK" not in checked.stdout


def test_failed_subject_generation_leaves_no_partial_record(
    fixture_repo: Fixture, tmp_path: Path
) -> None:
    repo = fixture_repo.repo
    git(repo, "checkout", "-q", "feature")
    real_git = shutil.which("git")
    assert real_git is not None
    _fake, env = tool_path(
        tmp_path,
        "git",
        """\
for argument in "$@"; do
    if [[ $argument == log ]]; then
        exit 88
    fi
done
exec %s "$@"
"""
        % shlex.quote(real_git),
    )

    recorded = guard(repo, "record", "--base", fixture_repo.old_base, env=env)
    state, subjects = state_files(repo)

    assert recorded.returncode == 2
    assert "cannot record the commit subjects" in recorded.stderr
    assert not state.exists() and not subjects.exists()


def test_binary_rewrite_cannot_hide_behind_the_generic_diff_marker(
    tmp_path: Path,
) -> None:
    """Two binary rewrites collide without ``git diff --binary``; reject them."""
    repo = tmp_path / "binary-repo"
    repo.mkdir()
    git(repo, "init", "-q", "-b", "main")
    git(repo, "config", "user.email", "guard@test")
    git(repo, "config", "user.name", "guard test")
    target = repo / "payload.bin"
    target.write_bytes(b"\x00base payload\xff" * 32)
    git(repo, "add", "--", "payload.bin")
    git(repo, "commit", "-q", "-m", "binary base")
    base = git(repo, "rev-parse", "HEAD")

    git(repo, "checkout", "-q", "-b", "expected")
    target.write_bytes(b"\x00expected delta\xfe" * 32)
    git(repo, "commit", "-q", "-am", "expected binary delta")
    expected = git(repo, "rev-parse", "HEAD")

    git(repo, "checkout", "-q", "-b", "rewritten", base)
    target.write_bytes(b"\x00adversarial rewrite\xfd" * 32)
    git(repo, "commit", "-q", "-am", "rewritten binary delta")
    rewritten = git(repo, "rev-parse", "HEAD")

    # Plant the exact historical hole: absent --binary, removing blob hashes
    # leaves identical text for two different binary deltas.
    def normalized_marker(head: str) -> list[str]:
        plain = git(
            repo,
            "diff",
            "--no-color",
            "--no-ext-diff",
            "--no-renames",
            base,
            head,
        )
        return [line for line in plain.splitlines() if not line.startswith("index ")]

    assert normalized_marker(expected) == normalized_marker(rewritten)

    unchanged = guard(repo, "check", base, expected, base, expected)
    assert unchanged.returncode == 0, unchanged.stdout + unchanged.stderr

    checked = guard(repo, "check", base, expected, base, rewritten)
    assert checked.returncode == 1, checked.stdout + checked.stderr
    assert "FAIL" in checked.stdout


def test_local_textconv_cannot_collapse_distinct_deltas(tmp_path: Path) -> None:
    """A local textconv that renders every blob identically must not influence proof."""
    repo = tmp_path / "textconv-repo"
    repo.mkdir()
    git(repo, "init", "-q", "-b", "main")
    git(repo, "config", "user.email", "guard@test")
    git(repo, "config", "user.name", "guard test")
    target = repo / "payload.txt"
    target.write_text("base payload\n")
    git(repo, "add", "--", "payload.txt")
    git(repo, "commit", "-q", "-m", "textconv base")
    base = git(repo, "rev-parse", "HEAD")

    git(repo, "checkout", "-q", "-b", "expected")
    target.write_text("expected delta\n")
    git(repo, "commit", "-q", "-am", "expected text delta")
    expected = git(repo, "rev-parse", "HEAD")

    git(repo, "checkout", "-q", "-b", "rewritten", base)
    target.write_text("adversarial rewrite\n")
    git(repo, "commit", "-q", "-am", "rewritten text delta")
    rewritten = git(repo, "rev-parse", "HEAD")

    converter = tmp_path / "collapse-textconv"
    converter.write_text("#!/usr/bin/env bash\nprintf 'COLLAPSED\\n'\n")
    converter.chmod(0o755)
    git_dir = Path(git(repo, "rev-parse", "--absolute-git-dir"))
    (git_dir / "info" / "attributes").write_text("payload.txt diff=collapse\n")
    git(repo, "config", "diff.collapse.textconv", str(converter))

    # Demonstrate the exact historical hole: the old diff flags implicitly enable textconv for
    # this porcelain command, erasing both real deltas before normalization even begins.
    old_flags = ("--binary", "--no-color", "--no-ext-diff", "--no-renames")
    assert git(repo, "diff", *old_flags, base, expected) == ""
    assert git(repo, "diff", *old_flags, base, rewritten) == ""

    checked = guard(repo, "check", base, expected, base, rewritten)

    assert checked.returncode == 1, checked.stdout + checked.stderr
    assert "FAIL" in checked.stdout


def test_temporary_directory_name_is_never_compiled_as_trap_code(
    fixture_repo: Fixture, tmp_path: Path
) -> None:
    repo = fixture_repo.repo
    git(repo, "checkout", "-q", "feature")
    hostile_tmp = tmp_path / "bad'; touch \"$DELTA_GUARD_MARKER\"; #'"
    hostile_tmp.mkdir()
    marker = tmp_path / "trap-executed"

    checked = guard(
        repo,
        "check",
        fixture_repo.old_base,
        fixture_repo.old_head,
        fixture_repo.old_base,
        fixture_repo.old_head,
        env={"TMPDIR": str(hostile_tmp), "DELTA_GUARD_MARKER": str(marker)},
    )

    assert checked.returncode == 0, checked.stdout + checked.stderr
    assert not marker.exists()


def test_guard_is_executable_and_reports_usage() -> None:
    assert os.access(GUARD, os.X_OK), "the tool must be committed executable"
    result = subprocess.run([str(GUARD)], capture_output=True, text=True, check=False)
    assert result.returncode == 2
