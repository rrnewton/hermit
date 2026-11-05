#!/usr/bin/env python3
"""Per-agent turn-runner. Runs inside the agent's presentation window.

Not invoked by the coordinator directly — ``agent_up.py`` launches it. It owns
one durable harness session and drives it turn by turn:

  * Block on the agent's inbox for the next message.
  * First message  -> ``codex exec ... -``           (captures the new session id)
  * Later messages -> ``codex exec resume <id> ... -``
  * Stream the ``--json`` event lines into transcript.log, capture the final
    answer via a file unique to the turn, publish ``last-message.txt``, then append the sentinel line
    ``===TURN-DONE <seq> rc=<n> <iso>===`` that the coordinator's Monitor
    watches to learn the turn finished.

Crash-visible by design: if a harness exits non-zero (or a turn times out), the
sentinel records ``rc=<n>`` / ``rc=timeout`` and the runner keeps living so the
agent can be re-prompted. Only an inbox STOP file (written by agent_down) or a
SIGTERM ends the loop.
"""

from __future__ import annotations

import json
import contextlib
import fcntl
import os
import re
import signal
import subprocess
import sys
import threading
import time
from dataclasses import dataclass
from pathlib import Path
from types import FrameType
from typing import Iterator, Optional

if __package__ in (None, ""):
    sys.path.insert(0, str(Path(__file__).resolve().parents[2]))
from agentctl.foreign import lib

_STOP_REQUESTED = False
_AGY_SESSION_RE = re.compile(r"Stream completed for ([0-9a-fA-F-]{36})")
_AGY_RESOLVED_MODEL_RE = re.compile(
    r'Propagating selected model override to backend: label="([^"]+)"'
)
_AGY_QUOTA_RE = re.compile(
    r"\bRESOURCE_EXHAUSTED\b|\bcode 429\b|Individual quota reached",
    re.IGNORECASE,
)
_AGY_RESET_RE = re.compile(
    r"Resets in\s+([0-9]+(?:d|h|m|s)(?:[0-9]+(?:d|h|m|s))*)",
    re.IGNORECASE,
)
_AGY_AUTH_RE = re.compile(
    r"error getting token source|not logged into Antigravity",
    re.IGNORECASE,
)
_MUSE_MAX_EVENTS = 100_000
_MUSE_MAX_EVENT_BYTES = 1_048_576
_MUSE_MAX_TRANSCRIPT_BYTES = 64 * 1_048_576
_MUSE_MAX_ANSWER_BYTES = 4 * 1_048_576
_MUSE_RUNNER_OWNED_OPTIONS = frozenset(
    ("--", "--api-key-stdin", "--json", "--model", "--prompt-file", "--session-id")
)
_MUSE_WORKSPACE_RECORD_KEYS = frozenset(
    ("command_id", "commit", "dirty", "reference", "vcs", "workspace_root")
)


@dataclass(frozen=True)
class _AgyOutcome:
    rc_label: str
    status: str
    done_fields: tuple[str, ...] = ()
    message_if_empty: str = ""


def _handle_sigterm(_signum: int, _frame: Optional[FrameType]) -> None:
    global _STOP_REQUESTED
    _STOP_REQUESTED = True


