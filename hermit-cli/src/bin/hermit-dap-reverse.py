# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

"""Reverse DAP requests for a Hermit replay managed by hermit-dap.

The first implementation deliberately trades speed for simplicity: invisible
source breakpoints record the executed source-line path, and each reverse
request starts the deterministic replay again and runs forward to an earlier
position on that path.
"""

import importlib
import os
import re
import shutil
import subprocess
import time

import gdb
from gdb.dap.server import capability, request
from gdb.dap.startup import DAPException
from gdb.dap.state import set_thread


server = importlib.import_module("gdb.dap.server")
frames = importlib.import_module("gdb.dap.frames")

# hermit-dap.rs defines both names before it execs this script.
_replay_command = HERMIT_REPLAY_COMMAND  # noqa: F821
_replay_target = HERMIT_REPLAY_TARGET  # noqa: F821
_replay_process = None
_setpriv = None
_history = []
_line_breakpoints = []
_arrival_leaders = {}
_line_program = None
_suppress_events = False
_last_stopped = None


def _unsupported_gdb(problem):
    # Fail at startup, before the client sees an adapter that lacks reverse
    # requests, and say which GDB is wrong rather than leaving a traceback.
    gdb.write(
        "hermit-dap: managed replay does not support this GDB ({}): {}. "
        "It is tested with GDB 17.2; use --gdb to select a GDB whose DAP "
        "interpreter provides these hooks.\n".format(gdb.VERSION, problem),
        gdb.STDERR,
    )
    os._exit(1)


# The reverse requests below rely on these parts of GDB's DAP implementation;
# none of them is a stable public interface, so check them all up front.
_missing_hooks = [
    name
    for name in (
        "send_event",
        "call_function_later",
        "send_gdb",
        "send_gdb_with_response",
        "_commands",
    )
    if not hasattr(server, name)
]
if _missing_hooks or not hasattr(gdb, "with_parameter"):
    _unsupported_gdb(
        "gdb.dap.server lacks {}".format(
            ", ".join(_missing_hooks or ["(gdb.with_parameter)"])
        )
    )
if not callable(getattr(frames, "_clear_frame_ids", None)):
    _unsupported_gdb("gdb.dap.frames lacks _clear_frame_ids")
for _command in ("attach", "disconnect"):
    if _command not in server._commands:
        _unsupported_gdb("gdb.dap has no '{}' request".format(_command))


# GDB's DAP modules do not expose a supported event-suppression interface. Keep
# their handlers connected so they can maintain thread and frame state, but
# hide the process churn caused by an internal replay restart from the client.
#
# Every event leaves through one Server method. GDB 17.2 names it _send_event
# (send_event_maybe_later and the module-level send_event both call it at call
# time); older DAP servers named it send_event. Wrap whichever this GDB has, so
# a renamed sink stops the adapter here instead of leaking restart events.
_event_sink_name = next(
    (
        name
        for name in ("_send_event", "send_event")
        if callable(getattr(server.Server, name, None))
    ),
    None,
)
if _event_sink_name is None:
    _unsupported_gdb("gdb.dap.server.Server has neither _send_event nor send_event")
_original_send_event = getattr(server.Server, _event_sink_name)


def _send_event(self, event, body=None):
    global _last_stopped
    if _suppress_events:
        if event == "stopped":
            _last_stopped = dict(body or {})
        return
    if event == "stopped" and body is not None and "hitBreakpointIds" in body:
        body = dict(body)
        body["hitBreakpointIds"] = [
            breakpoint_id
            for breakpoint_id in body["hitBreakpointIds"]
            if breakpoint_id > 0
        ]
    _original_send_event(self, event, body)


setattr(server.Server, _event_sink_name, _send_event)


def _append_line(pc, file, line, thread_id):
    # One entry per arrival at a line address. Reverse requests count earlier
    # entries with the same pc to choose which hit of a temporary breakpoint
    # to stop at, so an extra or a missing entry moves the target in time.
    _history.append(
        {
            "pc": pc,
            "file": os.path.realpath(file),
            "line": line,
            "thread_id": thread_id,
            "breakpoint": False,
            "breakpoint_ids": [],
        }
    )


