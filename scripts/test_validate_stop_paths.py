#!/usr/bin/env python3
"""Exercise the Rust validate driver's signal traps and ledger writer without a build."""

from __future__ import annotations

import json
import os
from pathlib import Path
import signal
import shutil
import socket
import subprocess
import tempfile
import time


ROOT = Path(__file__).resolve().parents[1]
VALIDATE = ROOT / "scripts" / "validate.rs"
TEST_ROOTS: list[Path] = []


def stop_test_env(
    tmpdir: Path,
    ledger: Path,
    *,
    lock_proven: bool = False,
    forged_owner: bool = False,
    include_parent: bool = True,
) -> dict[str, str]:
    TEST_ROOTS.append(tmpdir)
    env = os.environ.copy()
    env.update(
        HERMIT_VALIDATE_STOP_TEST_MODE="1",
        HERMIT_VALIDATE_LEDGER=str(ledger),
        VALIDATE_RUN_ON_DIRTY_TREE="1",
        VALIDATE_STOP_TEST_TMP_ROOT=str(tmpdir / "validation"),
        TMPDIR=str(tmpdir),
    )
    if include_parent:
        env["DEV_HERMIT_PARENT"] = str(ROOT.parent)
    else:
        env.pop("DEV_HERMIT_PARENT", None)
    if lock_proven:
        stat_fields = Path(f"/proc/{os.getpid()}/stat").read_text().rsplit(")", 1)[1].split()
        start_ticks = int(stat_fields[19])
        host = socket.gethostname().split(".", 1)[0]
        commit = subprocess.check_output(
            ["git", "rev-parse", "HEAD"], cwd=ROOT, text=True
        ).strip()
        authority = {
            "schema_version": 1,
            "admissible": True,
            "state": "held",
            "reason_code": None,
            "canonical_anchor_held": True,
            "cleanup_state": "none",
            "holder": {
                "kind": "validate",
                "target": commit,
                "host": host,
            },
            "owner": {
                "host": host,
                "liveness": "alive",
                "pid": os.getpid(),
                "start_ticks": start_ticks,
                "boot_id": Path("/proc/sys/kernel/random/boot_id").read_text().strip(),
            },
        }
        env.update(
            VALIDATE_STOP_TEST_AUTHORITY_STATUS_JSON=json.dumps(authority),
            # Plant the weaker legacy observation too. Canonical owner ancestry
            # must win; otherwise parked stop-test fixtures make every genuine
            # admitted schema-5 receipt unqualifiable.
            CI_HUB_VALIDATE_CONCURRENT="true",
        )
    if forged_owner:
        owner_file = tmpdir / "caller-chosen.owner"
        owner_file.write_text(f"pid={os.getpid()}\n")
        env.update(
            CI_HUB_VALIDATE_LOCK_OWNER_PID=str(os.getpid()),
            CI_HUB_VALIDATE_LOCK_OWNER_FILE=str(owner_file),
        )
    return env


def wait_for_text(log: Path, text: str, process: subprocess.Popen[bytes]) -> None:
    deadline = time.monotonic() + 10
    while time.monotonic() < deadline:
        if log.exists() and text in log.read_text(errors="replace"):
            return
        if process.poll() is not None:
            raise AssertionError(f"validate exited before ready: rc={process.returncode}")
        time.sleep(0.05)
    raise AssertionError(f"validate stop-test hook did not emit {text!r}")