@contextlib.contextmanager
def _owned_harness(name: str, proc: subprocess.Popen[str]) -> Iterator[threading.Event]:
    """Keep process ownership visible while a watchdog handles stop and timeout."""
    timed_out, finished = threading.Event(), threading.Event()
    path = lib.agent_dir(name) / "active-harness.json"

    def kill_group() -> None:
        try:
            os.killpg(proc.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass

    def watch() -> None:
        deadline = time.monotonic() + lib.TURN_TIMEOUT_S
        while not finished.wait(0.05):
            # STOP permits the active turn to finish during the caller's grace
            # period; SIGTERM requests interruption after that period expires.
            if _STOP_REQUESTED:
                kill_group()
                return
            if time.monotonic() >= deadline:
                timed_out.set()
                kill_group()
                return

    watcher = threading.Thread(target=watch, daemon=True)
    watcher.start()
    try:
        yield timed_out
    except BaseException:
        kill_group()
        proc.wait()
        raise
    finally:
        finished.set()
        watcher.join(timeout=1)
        path.unlink(missing_ok=True)
        lib._sync_directory(path.parent)


def _spawn_harness(name: str, argv: list[str], cwd: str, *, stdin: bool = False) -> subprocess.Popen[str]:
    with lib._harness_lock(name):
        if _STOP_REQUESTED or lib.stop_path(name).exists():
            raise InterruptedError("worker was stopped before harness launch")
        proc = subprocess.Popen(argv, cwd=cwd,
                                stdin=subprocess.PIPE if stdin else subprocess.DEVNULL,
                                stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                                text=True, start_new_session=True)
        try:
            lib.record_active_harness(name,
                lib.RunnerIdentity(os.getpid(), lib.pid_start_time(os.getpid())),
                lib.RunnerIdentity(proc.pid, lib.pid_start_time(proc.pid)))
        except BaseException:
            try:
                os.killpg(proc.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            proc.wait()
            raise
    return proc


def _append(name: str, text: str) -> None:
    with lib.transcript_path(name).open("a") as fh:
        fh.write(text)
        if not text.endswith("\n"):
            fh.write("\n")


def _exit_reason(name: str, reason: str) -> None:
    print(f"[subagents] runner clean exit: {reason}", file=sys.stderr, flush=True)


def _set_status(name: str, status: str, *, mark_turn: bool = False) -> None:
    with lib.registry_lock() as agents:
        rec = agents.get(name)
        if rec is None:
            return
        rec.status = status
        if mark_turn:
            rec.last_turn_at = lib.now_iso()


def _pane_echo(stripped: str) -> None:
    """Print a one-line live summary of a codex --json event to stdout (the
    presentation pane). Keeps panes human-watchable without duplicating full JSON."""
    if not stripped:
        return
    try:
        ev = json.loads(stripped)
    except json.JSONDecodeError:
        print(stripped[:200], flush=True)
        return
    if not isinstance(ev, dict):
        return
    etype = ev.get("type", "")
    raw_item = ev.get("item")
    item: dict[str, object] = raw_item if isinstance(raw_item, dict) else {}
    itype = item.get("type", "")
    if etype == "item.started" and itype == "command_execution":
        cmd = str(item.get("command", ""))[:160]
        print(f"$ {cmd}", flush=True)
    elif etype == "item.completed" and itype == "agent_message":
        text = str(item.get("text", "")).strip().splitlines()
        for ln in text[:12]:
            print(f"| {ln[:200]}", flush=True)
        if len(text) > 12:
            print(f"| ... ({len(text) - 12} more lines in transcript)", flush=True)
    elif etype == "thread.started":
        print(f"[session {ev.get('thread_id', '?')}]", flush=True)


def _record_session_id(name: str, session_id: str) -> None:
    with lib.registry_lock() as agents:
        rec = agents.get(name)
        if rec is not None and not rec.session_id:
            rec.session_id = session_id


def _record_runner_pid(name: str, stage_token: Optional[str] = None) -> None:
    pid = os.getpid()
    started_at = lib.pid_start_time(pid)
    identity = lib.RunnerIdentity(pid=pid, started_at=started_at)
    if stage_token is not None:
        lib.write_staged_runner(name, stage_token, identity)
        return
    lib.runner_pid_path(name).write_text(f"{pid}\n")
    with lib.registry_lock() as agents:
        rec = agents.get(name)
        if rec is None:
            lib.die(f"runner started for unknown agent {name!r} (no registry row)")
        rec.runner_pid = pid
        rec.runner_started_at = started_at
        rec.status = "idle"


def _build_codex_argv(rec: lib.AgentRecord, msg: lib.Message) -> list[str]:
    argv: list[str] = [lib.CODEX_BIN, "exec"]
    if rec.session_id:
        argv += ["resume", rec.session_id]
    model = msg.model or rec.model
    if model:
        argv += ["-m", model]
    # The sender persists per-turn effort in the inbox: changing a sender's
    # environment cannot otherwise update an already-running worker process.
    effort = msg.effort or os.environ.get("SUBAGENT_EFFORT")
    if effort:
        argv += ["-c", f"model_reasoning_effort={json.dumps(effort)}"]
    if rec.codex_bypass_permissions:
        argv.append("--dangerously-bypass-approvals-and-sandbox")
    argv += [
        "--skip-git-repo-check",
        "--json",
        "-o",
        str(lib.agent_dir(rec.name) / f"codex-answer-{msg.seq:012d}.txt"),
        "-",
    ]
    return argv


def _agy_turn_log_path(name: str, seq: int) -> Path:
    return lib.agent_dir(name) / f"agy-turn-{seq:012d}.log"


def _build_agy_argv(rec: lib.AgentRecord, msg: lib.Message, log_path: Path) -> list[str]:
    argv: list[str] = [lib.AGY_BIN]
    model = msg.model or rec.model
    if model:
        argv += ["--model", model]
    if rec.session_id:
        argv += ["--conversation", rec.session_id]
    argv += [
        "--add-dir",
        rec.cwd,
        "--log-file",
        str(log_path),
        "--print-timeout",
        f"{lib.TURN_TIMEOUT_S}s",
        "--print",
        msg.text,
    ]
    return argv


def _build_muse_argv(rec: lib.AgentRecord, msg: lib.Message) -> list[str]:
    """Build one exact Muse exec turn around the caller-owned session id.

    Muse 1.3 parses options before ``exec`` as TUI options.  Put the subcommand
    immediately after the executable on every turn; ``--session-id`` is the
    exec protocol's continuation identity, not the separate TUI ``resume``
    subcommand.
    """
    if not rec.session_id:
        raise ValueError("Muse worker has no caller-owned session id")
    for item in rec.harness_args:
        key = item.split("=", 1)[0]
        if key in _MUSE_RUNNER_OWNED_OPTIONS or item in ("exec", "resume"):
            raise ValueError(f"Muse harness arguments cannot override runner-owned argument {key!r}")
    argv = [lib.MUSE_BIN, "exec", *rec.harness_args]
    model = msg.model or rec.model
    if model:
        argv.extend(("--model", model))
    argv.extend(("--json", "--session-id", rec.session_id, "--", msg.text))
    return argv


def _is_muse_workspace_epilogue(
    event: dict[str, object], payload: dict[str, object], command_id: str | None
) -> bool:
    """Recognize Muse's sole observed post-terminal bookkeeping event.

    This event is accepted only once by the caller and never changes the
    terminal outcome or answer.  Keep its version-1 shape and command binding
    strict so a second output cannot masquerade as harmless bookkeeping.
    """
    if command_id is None:
        return False
    if (
        event.get("payload_type") != "session.workspace_branch.observed"
        or event.get("payload_schema_version") != 1
        or event.get("record_type") != "event"
        or event.get("durability") != "durable"
        or event.get("causation_id") != command_id
        or set(payload) != {"command_id", "kind", "record"}
        or payload.get("command_id") != command_id
        or payload.get("kind") != "workspace_branch_observed"
    ):
        return False
    record = payload.get("record")
    if not isinstance(record, dict):
        return False
    record_keys = set(record)
    if (
        not {"command_id", "commit", "reference", "vcs", "workspace_root"}
        <= record_keys
        or not record_keys <= _MUSE_WORKSPACE_RECORD_KEYS
        or record.get("command_id") != command_id
        or not isinstance(record.get("commit"), str)
        or record.get("vcs") != "git"
        or not isinstance(record.get("workspace_root"), str)
        or ("dirty" in record and not isinstance(record["dirty"], bool))
    ):
        return False
    reference = record.get("reference")
    return (
        isinstance(reference, dict)
        and set(reference) == {"kind", "name"}
        and reference.get("kind") == "branch"
        and isinstance(reference.get("name"), str)
    )


def _capture_agy_session_id(log_path: Path) -> Optional[str]:
    if not log_path.exists():
        return None
    matches = _AGY_SESSION_RE.findall(log_path.read_text(errors="replace"))
    return matches[-1].lower() if matches else None


def _agy_model_note(requested: Optional[str], log_path: Path, seq: int) -> str:
    requested_note = requested or "<agy-default>"
    if not log_path.exists():
        return (
            f"===AGY MODEL WARNING seq={seq} requested={requested_note} "
            "resolved=<unknown> reason=missing-log==="
        )
    log_text = log_path.read_text(errors="replace")
    resolved_matches = _AGY_RESOLVED_MODEL_RE.findall(log_text)
    resolved = resolved_matches[-1] if resolved_matches else "<agy-default>"
    warning_reasons: list[str] = []
    if requested is not None and resolved != requested:
        warning_reasons.append("requested-name-did-not-match-resolved-name")
    if (
        requested is not None
        and resolved != requested
        and ("not in local config" in log_text or "defaulting to" in log_text)
    ):
        warning_reasons.append("agy-reported-defaulting")
    if warning_reasons:
        return (
            f"===AGY MODEL WARNING seq={seq} requested={requested_note} "
            f"resolved={resolved} reason={','.join(warning_reasons)}==="
        )
    return f"===AGY MODEL seq={seq} requested={requested_note} resolved={resolved}==="


def _read_agy_turn_log(log_path: Path) -> tuple[str, Optional[str]]:
    if not log_path.exists():
        return "", None
    try:
        return log_path.read_text(errors="replace"), None
    except OSError as exc:
        return (
            "",
            f"===AGY LOG WARNING log={log_path} reason={type(exc).__name__}: {exc}===",
        )


def _classify_agy_outcome(
    raw_rc_label: str,
    stdout_text: str,
    stderr_text: str,
    turn_log_text: str,
) -> _AgyOutcome:
    corpus = "\n".join((stdout_text, stderr_text, turn_log_text))
    if _AGY_QUOTA_RE.search(corpus):
        reset_matches = _AGY_RESET_RE.findall(corpus)
        done_fields: tuple[str, ...] = ()
        reset_note = ""
        if reset_matches:
            reset_in = reset_matches[-1]
            done_fields = (f"reset_in={reset_in}",)
            reset_note = f" reset_in={reset_in}"
        return _AgyOutcome(
            rc_label="quota_exhausted",
            status="quota_exhausted",
            done_fields=done_fields,
            message_if_empty=f"[AGY ERROR] RESOURCE_EXHAUSTED (code 429).{reset_note}".strip(),
        )
    if _AGY_AUTH_RE.search(corpus):
        return _AgyOutcome(
            rc_label="auth_error",
            status="auth_error",
            message_if_empty=(
                "[AGY ERROR] Antigravity auth error: not logged in; human re-login required."
            ),
        )
    return _AgyOutcome(
        rc_label=raw_rc_label,
        status="idle" if raw_rc_label == "0" else "error",
    )


def _timeout_text(value: object) -> str:
    if isinstance(value, str):
        return value
    if isinstance(value, bytes):
        return value.decode(errors="replace")
    return ""


def _run_codex_turn(name: str, rec: lib.AgentRecord, msg: lib.Message) -> None:
    resume = bool(rec.session_id)
    argv = _build_codex_argv(rec, msg)
    model_note = msg.model or rec.model or "<codex-default>"
    mode = "resume" if resume else "exec"
    answer_path = Path(argv[argv.index("-o") + 1])
    answer_path.unlink(missing_ok=True)
    lib.last_message_path(name).write_text("")

    _set_status(name, "busy")
    lib.write_event("turn_started", name, seq=msg.seq, preview=lib.last_message_preview(name))
    _append(
        name,
        f"===TURN {msg.seq} START {lib.now_iso()} mode={mode} "
        f"model={model_note}===\n>>> {msg.text}",
    )

    proc = _spawn_harness(name, argv, rec.cwd, stdin=True)
    assert proc.stdin is not None and proc.stdout is not None

    captured_session: Optional[str] = None
    captured_error: Optional[str] = None
    stderr_chunks: list[str] = []
    def drain_stderr() -> None:
        assert proc.stderr is not None
        while block := proc.stderr.read(4096):
            stderr_chunks.append(block)
            if len(stderr_chunks) > 4:
                del stderr_chunks[0]

    with _owned_harness(name, proc) as timed_out:
        stderr_reader = threading.Thread(target=drain_stderr, daemon=True)
        stderr_reader.start()
        try:
            proc.stdin.write(msg.text)
            proc.stdin.close()
        except BrokenPipeError:
            pass
        with lib.transcript_path(name).open("a") as tf:
            for line in proc.stdout:
                tf.write(line)
                tf.flush()
                stripped = line.strip()
                # Mirror a compact live view to the presentation pane so an attached human
                # sees the agent working (full detail stays in the transcript).
                _pane_echo(stripped)
                if not stripped:
                    continue
                try:
                    event = json.loads(stripped)
                except json.JSONDecodeError:
                    continue
                if isinstance(event, dict) and event.get("type") == "thread.started":
                    tid = event.get("thread_id")
                    if isinstance(tid, str):
                        captured_session = tid
                if isinstance(event, dict) and event.get("type") in ("error", "turn.failed"):
                    err_obj = event.get("error")
                    emsg = event.get("message") or (
                        err_obj.get("message") if isinstance(err_obj, dict) else None
                    )
                    if isinstance(emsg, str) and emsg:
                        captured_error = emsg

        rc = proc.wait()
        stderr_reader.join()
    stderr_tail = "".join(stderr_chunks).strip()

    if captured_session and not resume:
        _record_session_id(name, captured_session)

    last_msg = ""
    lm_path = lib.last_message_path(name)
    if answer_path.exists():
        last_msg = answer_path.read_text().strip()
    if rc != 0 and not last_msg:
        # Surface harness/API errors (e.g. model usage limits with reset times)
        # instead of an empty message + bare nonzero rc.
        last_msg = f"[HARNESS ERROR] {captured_error or stderr_tail[-2000:] or f'process exited {rc}'}"
    lm_path.write_text(last_msg)

    rc_label = "timeout" if timed_out.is_set() else str(rc)
    _append(name, f"===TURN {msg.seq} OUTPUT===\n{last_msg}")
    if rc != 0 and stderr_tail:
        _append(name, f"===TURN {msg.seq} STDERR (tail)===\n{stderr_tail[-2000:]}")
    _append(name, f"===TURN-DONE {msg.seq} rc={rc_label} {lib.now_iso()}===")
    lib.write_event("TURN-DONE", name, seq=msg.seq, rc=rc_label, preview=lib.last_message_preview(name))

    _set_status(name, "idle" if rc == 0 else "error", mark_turn=True)


def _run_agy_turn(name: str, rec: lib.AgentRecord, msg: lib.Message) -> None:
    resume = bool(rec.session_id)
    turn_log = _agy_turn_log_path(name, msg.seq)
    argv = _build_agy_argv(rec, msg, turn_log)
    requested_model = msg.model or rec.model
    model_note = requested_model or "<agy-default>"
    mode = "resume" if resume else "exec"

    _set_status(name, "busy")
    lib.write_event("turn_started", name, seq=msg.seq, preview=lib.last_message_preview(name))
    _append(
        name,
        f"===TURN {msg.seq} START {lib.now_iso()} mode={mode} "
        f"harness=agy model={model_note} log={turn_log}===\n>>> {msg.text}",
    )

    proc = _spawn_harness(name, argv, rec.cwd)
    with _owned_harness(name, proc) as timed_out:
        stdout_text, stderr_text = proc.communicate()
    rc = proc.returncode

    captured_session: Optional[str] = None
    nosession = False
    if not resume:
        captured_session = _capture_agy_session_id(turn_log)
        if captured_session is None:
            nosession = True
        else:
            _record_session_id(name, captured_session)

    raw_rc_label = "timeout" if timed_out.is_set() else str(rc)
    if nosession:
        raw_rc_label = "nosession"
    turn_log_text, turn_log_warning = _read_agy_turn_log(turn_log)
    outcome = _classify_agy_outcome(raw_rc_label, stdout_text, stderr_text, turn_log_text)

    last_message = stdout_text
    if not last_message.strip() and outcome.message_if_empty:
        last_message = outcome.message_if_empty
    lib.last_message_path(name).write_text(last_message)
    for line in last_message.strip().splitlines()[:12]:
        print(f"| {line[:200]}", flush=True)
    if len(last_message.strip().splitlines()) > 12:
        print("| ... (more lines in transcript)", flush=True)

    _append(name, _agy_model_note(requested_model, turn_log, msg.seq))
    if turn_log_warning is not None:
        _append(name, turn_log_warning)
    if nosession:
        _append(
            name,
            f"===AGY SESSION ERROR seq={msg.seq} log={turn_log} "
            "missing 'Stream completed for <uuid>'; refusing to continue without session_id===",
        )
    _append(name, f"===TURN {msg.seq} OUTPUT===\n{last_message.strip()}")
    if stderr_text.strip():
        _append(name, f"===TURN {msg.seq} STDERR (tail)===\n{stderr_text.strip()[-2000:]}")
    done_suffix = "" if not outcome.done_fields else " " + " ".join(outcome.done_fields)
    _append(name, f"===TURN-DONE {msg.seq} rc={outcome.rc_label}{done_suffix} {lib.now_iso()}===")
    lib.write_event(
        "TURN-DONE",
        name,
        seq=msg.seq,
        rc=outcome.rc_label,
        preview=lib.last_message_preview(name),
    )

    _set_status(name, outcome.status, mark_turn=True)


def _run_muse_turn(name: str, rec: lib.AgentRecord, msg: lib.Message) -> None:
    """Run and validate one bounded Muse JSONL turn with a stable session id."""
    argv = _build_muse_argv(rec, msg)
    requested_model = msg.model or rec.model or "<muse-default>"
    _set_status(name, "busy")
    lib.write_event("turn_started", name, seq=msg.seq, preview=lib.last_message_preview(name))
    _append(
        name,
        f"===TURN {msg.seq} START {lib.now_iso()} mode=exec harness=muse "
        f"model={requested_model} session={rec.session_id}===\n>>> {msg.text}",
    )
    proc = _spawn_harness(name, argv, rec.cwd)
    assert proc.stdout is not None
    stderr_chunks: list[str] = []

    def drain_stderr() -> None:
        assert proc.stderr is not None
        try:
            while block := proc.stderr.read(4096):
                stderr_chunks.append(block)
                if len(stderr_chunks) > 16:
                    del stderr_chunks[0]
        except UnicodeError as exc:
            # A malformed diagnostic must not produce an unhandled exception in
            # the background reader.  Preserve a bounded, printable indication.
            stderr_chunks.append(f"[Muse stderr Unicode decode error: {exc}]")

    event_count = 0
    transcript_bytes = 0
    previous_sequence: int | None = None
    accepted_command_id: str | None = None
    terminal: str | None = None
    saw_workspace_epilogue = False
    answer = ""
    protocol_error: str | None = None
    with _owned_harness(name, proc) as timed_out:
        stderr_reader = threading.Thread(target=drain_stderr, daemon=True)
        stderr_reader.start()
        with lib.transcript_path(name).open("a") as transcript:
            while True:
                try:
                    line = proc.stdout.readline(_MUSE_MAX_EVENT_BYTES + 1)
                except UnicodeError as exc:
                    protocol_error = f"Muse JSONL Unicode decode failure: {exc}"
                    break
                if not line:
                    break
                try:
                    encoded = line.encode("utf-8")
                except UnicodeError as exc:
                    protocol_error = f"Muse JSONL contains invalid Unicode: {exc}"
                    break
                if len(encoded) > _MUSE_MAX_EVENT_BYTES:
                    protocol_error = f"Muse JSONL event exceeds {_MUSE_MAX_EVENT_BYTES} bytes"
                    try:
                        os.killpg(proc.pid, signal.SIGKILL)
                    except ProcessLookupError:
                        pass
                    break
                transcript_bytes += len(encoded)
                if transcript_bytes > _MUSE_MAX_TRANSCRIPT_BYTES:
                    protocol_error = (
                        f"Muse turn transcript exceeds {_MUSE_MAX_TRANSCRIPT_BYTES} bytes"
                    )
                    try:
                        os.killpg(proc.pid, signal.SIGKILL)
                    except ProcessLookupError:
                        pass
                    break
                event_count += 1
                if event_count > _MUSE_MAX_EVENTS:
                    protocol_error = f"Muse turn exceeds {_MUSE_MAX_EVENTS} events"
                    try:
                        os.killpg(proc.pid, signal.SIGKILL)
                    except ProcessLookupError:
                        pass
                    break
                transcript.write(line)
                transcript.flush()
                try:
                    event = json.loads(line)
                except (json.JSONDecodeError, RecursionError) as exc:
                    protocol_error = f"Muse emitted invalid JSONL: {exc}"
                    break
                if not isinstance(event, dict) or event.get("schema_version") != 1:
                    protocol_error = "Muse emitted an unsupported event envelope"
                    break
                stream = event.get("stream")
                if not isinstance(stream, dict) or stream.get("kind") != "session" or stream.get("id") != rec.session_id:
                    protocol_error = "Muse event session identity changed"
                    break
                sequence = event.get("sequence")
                if (not isinstance(sequence, int) or isinstance(sequence, bool) or sequence < 0
                        or (previous_sequence is not None and sequence <= previous_sequence)):
                    protocol_error = "Muse event sequence is invalid or non-monotonic"
                    break
                previous_sequence = sequence
                payload = event.get("payload")
                payload = payload if isinstance(payload, dict) else {}
                if terminal is not None:
                    if (
                        not saw_workspace_epilogue
                        and _is_muse_workspace_epilogue(
                            event, payload, accepted_command_id
                        )
                    ):
                        saw_workspace_epilogue = True
                        continue
                    protocol_error = "Muse emitted an unexpected event after its terminal outcome"
                    break
                if event.get("payload_type") == "runtime.command.accepted":
                    command_id = payload.get("command_id")
                    if (accepted_command_id is not None
                            or payload.get("command_kind") != "turn.submit"
                            or not isinstance(command_id, str) or not command_id):
                        protocol_error = "Muse emitted an invalid or duplicate command acceptance"
                        break
                    accepted_command_id = command_id
                if event.get("payload_type") == "run.terminal.completed":
                    if (accepted_command_id is None
                            or payload.get("command_id") != accepted_command_id):
                        protocol_error = "Muse terminal outcome does not match the accepted command"
                        break
                    terminal_value = payload.get("terminal")
                    text_value = payload.get("text")
                    if terminal_value not in ("completed", "failed", "cancelled") or not isinstance(text_value, str):
                        protocol_error = "Muse terminal outcome is malformed"
                        break
                    try:
                        answer_bytes = len(text_value.encode("utf-8"))
                    except UnicodeError as exc:
                        protocol_error = f"Muse final answer contains invalid Unicode: {exc}"
                        break
                    if answer_bytes > _MUSE_MAX_ANSWER_BYTES:
                        protocol_error = f"Muse final answer exceeds {_MUSE_MAX_ANSWER_BYTES} bytes"
                        break
                    terminal, answer = str(terminal_value), text_value
        if protocol_error is not None and proc.poll() is None:
            try:
                os.killpg(proc.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
        rc = proc.wait()
        stderr_reader.join()
    stderr_tail = "".join(stderr_chunks).strip()[-8000:]
    if timed_out.is_set():
        outcome = "timeout"
    elif protocol_error is not None:
        outcome = "protocol_error"
    elif rc != 0:
        outcome = str(rc)
    elif accepted_command_id is None or terminal is None:
        protocol_error = "Muse exited without a complete accepted terminal outcome"
        outcome = "protocol_error"
    elif terminal != "completed":
        outcome = terminal
    else:
        outcome = "0"
    if outcome == "protocol_error":
        detail = protocol_error or "Muse emitted an invalid event stream"
        if answer:
            answer = f"[MUSE ERROR] {detail}\n[MUSE UNVERIFIED OUTPUT]\n{answer}"
        else:
            answer = f"[MUSE ERROR] {detail}"
    elif not answer:
        answer = f"[MUSE ERROR] {protocol_error or stderr_tail or f'process exited {rc}'}"
    lib.last_message_path(name).write_text(answer)
    for line in answer.strip().splitlines()[:12]:
        print(f"| {line[:200]}", flush=True)
    if len(answer.strip().splitlines()) > 12:
        print("| ... (more lines in transcript)", flush=True)
    _append(name, f"===TURN {msg.seq} OUTPUT===\n{answer}")
    if stderr_tail:
        _append(name, f"===TURN {msg.seq} STDERR (tail)===\n{stderr_tail}")
    _append(name, f"===TURN-DONE {msg.seq} rc={outcome} {lib.now_iso()}===")
    lib.write_event("TURN-DONE", name, seq=msg.seq, rc=outcome,
                    preview=lib.last_message_preview(name))
    _set_status(name, "idle" if outcome == "0" else "error", mark_turn=True)


def _run_turn(name: str, msg: lib.Message) -> None:
    with lib.registry_lock() as agents:
        rec = agents.get(name)
        if rec is None:
            lib.die(f"agent {name!r} vanished from registry mid-run")
    if rec.harness == "codex":
        _run_codex_turn(name, rec, msg)
    elif rec.harness == "agy":
        _run_agy_turn(name, rec, msg)
    elif rec.harness == "muse":
        _run_muse_turn(name, rec, msg)
    else:
        _append(
            name,
            f"===TURN {msg.seq} START {lib.now_iso()} mode=error "
            f"harness={rec.harness}===\n>>> {msg.text}",
        )
        _append(name, f"===TURN-DONE {msg.seq} rc=unsupported_harness {lib.now_iso()}===")
        _set_status(name, "error", mark_turn=True)


@contextlib.contextmanager
def _intake_lock(name: str) -> Iterator[None]:
    path = lib.agent_dir(name) / ".headless-turn.lock"
    fd = os.open(path, os.O_CREAT | os.O_RDWR | os.O_NOFOLLOW, 0o600)
    try:
        fcntl.flock(fd, fcntl.LOCK_EX)
        yield
    finally:
        os.close(fd)


def _quarantine(name: str, path: Path, reason: str) -> None:
    destination = lib.failed_dir(name) / path.name
    if destination.exists():
        destination = destination.with_name(f"{path.stem}-{time.time_ns()}.json")
    lib._write_durable_json(destination.with_suffix(".json.error"), {
        "outcome": "possibly_submitted", "error": reason, "recorded_at": lib.now_iso(),
    })
    os.replace(path, destination)
    lib._sync_directory(destination.parent)
    lib._sync_directory(path.parent)
    _set_status(name, "error")
    lib.write_event("turn_quarantined", name, rc="possibly_submitted", preview=reason)


def _recover_inflight(name: str) -> None:
    for path in sorted(lib.inflight_dir(name).glob("*.json")):
        _quarantine(name, path, "runner exited after claiming this turn; inspect its transcript before retrying")


def _consume(name: str, msg_path: Path) -> None:
    with _intake_lock(name):
        _recover_inflight(name)
        if not msg_path.exists():
            return
        if (lib.agent_dir(name) / "automation-paused").exists() or lib.migration_pause_path(name).exists():
            return
        destination = lib.processed_dir(name) / msg_path.name
        if destination.exists():
            _quarantine(name, msg_path, "this turn sequence already has a completed request")
            return
        claimed = lib.inflight_dir(name) / msg_path.name
        with msg_path.open("rb") as stream:
            os.fsync(stream.fileno())
        os.replace(msg_path, claimed)
        lib._sync_directory(claimed.parent)
        lib._sync_directory(msg_path.parent)
        try:
            msg = lib.Message.from_path(claimed)
        except (OSError, ValueError, TypeError, KeyError) as exc:
            _quarantine(name, claimed, f"invalid turn request: {exc}")
            return
        _run_turn(name, msg)
        for output in (lib.transcript_path(name), lib.last_message_path(name)):
            if output.exists():
                with output.open("rb") as stream:
                    os.fsync(stream.fileno())
        os.replace(claimed, destination)
        lib._sync_directory(destination.parent)
        lib._sync_directory(claimed.parent)


def main() -> int:
    """Run ordered queued turns until a stop marker or termination signal arrives."""
    if len(sys.argv) != 2:
        lib.die("usage: agent_runner.py <name>")
    name = sys.argv[1]
    stage_token = os.environ.get("SUBAGENTS_MIGRATION_STAGE")
    signal.signal(signal.SIGTERM, _handle_sigterm)
    lib.ensure_agent_dirs(name)
    if stage_token is None:
        old = lib.read_registry().get(name)
        if old is not None and old.runner_pid is not None:
            if lib.runner_identity_alive(old) and old.runner_pid != os.getpid():
                lib.die(f"worker {name!r} already has a live runner")
            lib.terminate_active_harness(old)
    _record_runner_pid(name, stage_token)
    if stage_token is not None:
        while not _STOP_REQUESTED:
            if lib.stop_path(name).exists():
                _exit_reason(name, "stop marker while awaiting staged activation")
                return 0
            if lib.staged_runner_activate_path(name, stage_token).exists():
                lib.acknowledge_staged_runner_activation(name, stage_token)
                stage_token = None
                break
            if not lib.staged_runner_path(name, stage_token).exists():
                _exit_reason(name, "staged marker removed before activation acknowledgement")
                return 0
            time.sleep(0.05)
    _append(name, f"===RUNNER UP {name} pid={os.getpid()} {lib.now_iso()}===")
    with _intake_lock(name):
        _recover_inflight(name)

    while not _STOP_REQUESTED:
        if lib.stop_path(name).exists():
            break
        if stage_token is None and lib.migration_pause_path(name).exists():
            lib.acknowledge_migration_pause(
                name, lib.RunnerIdentity(pid=os.getpid(), started_at=lib.pid_start_time(os.getpid()))
            )
            time.sleep(0.05)
            continue
        if (lib.agent_dir(name) / "automation-paused").exists():
            time.sleep(0.05)
            continue
        pending = lib.next_pending_message(name)
        if pending is None:
            time.sleep(0.5)
            continue
        _consume(name, pending)

    _append(name, f"===RUNNER DOWN {name} {lib.now_iso()}===")
    _exit_reason(name, "stop marker or SIGTERM after active runner loop")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