def _arrival_leader(pc):
    # GDB often resolves several source lines to one address (a function's
    # opening line and its first statement both land after the prologue) and
    # calls stop() once for each of those breakpoints on a single arrival.
    # Only the lowest-numbered line breakpoint at the address records it.
    if pc not in _arrival_leaders:
        _arrival_leaders[pc] = min(
            (
                breakpoint.number
                for breakpoint in _line_breakpoints
                if breakpoint.is_valid()
                and any(
                    location.address is not None and int(location.address) == pc
                    for location in breakpoint.locations
                )
            ),
            default=None,
        )
    return _arrival_leaders[pc]


class _LineBreakpoint(gdb.Breakpoint):
    def __init__(self, file, line):
        super().__init__("{}:{}".format(file, line), internal=True)
        self.silent = True
        self.file = file
        self.line = line

    def stop(self):
        if not _suppress_events:
            try:
                pc = int(gdb.newest_frame().pc())
                if _arrival_leader(pc) in (None, self.number):
                    _append_line(
                        pc,
                        self.file,
                        self.line,
                        gdb.selected_thread().global_num,
                    )
            except (gdb.error, AttributeError):
                pass
        return False


def _install_line_breakpoints(event=None):
    global _line_program
    global _suppress_events

    program = gdb.current_progspace().filename
    if program is None or program == _line_program or not os.path.isfile(program):
        return

    try:
        decoded = subprocess.run(
            # Without --wide, readelf truncates long file names to a fixed
            # column, and no line breakpoint can be set from a truncated name.
            ["readelf", "--wide", "--debug-dump=decodedline", program],
            check=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,
            text=True,
        ).stdout
    except (OSError, subprocess.CalledProcessError) as error:
        raise gdb.error("failed to read source-line information: {}".format(error))

    source_lines = set()
    for row in decoded.splitlines():
        match = re.match(r"^\s*(.*?)\s+(\d+)\s+0x[0-9a-fA-F]+(?:\s|$)", row)
        if match is not None:
            source_lines.add((os.path.realpath(match.group(1)), int(match.group(2))))

    previous_suppression = _suppress_events
    _suppress_events = True
    _arrival_leaders.clear()
    try:
        for file, line in sorted(source_lines):
            try:
                _line_breakpoints.append(_LineBreakpoint(file, line))
            except gdb.error:
                pass
    finally:
        _suppress_events = previous_suppression
    _line_program = program


def _source_position(event):
    try:
        frame = gdb.newest_frame()
        sal = frame.find_sal()
        if sal.symtab is None or sal.line <= 0:
            return None
        breakpoint_ids = []
        if isinstance(event, gdb.BreakpointEvent):
            breakpoint_ids = [
                breakpoint.number
                for breakpoint in event.breakpoints
                if breakpoint.visible
            ]
        return {
            "pc": int(frame.pc()),
            "file": os.path.realpath(sal.symtab.fullname()),
            "line": sal.line,
            "thread_id": gdb.selected_thread().global_num,
            "breakpoint": bool(breakpoint_ids),
            "breakpoint_ids": breakpoint_ids,
        }
    except (gdb.error, AttributeError):
        return None


def _remember_stop(event):
    if _suppress_events:
        return

    position = _source_position(event)
    if position is None:
        return
    # A stop at the address a line breakpoint just recorded is that same
    # arrival, even when the recording breakpoint names another source line
    # that resolves to the same address.
    if (
        _history
        and _history[-1]["pc"] == position["pc"]
        and _history[-1]["thread_id"] == position["thread_id"]
    ):
        _history[-1].update(position)
    else:
        _history.append(position)


def _wait_for_replay(timeout=5.0):
    global _replay_process
    if _replay_process is None:
        return
    try:
        _replay_process.wait(timeout=timeout)
    except subprocess.TimeoutExpired:
        _replay_process.kill()
        _replay_process.wait()
    _replay_process = None