def assert_schema5_contract(
    row: dict, *, admitted: bool = False, repo: Path = ROOT
) -> None:
    """A current writer never escapes strict evidence by downgrading schema."""
    assert row["schema_version"] == 5, row
    assert row["repo"] == "hermit", row
    assert row["producer"] == "hermit-validate-rs-git-provenance-v1", row
    assert row["git_provenance_version"] == 1, row
    expected_depth = int(
        subprocess.check_output(
            ["git", "rev-list", "--count", row["commit"]], cwd=repo, text=True
        ).strip()
    )
    assert row["git_depth"] == expected_depth > 0, row
    expected_tree = subprocess.check_output(
        ["git", "rev-parse", f"{row['commit']}^{{tree}}"], cwd=repo, text=True
    ).strip()
    assert row["tree"] == expected_tree, row
    expected_shallow = (
        subprocess.check_output(
            ["git", "rev-parse", "--is-shallow-repository"], cwd=repo, text=True
        ).strip()
        == "true"
    )
    assert row["git_is_shallow"] is expected_shallow, row
    assert row["git_comparison_ref"] == "origin/main", row
    expected_comparison = subprocess.check_output(
        ["git", "rev-parse", "--verify", "origin/main^{commit}"],
        cwd=repo,
        stderr=subprocess.PIPE,
        text=True,
    ).strip()
    assert row["git_comparison_sha"] == expected_comparison, row
    behind, ahead = map(
        int,
        subprocess.check_output(
            [
                "git",
                "rev-list",
                "--left-right",
                "--count",
                f"{expected_comparison}...{row['commit']}",
            ],
            cwd=repo,
            text=True,
        ).split(),
    )
    assert row["git_behind"] == behind >= 0, row
    assert row["git_ahead"] == ahead >= 0, row
    if admitted:
        assert row["admission"] == "ci-hub-validate-lock", row
        assert row["concurrent_validates"] == 0, row
        assert row["concurrency_proof"] == "validate_lock_owner_ancestry", row
    else:
        # These direct stop-path tests do not run through validate-lock, so their
        # honest admission result is unknown. A schema-4 fallback would incorrectly
        # grandfather that absence; schema 5 plus null is correctly unqualifiable.
        assert row["admission"] is None, row


def run_signal(
    sig: signal.Signals,
    expect_record: bool,
    *,
    prior_failure: bool = False,
    lock_proven: bool = False,
    forged_owner: bool = False,
    include_parent: bool = True,
) -> None:
    with tempfile.TemporaryDirectory(prefix=f"validate-stop-{sig.name.lower()}-") as tmp:
        tmpdir = Path(tmp)
        ledger = tmpdir / "ledger.jsonl"
        log = tmpdir / "validate.log"
        env = stop_test_env(
            tmpdir,
            ledger,
            lock_proven=lock_proven,
            forged_owner=forged_owner,
            include_parent=include_parent,
        )
        env.update(
            VALIDATE_STOP_TEST_PRIOR_FAILURE="1" if prior_failure else "0",
        )
        with log.open("wb") as output:
            process = subprocess.Popen(
                [str(VALIDATE), "full"],
                cwd=ROOT,
                env=env,
                stdout=output,
                stderr=subprocess.STDOUT,
                start_new_session=True,
            )
            wait_for_text(log, "VALIDATE_STOP_TEST_READY", process)
            process.send_signal(sig)
            rc = process.wait(timeout=10)

        rows = [json.loads(line) for line in ledger.read_text().splitlines()] if ledger.exists() else []
        if not expect_record:
            assert not rows, (sig.name, rows)
            assert rc == -sig.value, (sig.name, rc)
            return

        assert rc == 130, (sig.name, rc, log.read_text(errors="replace"))
        assert len(rows) == 1, (sig.name, rows)
        row = rows[0]
        assert_schema5_contract(row, admitted=lock_proven)
        assert row["result"] == ("fail" if prior_failure else "no_result"), row
        assert row["raw_result"] == "fail", row
        assert row["gates_run"] == row["checks"] == 2, row
        assert row["failures"] == (1 if prior_failure else 0), row
        assert row["interruption_signal"] == sig.name.removeprefix("SIG"), row


def run_incomplete_exit() -> None:
    with tempfile.TemporaryDirectory(prefix="validate-stop-incomplete-") as tmp:
        tmpdir = Path(tmp)
        # A fixture path named exactly `ledger` must remain an explicit file;
        # basename matching alone must never redirect a caller-selected path to
        # a sibling adapter.
        ledger = tmpdir / "ledger"
        env = stop_test_env(tmpdir, ledger)
        env.update(
            VALIDATE_STOP_TEST_EXIT_EARLY="1",
        )
        process = subprocess.run(
            [str(VALIDATE), "full"],
            cwd=ROOT,
            env=env,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            check=False,
        )
        assert process.returncode == 1, process.stdout.decode(errors="replace")
        rows = [json.loads(line) for line in ledger.read_text().splitlines()]
        assert len(rows) == 1, rows
        row = rows[0]
        assert_schema5_contract(row)
        # An ordinary early exit is not an operator stop. It remains a raw
        # failure unless the producer carries an explicit interruption signal.
        assert row["result"] == "fail", row
        assert row["gates_run"] == 2 and row["gates_expected"] is None, row
        assert row["interruption_signal"] is None, row


def run_canonical_adapter_contract(*, refuse: bool) -> None:
    """Production-shaped writes use the parent adapter, never a raw shadow."""
    with tempfile.TemporaryDirectory(prefix="validate-canonical-adapter-") as tmp:
        tmpdir = Path(tmp)
        canonical_root = tmpdir / "canonical-root"
        if refuse:
            parent = tmpdir / "parent"
            adapter = parent / "ci-hub" / "ledger" / "validate_rows.py"
            adapter.parent.mkdir(parents=True)
            adapter.write_text(
                "import sys\n"
                "sys.stdin.read()\n"
                "print('planted refusal', file=sys.stderr)\n"
                "raise SystemExit(2)\n"
            )
        else:
            # Exercise the REAL parent adapter, redirected only through its
            # explicit stop-test root. This proves the producer/consumer
            # contract without appending the machine's authoritative ledger.
            parent = next(
                candidate
                for candidate in ROOT.parents
                if (candidate / "ci-hub" / "ledger" / "validate_rows.py").is_file()
            )
        raw_shadow = parent / "ignored" / "validate-run-ledger.jsonl"
        raw_before = raw_shadow.read_bytes() if raw_shadow.exists() else None
        env = os.environ.copy()
        env.update(
            HERMIT_VALIDATE_STOP_TEST_MODE="1",
            DEV_HERMIT_PARENT=str(parent),
            CI_HUB_VALIDATE_LEDGER_TEST_ROOT=str(canonical_root),
            VALIDATE_RUN_ON_DIRTY_TREE="1",
            VALIDATE_STOP_TEST_TMP_ROOT=str(tmpdir / "validation"),
            VALIDATE_STOP_TEST_EXIT_EARLY="1",
            TMPDIR=str(tmpdir),
        )
        env.pop("HERMIT_VALIDATE_LEDGER", None)
        process = subprocess.run(
            [str(VALIDATE), "full"],
            cwd=ROOT,
            env=env,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            check=False,
        )
        output = process.stdout.decode(errors="replace")
        assert process.returncode == 1, output
        raw_after = raw_shadow.read_bytes() if raw_shadow.exists() else None
        assert raw_after == raw_before, "canonical write touched the retired raw shadow"
        if refuse:
            assert not list(canonical_root.glob("ledger/**/*.jsonl")), output
            assert "canonical ledger writer" in output and "refused" in output, output
        else:
            shards = list(canonical_root.glob("ledger/hermit/*/*.jsonl"))
            assert len(shards) == 1, (shards, output)
            events = [json.loads(line) for line in shards[0].read_text().splitlines()]
            assert len(events) == 1, events
            assert events[0]["schema"] == "validate-ledger/v1", events[0]
            assert_schema5_contract(events[0]["legacy_row"])
            assert "canonical ledger record appended" in output, output


def run_cleanup_signal_race() -> None:
    with tempfile.TemporaryDirectory(prefix="validate-stop-cleanup-race-") as tmp:
        tmpdir = Path(tmp)
        ledger = tmpdir / "ledger.jsonl"
        log = tmpdir / "validate.log"
        cleanup_ready = tmpdir / "cleanup-ready"
        env = stop_test_env(tmpdir, ledger)
        env.update(
            VALIDATE_STOP_TEST_EXIT_EARLY="1",
            VALIDATE_STOP_TEST_CLEANUP_READY_FILE=str(cleanup_ready),
            VALIDATE_STOP_TEST_CLEANUP_DELAY_SECONDS="1",
        )
        with log.open("wb") as output:
            process = subprocess.Popen(
                [str(VALIDATE), "full"],
                cwd=ROOT,
                env=env,
                stdout=output,
                stderr=subprocess.STDOUT,
                start_new_session=True,
            )
            deadline = time.monotonic() + 10
            while time.monotonic() < deadline and not cleanup_ready.exists():
                if process.poll() is not None:
                    raise AssertionError(
                        f"validate exited before cleanup hook: rc={process.returncode}"
                    )
                time.sleep(0.01)
            assert cleanup_ready.exists(), "cleanup hook did not become ready"
            for _ in range(20):
                process.send_signal(signal.SIGTERM)
                time.sleep(0.01)
            rc = process.wait(timeout=10)

        rows = [json.loads(line) for line in ledger.read_text().splitlines()]
        assert rc == 1, (rc, log.read_text(errors="replace"))
        assert len(rows) == 1, rows
        assert_schema5_contract(rows[0])
        assert rows[0]["result"] == "fail", rows[0]
        assert rows[0]["interruption_signal"] is None, rows[0]