def _terminate_replay():
    if _replay_process is None:
        return
    _replay_process.kill()
    _wait_for_replay(0)


def _start_replay():
    global _replay_process
    _replay_process = subprocess.Popen(
        [_setpriv, "--pdeathsig", "SIGKILL", "--"] + _replay_command,
        stdin=subprocess.DEVNULL,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
        close_fds=True,
    )
    time.sleep(0.5)
    status = _replay_process.poll()
    if status is not None:
        _replay_process = None
        raise gdb.error("Hermit replay exited before GDB attached: {}".format(status))


def _connect_replay():
    last_error = None
    for _ in range(200):
        try:
            gdb.execute(
                "target remote " + _replay_target, from_tty=False, to_string=True
            )
            return
        except gdb.error as error:
            last_error = error
            time.sleep(0.05)
    raise gdb.error("Hermit replay did not accept a GDB connection: " + str(last_error))


def _restart_at(position, occurrence, reason):
    global _last_stopped
    global _suppress_events

    disabled_breakpoints = []
    target = None
    _last_stopped = None
    _suppress_events = True
    try:
        disabled_breakpoints = [
            breakpoint
            for breakpoint in (gdb.breakpoints() or [])
            if breakpoint.is_valid()
            and breakpoint.enabled
            and (breakpoint.visible or isinstance(breakpoint, _LineBreakpoint))
        ]
        for breakpoint in disabled_breakpoints:
            breakpoint.enabled = False

        with gdb.with_parameter("confirm", False):
            try:
                gdb.execute("kill", from_tty=False, to_string=True)
            except gdb.error:
                pass
        _wait_for_replay()
        try:
            gdb.execute("disconnect", from_tty=False, to_string=True)
        except gdb.error:
            pass

        _start_replay()
        _connect_replay()
        # GDB's DAP layer caches frames per thread and drops the cache only
        # when the inferior resumes. A restart that stops at the replay's
        # entry never resumes, so drop the killed inferior's frames here.
        frames._clear_frame_ids(None)

        if position is not None:
            target = gdb.Breakpoint(
                "*{:#x}".format(position["pc"]),
                type=gdb.BP_BREAKPOINT,
                internal=True,
                temporary=True,
            )
            target.ignore_count = max(0, occurrence - 1)
            gdb.execute("continue", from_tty=False, to_string=True)
            if int(gdb.newest_frame().pc()) != position["pc"]:
                raise gdb.error("replay did not stop at the requested source position")

        body = dict(_last_stopped or {})
        body["reason"] = reason
        body["threadId"] = gdb.selected_thread().global_num
        body["allThreadsStopped"] = True
        if reason == "breakpoint" and position is not None:
            body["hitBreakpointIds"] = position["breakpoint_ids"]
        else:
            body.pop("hitBreakpointIds", None)
        return body
    finally:
        if target is not None and target.is_valid():
            target.delete()
        for breakpoint in disabled_breakpoints:
            if breakpoint.is_valid():
                breakpoint.enabled = True
        _suppress_events = False


def _report_reverse_failure(operation, error):
    server.send_event(
        "output",
        {
            "category": "stderr",
            "output": "hermit-dap: {} failed: {}\n".format(operation, error),
        },
    )
    server.send_event("terminated")


def _step_back(thread_id, granularity):
    if granularity != "statement":
        raise DAPException("Hermit stepBack currently supports statement granularity")
    set_thread(thread_id)
    matching = [
        index
        for index, position in enumerate(_history)
        if position["thread_id"] == thread_id
    ]
    target_index = matching[-2] if len(matching) >= 2 else -1
    position = _history[target_index] if target_index >= 0 else None
    occurrence = (
        sum(
            1 for entry in _history[: target_index + 1] if entry["pc"] == position["pc"]
        )
        if position is not None
        else 0
    )
    body = _restart_at(position, occurrence, "step")
    if target_index >= 0:
        del _history[target_index + 1 :]
    else:
        _history.clear()
    server.send_event("stopped", body)