def git(repo: Path, *args: str, env: dict[str, str] | None = None) -> str:
    return subprocess.check_output(
        ["git", "-C", str(repo), *args], env=env, text=True
    ).strip()


def run_fixture_receipt(
    repo: Path, root: Path, name: str
) -> tuple[subprocess.CompletedProcess[bytes], dict | None]:
    ledger = root / f"{name}.jsonl"
    env = os.environ.copy()
    env.update(
        HERMIT_VALIDATE_STOP_TEST_MODE="1",
        HERMIT_VALIDATE_LEDGER=str(ledger),
        VALIDATE_RUN_ON_DIRTY_TREE="1",
        VALIDATE_STOP_TEST_TMP_ROOT=str(root / f"validation-{name}"),
        VALIDATE_STOP_TEST_EXIT_EARLY="1",
        VALIDATE_STOP_TEST_PRIOR_FAILURE="0",
        TMPDIR=str(root),
    )
    env.pop("DEV_HERMIT_PARENT", None)
    process = subprocess.run(
        [str(repo / "scripts" / "validate.rs"), "full"],
        cwd=repo,
        env=env,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        check=False,
        timeout=180,
    )
    rows = (
        [json.loads(line) for line in ledger.read_text().splitlines()]
        if ledger.exists()
        else []
    )
    assert len(rows) <= 1, rows
    return process, rows[0] if rows else None


def run_git_provenance_causal_contract() -> None:
    """Exercise real full/shallow rows, frozen comparison, refusal, and mutation."""
    with tempfile.TemporaryDirectory(prefix="validate-git-provenance-") as tmp:
        root = Path(tmp)
        source = root / "source"
        origin = root / "origin.git"
        full = root / "full"
        shallow = root / "depth1"
        mutant = root / "mutant"
        source.mkdir()
        shutil.copytree(ROOT / "scripts", source / "scripts")
        os.symlink(ROOT / "agent-utils", source / "agent-utils", target_is_directory=True)
        subprocess.run(["git", "init", "-q", "-b", "main", str(source)], check=True)
        git_env = {
            **os.environ,
            "GIT_AUTHOR_NAME": "validate provenance test",
            "GIT_AUTHOR_EMAIL": "validate@example.invalid",
            "GIT_COMMITTER_NAME": "validate provenance test",
            "GIT_COMMITTER_EMAIL": "validate@example.invalid",
        }
        for number in range(1, 4):
            (source / "payload").write_text(f"commit {number}\n")
            subprocess.run(["git", "-C", str(source), "add", "."], check=True)
            subprocess.run(
                ["git", "-C", str(source), "commit", "-qm", f"fixture {number}"],
                check=True,
                env=git_env,
            )
        subprocess.run(
            ["git", "clone", "-q", "--bare", str(source), str(origin)], check=True
        )
        subprocess.run(["git", "clone", "-q", str(origin), str(full)], check=True)
        subprocess.run(
            ["git", "clone", "-q", "--depth=1", f"file://{origin}", str(shallow)],
            check=True,
        )

        full_process, full_row = run_fixture_receipt(full, root, "full")
        shallow_process, shallow_row = run_fixture_receipt(shallow, root, "depth1")
        assert full_process.returncode == 1, full_process.stdout.decode(errors="replace")
        assert shallow_process.returncode == 1, shallow_process.stdout.decode(
            errors="replace"
        )
        assert full_row is not None and shallow_row is not None
        assert_schema5_contract(full_row, repo=full)
        assert_schema5_contract(shallow_row, repo=shallow)
        assert full_row["commit"] == shallow_row["commit"], (full_row, shallow_row)
        assert full_row["tree"] == shallow_row["tree"], (full_row, shallow_row)
        assert full_row["git_depth"] == 3 and full_row["git_is_shallow"] is False, full_row
        assert (
            shallow_row["git_depth"] == 1
            and shallow_row["git_is_shallow"] is True
        ), shallow_row

        # The stop seam freezes provenance before announcing READY. Move HEAD
        # with an empty commit while it is parked: the tree remains identical,
        # so only the final commit+tree binding can prevent an old-code claim.
        race_ledger = root / "snapshot-race.jsonl"
        race_log = root / "snapshot-race.log"
        race_env = os.environ.copy()
        race_env.update(
            HERMIT_VALIDATE_STOP_TEST_MODE="1",
            HERMIT_VALIDATE_LEDGER=str(race_ledger),
            VALIDATE_RUN_ON_DIRTY_TREE="1",
            VALIDATE_STOP_TEST_TMP_ROOT=str(root / "validation-snapshot-race"),
            VALIDATE_STOP_TEST_PRIOR_FAILURE="0",
            VALIDATE_STOP_TEST_MAX_SECONDS="30",
            TMPDIR=str(root),
        )
        race_env.pop("DEV_HERMIT_PARENT", None)
        frozen_commit = git(full, "rev-parse", "HEAD")
        frozen_tree = git(full, "rev-parse", "HEAD^{tree}")
        with race_log.open("wb") as output:
            race_process = subprocess.Popen(
                [str(full / "scripts" / "validate.rs"), "full"],
                cwd=full,
                env=race_env,
                stdout=output,
                stderr=subprocess.STDOUT,
                start_new_session=True,
            )
            try:
                wait_for_text(race_log, "VALIDATE_STOP_TEST_READY", race_process)
                subprocess.run(
                    ["git", "-C", str(full), "commit", "--allow-empty", "-qm", "clean HEAD move"],
                    check=True,
                    env=git_env,
                )
                assert git(full, "rev-parse", "HEAD") != frozen_commit
                assert git(full, "rev-parse", "HEAD^{tree}") == frozen_tree
                race_process.send_signal(signal.SIGTERM)
                race_rc = race_process.wait(timeout=10)
            finally:
                if race_process.poll() is None:
                    os.killpg(race_process.pid, signal.SIGKILL)
                    race_process.wait(timeout=10)
        race_rows = [json.loads(line) for line in race_ledger.read_text().splitlines()]
        race_output = race_log.read_text(errors="replace")
        assert race_rc == 130, (race_rc, race_output)
        assert len(race_rows) == 1, race_rows
        assert race_rows[0]["commit"] == frozen_commit, race_rows[0]
        assert race_rows[0]["tree"] == frozen_tree, race_rows[0]
        assert race_rows[0]["commit_anchored"] is False, race_rows[0]
        assert "frozen Git snapshot moved before receipt minting" in race_output, race_output

        subprocess.run(
            ["git", "-C", str(full), "checkout", "-q", "--detach", frozen_commit],
            check=True,
        )
        restored_process, restored_row = run_fixture_receipt(
            full, root, "snapshot-restored"
        )
        restored_output = restored_process.stdout.decode(errors="replace")
        assert restored_process.returncode == 1, restored_output
        assert restored_row is not None
        assert_schema5_contract(restored_row, repo=full)
        assert "frozen Git snapshot moved before receipt minting" not in restored_output

        # A local commit establishes the orientation: origin/main is on the LEFT,
        # so a branch commit must report behind=0, ahead=1.
        (full / "ahead").write_text("local branch commit\n")
        subprocess.run(["git", "-C", str(full), "add", "ahead"], check=True)
        subprocess.run(
            ["git", "-C", str(full), "commit", "-qm", "ahead"],
            check=True,
            env=git_env,
        )
        ahead_process, ahead_row = run_fixture_receipt(full, root, "ahead")
        assert ahead_process.returncode == 1, ahead_process.stdout.decode(
            errors="replace"
        )
        assert ahead_row is not None
        assert_schema5_contract(ahead_row, repo=full)
        frozen_comparison = ahead_row["git_comparison_sha"]
        assert ahead_row["git_behind"] == 0 and ahead_row["git_ahead"] == 1, ahead_row

        # Move origin/main after the row exists. The receipt remains auditable
        # because it names the old SHA; recomputing through the live ref now gives
        # a different pair while recomputing through the recorded SHA does not.
        (source / "payload").write_text("upstream moved\n")
        subprocess.run(["git", "-C", str(source), "add", "payload"], check=True)
        subprocess.run(
            ["git", "-C", str(source), "commit", "-qm", "upstream move"],
            check=True,
            env=git_env,
        )
        subprocess.run(
            ["git", "-C", str(source), "push", "-q", str(origin), "main:main"],
            check=True,
        )
        subprocess.run(["git", "-C", str(full), "fetch", "-q", "origin"], check=True)
        assert git(full, "rev-parse", "origin/main") != frozen_comparison
        frozen_counts = tuple(
            map(
                int,
                git(
                    full,
                    "rev-list",
                    "--left-right",
                    "--count",
                    f"{frozen_comparison}...{ahead_row['commit']}",
                ).split(),
            )
        )
        live_counts = tuple(
            map(
                int,
                git(
                    full,
                    "rev-list",
                    "--left-right",
                    "--count",
                    f"origin/main...{ahead_row['commit']}",
                ).split(),
            )
        )
        assert frozen_counts == (ahead_row["git_behind"], ahead_row["git_ahead"])
        assert live_counts != frozen_counts, (live_counts, frozen_counts, ahead_row)

        subprocess.run(["git", "clone", "-q", str(origin), str(mutant)], check=True)
        subprocess.run(
            ["git", "-C", str(mutant), "update-ref", "-d", "refs/remotes/origin/main"],
            check=True,
        )
        missing_process, missing_row = run_fixture_receipt(mutant, root, "missing-ref")
        missing_output = missing_process.stdout.decode(errors="replace")
        assert missing_process.returncode == 2, missing_output
        assert missing_row is None, missing_output
        assert "Git provenance probe" in missing_output, missing_output

        script = mutant / "scripts" / "validate.rs"
        text = script.read_text()
        needle = (
            "fn required_git_provenance(root: &Path) -> Result<GitProvenance, String> {\n"
            "    probe_git_provenance(root, GIT_COMPARISON_REF)\n"
            "}"
        )
        replacement = (
            "fn required_git_provenance(root: &Path) -> Result<GitProvenance, String> {\n"
            "    probe_git_provenance(root, GIT_COMPARISON_REF).or_else(|_| {\n"
            "        let commit = parse_sha40(\"HEAD\", &git_output(root, &[\"rev-parse\", \"--verify\", \"HEAD^{commit}\"])? )?;\n"
            "        let tree = parse_sha40(\"HEAD tree\", &git_output(root, &[\"rev-parse\", \"--verify\", &format!(\"{commit}^{{tree}}\")])? )?;\n"
            "        let depth = parse_git_depth(&git_output(root, &[\"rev-list\", \"--count\", &commit])?)?;\n"
            "        Ok(GitProvenance { tree, depth, is_shallow: false, comparison_ref: GIT_COMPARISON_REF.into(), comparison_sha: commit.clone(), commit, behind: 0, ahead: 0 })\n"
            "    })\n"
            "}"
        )
        assert text.count(needle) == 1, "Git-provenance mutation seam drifted"
        script.write_text(text.replace(needle, replacement))
        mutated_process, mutated_row = run_fixture_receipt(mutant, root, "zero-fallback")
        mutated_output = mutated_process.stdout.decode(errors="replace")
        assert mutated_process.returncode == 1, mutated_output
        assert mutated_row is not None, mutated_output
        assert mutated_row["git_ahead"] == 0 and mutated_row["git_behind"] == 0, mutated_row
        try:
            assert_schema5_contract(mutated_row, repo=mutant)
        except (AssertionError, subprocess.CalledProcessError):
            pass
        else:
            raise AssertionError(f"literal zero-fallback mutation escaped: {mutated_row}")
        print(
            "GIT PROVENANCE CAUSAL CONTRACT: full/depth-1 rows differ; "
            "clean HEAD race is unanchored and restored snapshot binds; ahead orientation "
            "and frozen SHA bind; missing ref and zero fallback refuse"
        )