@capability("supportsStepBack")
@request("stepBack", on_dap_thread=True)
def step_back(
    *, threadId: int, singleThread: bool = False, granularity: str = "statement", **args
):
    if singleThread:
        raise DAPException("Hermit reverse execution restarts the whole replay")

    def run():
        try:
            _step_back(threadId, granularity)
        except Exception as error:
            _report_reverse_failure("stepBack", error)

    server.call_function_later(lambda: server.send_gdb(run))


def _visible_breakpoints_by_pc():
    result = {}
    for breakpoint in gdb.breakpoints() or []:
        if (
            not breakpoint.is_valid()
            or not breakpoint.enabled
            or not breakpoint.visible
        ):
            continue
        for location in breakpoint.locations:
            if location.enabled and location.address is not None:
                result.setdefault(int(location.address), []).append(breakpoint.number)
    return result


def _reverse_continue(thread_id):
    set_thread(thread_id)
    breakpoints = _visible_breakpoints_by_pc()
    target_index = next(
        (
            index
            for index in range(len(_history) - 2, -1, -1)
            if _history[index]["pc"] in breakpoints
        ),
        -1,
    )
    position = dict(_history[target_index]) if target_index >= 0 else None
    if position is not None:
        position["breakpoint_ids"] = breakpoints[position["pc"]]
    occurrence = (
        sum(
            1 for entry in _history[: target_index + 1] if entry["pc"] == position["pc"]
        )
        if position is not None
        else 0
    )
    body = _restart_at(
        position, occurrence, "breakpoint" if position is not None else "entry"
    )
    if target_index >= 0:
        del _history[target_index + 1 :]
    else:
        _history.clear()
    server.send_event("stopped", body)


@request("reverseContinue", on_dap_thread=True)
def reverse_continue(*, threadId: int, singleThread: bool = False, **args):
    if singleThread:
        raise DAPException("Hermit reverse execution restarts the whole replay")

    def run():
        try:
            _reverse_continue(threadId)
        except Exception as error:
            _report_reverse_failure("reverseContinue", error)

    server.call_function_later(lambda: server.send_gdb(run))


def _kill_replay_for_disconnect():
    global _suppress_events
    previous_suppression = _suppress_events
    _suppress_events = True
    try:
        with gdb.with_parameter("confirm", False):
            try:
                gdb.execute("kill", from_tty=False, to_string=True)
            except gdb.error:
                pass
        _wait_for_replay()
    finally:
        _suppress_events = previous_suppression


_original_attach = server._commands["attach"]
_original_disconnect = server._commands["disconnect"]


def _attach(**args):
    if args.get("target") != _replay_target:
        raise DAPException(
            "managed replay requires DAP target {}".format(_replay_target)
        )
    return _original_attach(**args)


def _disconnect(**args):
    try:
        server.send_gdb_with_response(_kill_replay_for_disconnect)
    except Exception:
        pass
    args["terminateDebuggee"] = False
    return _original_disconnect(**args)


server._commands["attach"] = _attach
server._commands["disconnect"] = _disconnect


def _cleanup(event):
    _terminate_replay()


_setpriv = (
    "/usr/bin/setpriv"
    if os.path.isfile("/usr/bin/setpriv")
    else shutil.which("setpriv")
)
if shutil.which("readelf") is None or _setpriv is None:
    gdb.write(
        "hermit-dap: managed replay requires readelf and setpriv\n",
        gdb.STDERR,
    )
    os._exit(1)

gdb.events.new_objfile.connect(_install_line_breakpoints)
gdb.events.stop.connect(_remember_stop)
gdb.events.gdb_exiting.connect(_cleanup)
try:
    _start_replay()
except Exception as error:
    gdb.write("hermit-dap: failed to start replay: {}\n".format(error), gdb.STDERR)
    os._exit(1)