def main() -> None:
    for sig in (signal.SIGTERM, signal.SIGINT, signal.SIGHUP):
        run_signal(sig, expect_record=True)
    run_signal(signal.SIGKILL, expect_record=False)
    run_signal(signal.SIGTERM, expect_record=True, prior_failure=True)
    run_signal(signal.SIGTERM, expect_record=True, lock_proven=True)
    run_signal(signal.SIGTERM, expect_record=True, include_parent=False)
    # The retired shell contract trusted these caller-selected values. Rust must
    # ignore them: only the canonical authority query can establish admission.
    run_signal(signal.SIGTERM, expect_record=True, forged_owner=True)
    run_incomplete_exit()
    run_canonical_adapter_contract(refuse=False)
    run_canonical_adapter_contract(refuse=True)
    run_cleanup_signal_race()
    run_git_provenance_causal_contract()
    leaked = [path for path in TEST_ROOTS if path.exists()]
    assert not leaked, f"stop-path test residue: {leaked}"
    print(
        "PASS: TERM/INT/HUP => NO-RESULT; KILL => no record; "
        "prior failure remains fail; forged owner path is unadmitted; canonical adapter "
        "accept/refuse bracketed; parentless receipt passes; Git provenance causal/mutation "
        "contract passes; cleanup is signal-atomic"
    )


if __name__ == "__main__":
    main()
