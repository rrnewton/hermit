#!/usr/bin/env python3
"""Turn a provider event stream into contentless vibe-talk live hints.

The upstream command is a trust boundary: it must emit only message events for the one channel
named by ``--channel`` and suppress heartbeats.  This adapter deliberately does not interpret an
event or forward any of its fields.  Every nonblank JSON-object line causes exactly one
``POST /api/v1/live/hints`` whose entire body is ``{"channel_id": "..."}``.  vibe-talk then reads
the allowlisted channel through its configured, read-only provider and cursor-deduplicates the
result.

The command after the literal ``--`` is executed directly, never by a shell.  Its stderr is
discarded and its stdout is accepted one bounded line at a time so event content and credentials
cannot enter this adapter's logs.
"""

from __future__ import annotations

import argparse
import contextlib
import http.client
import json
import os
from pathlib import Path
import signal
import ssl
import stat
import subprocess
import sys
import tempfile
import threading
import time
from collections import deque
from collections.abc import Callable, Iterator, Sequence
from dataclasses import dataclass
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from types import FrameType
from typing import NoReturn, Protocol, TypeAlias, cast
from urllib.parse import SplitResult, urlsplit


EXIT_OK = 0
EXIT_USAGE = 2
EXIT_UPSTREAM_PROTOCOL = 10
EXIT_UPSTREAM_FAILED = 11
EXIT_DELIVERY_FAILED = 12
EXIT_CONTROL_FAILED = 17

MAX_EVENT_LINE_BYTES = 64 * 1024
MAX_TOKEN_FILE_BYTES = 4096
MIN_TOKEN_BYTES = 24
REQUEST_TIMEOUT_SECONDS = 10.0
CHILD_EXIT_GRACE_SECONDS = 2.0
CHILD_TERM_GRACE_SECONDS = 2.0
RETRY_DELAYS_SECONDS = (0.25, 0.5, 1.0, 2.0)
HINT_PATH = "/api/v1/live/hints"

Log = Callable[[str], None]
StopCheck = Callable[[], bool]
Sleep = Callable[[float], None]
Transport = Callable[[str, str, bytes, float], int]


class AdapterFailure(Exception):
    """An expected operator-facing failure with a stable exit code and contentless message."""

    def __init__(self, code: int, message: str) -> None:
        super().__init__(message)
        self.code = code
        self.message = message


class StopRequested(Exception):
    """Internal control flow after SIGINT or SIGTERM."""


class RetryableDelivery(Exception):
    """One delivery failed in a way for which retry is safe."""

    def __init__(self, status: int | None) -> None:
        super().__init__()
        self.status = status


class HintSender(Protocol):
    """The narrow interface the stream loop needs from the HTTP client."""

    def post(self, stopping: StopCheck) -> int:
        """Post one contentless hint and return its successful HTTP status."""


def stderr_log(message: str) -> None:
    """Emit one already-sanitized operator message."""

    print(f"live-hint-adapter: {message}", file=sys.stderr, flush=True)


def parse_base_url(raw: str) -> str:
    """Validate an origin-only HTTP(S) base URL and return its hint endpoint."""

    try:
        parts = urlsplit(raw)
        # Accessing port performs urllib's bracket/range validation.
        _ = parts.port
    except ValueError as exc:
        raise AdapterFailure(EXIT_USAGE, "--url is not a valid HTTP(S) origin") from exc
    if parts.scheme not in {"http", "https"} or not parts.hostname:
        raise AdapterFailure(EXIT_USAGE, "--url must be an HTTP(S) origin")
    if parts.username is not None or parts.password is not None:
        raise AdapterFailure(EXIT_USAGE, "--url must not contain credentials")
    if parts.path not in {"", "/"} or parts.query or parts.fragment:
        raise AdapterFailure(EXIT_USAGE, "--url must be an origin without a path, query, or fragment")
    if parts.scheme == "http" and parts.hostname not in {"127.0.0.1", "::1", "localhost"}:
        raise AdapterFailure(EXIT_USAGE, "plain HTTP is allowed only for a loopback --url")
    return f"{parts.scheme}://{parts.netloc}{HINT_PATH}"


def validate_channel(raw: str) -> str:
    """Validate the fixed opaque channel id without imposing provider-specific syntax."""

    encoded = raw.encode("utf-8")
    if not raw or raw != raw.strip() or len(encoded) > 512:
        raise AdapterFailure(EXIT_USAGE, "--channel must be 1 to 512 UTF-8 bytes with no edge whitespace")
    if any(ord(character) < 0x20 or ord(character) == 0x7F for character in raw):
        raise AdapterFailure(EXIT_USAGE, "--channel must not contain control characters")
    return raw


def read_private_token(path: Path) -> str:
    """Read a bounded regular token file without following a final symlink."""

    flags = os.O_RDONLY | getattr(os, "O_CLOEXEC", 0) | getattr(os, "O_NOFOLLOW", 0)
    try:
        descriptor = os.open(path, flags)
    except OSError as exc:
        raise AdapterFailure(EXIT_USAGE, "cannot open --token-file as a private regular file") from exc
    try:
        metadata = os.fstat(descriptor)
        if not stat.S_ISREG(metadata.st_mode):
            raise AdapterFailure(EXIT_USAGE, "--token-file must be a regular file")
        if metadata.st_uid != os.geteuid():
            raise AdapterFailure(EXIT_USAGE, "--token-file must be owned by the current effective user")
        if metadata.st_nlink != 1:
            raise AdapterFailure(EXIT_USAGE, "--token-file must have exactly one hard link")
        if metadata.st_mode & 0o077:
            raise AdapterFailure(EXIT_USAGE, "--token-file must not be accessible by group or other")
        with os.fdopen(descriptor, "rb", closefd=False) as source:
            raw = source.read(MAX_TOKEN_FILE_BYTES + 1)
    finally:
        os.close(descriptor)
    if len(raw) > MAX_TOKEN_FILE_BYTES:
        raise AdapterFailure(EXIT_USAGE, f"--token-file exceeds {MAX_TOKEN_FILE_BYTES} bytes")
    try:
        text = raw.decode("utf-8")
    except UnicodeDecodeError as exc:
        raise AdapterFailure(EXIT_USAGE, "--token-file is not UTF-8") from exc
    if text.endswith("\n"):
        text = text[:-1]
        if text.endswith("\r"):
            text = text[:-1]
    token = text
    token_bytes = token.encode("utf-8")
    if len(token_bytes) < MIN_TOKEN_BYTES or any(
        ord(character) < 0x21 or ord(character) > 0x7E for character in token
    ):
        raise AdapterFailure(
            EXIT_USAGE,
            f"--token-file must contain one printable ASCII token of at least {MIN_TOKEN_BYTES} bytes",
        )
    return token


def open_connection(parts: SplitResult, timeout: float) -> http.client.HTTPConnection:
    """Create one request-scoped connection; http.client never follows redirects."""

    hostname = parts.hostname
    if hostname is None:  # Guard for callers other than parse_base_url.
        raise RetryableDelivery(None)
    port = parts.port
    if parts.scheme == "https":
        return http.client.HTTPSConnection(
            hostname,
            port=port,
            timeout=timeout,
            context=ssl.create_default_context(),
        )
    return http.client.HTTPConnection(hostname, port=port, timeout=timeout)


def post_once(endpoint: str, token: str, body: bytes, timeout: float) -> int:
    """Make one bounded contentless request and return its status without reading its body."""

    parts = urlsplit(endpoint)
    connection: http.client.HTTPConnection | None = None
    try:
        connection = open_connection(parts, timeout)
        connection.request(
            "POST",
            parts.path,
            body=body,
            headers={
                "Authorization": f"Bearer {token}",
                "Content-Type": "application/json",
                "Content-Length": str(len(body)),
                "Accept": "application/json",
            },
        )
        response = connection.getresponse()
        try:
            return response.status
        finally:
            response.close()
    except (OSError, http.client.HTTPException) as exc:
        raise RetryableDelivery(None) from exc
    finally:
        if connection is not None:
            connection.close()


@dataclass(frozen=True)
class RetryPolicy:
    """A finite retry schedule; attempts equal one plus the number of delays."""

    delays: tuple[float, ...] = RETRY_DELAYS_SECONDS


class HttpHintSender:
    """Authenticated client for the contentless hint endpoint."""

    def __init__(
        self,
        endpoint: str,
        channel: str,
        token: str,
        log: Log,
        *,
        timeout: float = REQUEST_TIMEOUT_SECONDS,
        policy: RetryPolicy = RetryPolicy(),
        sleep: Sleep = time.sleep,
        transport: Transport = post_once,
    ) -> None:
        self.endpoint = endpoint
        self.token = token
        self.log = log
        self.timeout = timeout
        self.policy = policy
        self.sleep = sleep
        self.transport = transport
        # This immutable body is the privacy property: upstream fields have nowhere to enter it.
        self.body = json.dumps(
            {"channel_id": channel}, separators=(",", ":"), ensure_ascii=False
        ).encode("utf-8")

    def post(self, stopping: StopCheck) -> int:
        """Post once, retrying only transport failures, 429, and 5xx."""

        attempts = len(self.policy.delays) + 1
        last_retryable_status: int | None = None
        for attempt in range(1, attempts + 1):
            if stopping():
                raise StopRequested
            status: int | None
            try:
                status = self.transport(
                    self.endpoint,
                    self.token,
                    self.body,
                    self.timeout,
                )
            except RetryableDelivery as exc:
                status = exc.status
            if status == 200 or status == 202:
                return status
            if status == 429 or (status is not None and 500 <= status <= 599):
                last_retryable_status = status
            elif status is None:
                last_retryable_status = None
            else:
                raise AdapterFailure(
                    EXIT_DELIVERY_FAILED,
                    f"hint endpoint permanently refused a request with HTTP {status}",
                )
            if attempt == attempts:
                break
            delay = self.policy.delays[attempt - 1]
            result = (
                "a transport failure"
                if last_retryable_status is None
                else f"retryable HTTP {last_retryable_status}"
            )
            self.log(f"hint delivery attempt {attempt}/{attempts} had {result}; retrying")
            self.sleep(delay)
        result = (
            "transport failure"
            if last_retryable_status is None
            else f"HTTP {last_retryable_status}"
        )
        raise AdapterFailure(
            EXIT_DELIVERY_FAILED,
            f"hint delivery failed after {attempts} attempts; last result was {result}",
        )


def reject_nonstandard_json_constant(value: str) -> NoReturn:
    """Reject NaN and infinities, which json.loads otherwise accepts as extensions."""

    raise ValueError(f"nonstandard constant {value}")


def require_json_object(raw: bytes, line_number: int) -> None:
    """Validate one line without retaining or returning its potentially sensitive fields."""

    try:
        decoded = raw.decode("utf-8")
        value: object = json.loads(decoded, parse_constant=reject_nonstandard_json_constant)
    except (UnicodeDecodeError, ValueError, RecursionError) as exc:
        raise AdapterFailure(
            EXIT_UPSTREAM_PROTOCOL,
            f"upstream line {line_number} is not a valid JSON object",
        ) from exc
    if not isinstance(value, dict):
        raise AdapterFailure(
            EXIT_UPSTREAM_PROTOCOL,
            f"upstream line {line_number} is valid JSON but not an object",
        )


class ShutdownController:
    """Signal state shared by the handler, stream loop, and retry loop."""

    def __init__(self) -> None:
        self.signal_number: int | None = None
        self.child: subprocess.Popen[bytes] | None = None

    def handler(self, signum: int, _frame: FrameType | None) -> None:
        """Record the first signal and unblock a stdout read by terminating the child group."""

        if self.signal_number is None:
            self.signal_number = signum
        child = self.child
        if child is not None:
            signal_process_group(child.pid, signal.SIGTERM)

    def attach(self, child: subprocess.Popen[bytes]) -> None:
        """Attach a child and honor a signal that arrived just before it started."""

        self.child = child
        if self.signal_number is not None:
            signal_process_group(child.pid, signal.SIGTERM)

    def detach(self) -> None:
        """Forget the child after it has been reaped."""

        self.child = None

    def stopping(self) -> bool:
        """Whether SIGINT or SIGTERM has requested shutdown."""

        return self.signal_number is not None


def signal_process_group(group: int, requested_signal: signal.Signals) -> None:
    """Signal an owned child process group, tolerating a group that already exited."""

    try:
        os.killpg(group, requested_signal)
    except OSError:
        pass


def process_group_exists(group: int) -> bool:
    """Whether any process still has the owned child's process-group id."""

    try:
        os.killpg(group, 0)
    except ProcessLookupError:
        return False
    except PermissionError:
        return True
    return True


def clean_child(child: subprocess.Popen[bytes]) -> None:
    """Terminate the entire owned group and reap its leader, escalating after a bound."""

    signal_process_group(child.pid, signal.SIGTERM)
    try:
        child.wait(timeout=CHILD_TERM_GRACE_SECONDS)
    except subprocess.TimeoutExpired:
        pass
    deadline = time.monotonic() + CHILD_TERM_GRACE_SECONDS
    while process_group_exists(child.pid) and time.monotonic() < deadline:
        time.sleep(0.02)
    if process_group_exists(child.pid):
        signal_process_group(child.pid, signal.SIGKILL)
    try:
        child.wait(timeout=CHILD_TERM_GRACE_SECONDS)
    except subprocess.TimeoutExpired as exc:
        raise AdapterFailure(EXIT_UPSTREAM_FAILED, "could not reap the upstream command") from exc


@contextlib.contextmanager
def installed_signal_handlers(controller: ShutdownController) -> Iterator[None]:
    """Install and then restore SIGINT/SIGTERM handlers in the main thread."""

    previous_int = signal.getsignal(signal.SIGINT)
    previous_term = signal.getsignal(signal.SIGTERM)
    signal.signal(signal.SIGINT, controller.handler)
    signal.signal(signal.SIGTERM, controller.handler)
    try:
        yield
    finally:
        signal.signal(signal.SIGINT, previous_int)
        signal.signal(signal.SIGTERM, previous_term)


def run_upstream(
    command: Sequence[str], sender: HintSender, controller: ShutdownController, log: Log
) -> NoReturn:
    """Consume one bounded line at a time and synchronously settle its one hint request."""

    try:
        child = subprocess.Popen(
            list(command),
            stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,
            start_new_session=True,
            close_fds=True,
        )
    except OSError as exc:
        raise AdapterFailure(EXIT_UPSTREAM_FAILED, "could not start the upstream command") from exc
    controller.attach(child)
    line_number = 0
    event_count = 0
    try:
        stdout = child.stdout
        if stdout is None:  # Popen contract above always creates this pipe.
            raise AdapterFailure(EXIT_UPSTREAM_FAILED, "upstream stdout pipe was not created")
        while True:
            raw = stdout.readline(MAX_EVENT_LINE_BYTES + 2)
            if controller.stopping():
                raise StopRequested
            if not raw:
                break
            line_number += 1
            line = raw.removesuffix(b"\n").removesuffix(b"\r")
            if len(line) > MAX_EVENT_LINE_BYTES:
                raise AdapterFailure(
                    EXIT_UPSTREAM_PROTOCOL,
                    f"upstream line {line_number} exceeds {MAX_EVENT_LINE_BYTES} bytes",
                )
            candidate = line.strip()
            if not candidate:
                continue
            require_json_object(candidate, line_number)
            status = sender.post(controller.stopping)
            if controller.stopping():
                raise StopRequested
            event_count += 1
            log(f"hint {event_count} accepted with HTTP {status}")
        if controller.stopping():
            raise StopRequested
        try:
            return_code = child.wait(timeout=CHILD_EXIT_GRACE_SECONDS)
        except subprocess.TimeoutExpired as exc:
            raise AdapterFailure(
                EXIT_UPSTREAM_FAILED,
                "upstream closed stdout but did not exit",
            ) from exc
        if controller.stopping():
            raise StopRequested
        if return_code == 0:
            raise AdapterFailure(
                EXIT_UPSTREAM_FAILED,
                f"upstream event stream ended unexpectedly after {event_count} event(s)",
            )
        raise AdapterFailure(
            EXIT_UPSTREAM_FAILED,
            f"upstream command exited with status {return_code}",
        )
    finally:
        clean_child(child)
        controller.detach()


class RecordingHintSender:
    """Content-free test double used by the offline controls."""

    def __init__(self, after_post: Callable[[int], None] | None = None) -> None:
        self.calls = 0
        self.after_post = after_post

    def post(self, stopping: StopCheck) -> int:
        if stopping():
            raise StopRequested
        self.calls += 1
        if self.after_post is not None:
            self.after_post(self.calls)
        return 202


RequestRecord: TypeAlias = tuple[str, str, bytes]


class RecordingServer(ThreadingHTTPServer):
    """Loopback-only HTTP fixture with a deterministic response queue."""

    records: list[RequestRecord]
    statuses: deque[int]

    def __init__(self, statuses: Sequence[int]) -> None:
        super().__init__(("127.0.0.1", 0), RecordingHandler)
        self.records = []
        self.statuses = deque(statuses)


class RecordingHandler(BaseHTTPRequestHandler):
    """Record only what the adapter sends; never print requests or headers."""

    server: RecordingServer

    def do_POST(self) -> None:
        length_text = self.headers.get("Content-Length", "0")
        try:
            length = int(length_text)
        except ValueError:
            length = 0
        body = self.rfile.read(max(0, length))
        authorization = self.headers.get("Authorization", "")
        self.server.records.append((self.path, authorization, body))
        status = self.server.statuses.popleft() if self.server.statuses else 500
        payload = b'{"accepted":true,"duplicate":false}'
        self.send_response(status)
        if status == 302:
            self.send_header("Location", "/must-not-follow")
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(payload)))
        self.end_headers()
        with contextlib.suppress(BrokenPipeError, ConnectionResetError):
            self.wfile.write(payload)

    def log_message(self, _format: str, *_args: object) -> None:
        pass


@contextlib.contextmanager
def recording_server(statuses: Sequence[int]) -> Iterator[tuple[RecordingServer, str]]:
    """Run the loopback HTTP fixture and stop it deterministically."""

    server = RecordingServer(statuses)
    host, port = cast(tuple[str, int], server.server_address)
    thread = threading.Thread(target=server.serve_forever, name="hint-adapter-self-test", daemon=True)
    thread.start()
    try:
        yield server, f"http://{host}:{port}"
    finally:
        server.shutdown()
        server.server_close()
        thread.join(timeout=2.0)
        if thread.is_alive():
            raise AssertionError("HTTP fixture did not stop")


def expect_failure(code: int, action: Callable[[], object]) -> AdapterFailure:
    """Run one negative control and require its exact failure class and code."""

    try:
        action()
    except AdapterFailure as exc:
        if exc.code != code:
            raise AssertionError(f"expected exit {code}, got {exc.code}") from exc
        return exc
    raise AssertionError(f"expected exit {code}, action passed")


def self_test() -> int:
    """Run deterministic, offline controls over framing, privacy, retry, and cleanup."""

    controls = 0
    secret = "self-test-secret-token-value"
    content_marker = "MESSAGE-CONTENT-MUST-NOT-LEAK"

    # The real HTTP transport sends exactly the contentless body, retries only retryable statuses,
    # and neither redirects nor places the credential or upstream marker in its logs.
    with recording_server([500, 429, 202]) as (server, base_url):
        endpoint = parse_base_url(base_url)
        logs: list[str] = []
        sleeps: list[float] = []
        sender = HttpHintSender(
            endpoint,
            "fixed-channel",
            secret,
            logs.append,
            policy=RetryPolicy((0.0, 0.0)),
            sleep=sleeps.append,
        )
        if sender.post(lambda: False) != 202:
            raise AssertionError("202 was not accepted")
        expected_body = b'{"channel_id":"fixed-channel"}'
        if len(server.records) != 3 or any(
            record != (HINT_PATH, f"Bearer {secret}", expected_body) for record in server.records
        ):
            raise AssertionError("HTTP request was not the exact contentless contract")
        if sleeps != [0.0, 0.0]:
            raise AssertionError("retry schedule was not followed")
        if secret in "\n".join(logs) or content_marker in "\n".join(logs):
            raise AssertionError("a secret or event marker entered retry logs")
    controls += 1

    with recording_server([200]) as (server, base_url):
        endpoint = parse_base_url(base_url)
        http_sender = HttpHintSender(endpoint, "fixed-channel", secret, lambda _line: None)
        if http_sender.post(lambda: False) != 200 or len(server.records) != 1:
            raise AssertionError("200 coalesced response was not accepted")
    controls += 1

    for permanent_status in (302, 400, 401, 404):
        with recording_server([permanent_status, 202]) as (server, base_url):
            endpoint = parse_base_url(base_url)
            http_sender = HttpHintSender(endpoint, "fixed-channel", secret, lambda _line: None)
            expect_failure(EXIT_DELIVERY_FAILED, lambda: http_sender.post(lambda: False))
            if len(server.records) != 1:
                raise AssertionError(f"HTTP {permanent_status} was retried or redirected")
    controls += 1

    with recording_server([503, 503, 503]) as (server, base_url):
        endpoint = parse_base_url(base_url)
        http_sender = HttpHintSender(
            endpoint,
            "fixed-channel",
            secret,
            lambda _line: None,
            policy=RetryPolicy((0.0, 0.0)),
            sleep=lambda _delay: None,
        )
        expect_failure(EXIT_DELIVERY_FAILED, lambda: http_sender.post(lambda: False))
        if len(server.records) != 3:
            raise AssertionError("retry exhaustion did not use the finite attempt bound")
    controls += 1

    transport_calls = 0

    def flaky_transport(_endpoint: str, _token: str, _body: bytes, _timeout: float) -> int:
        nonlocal transport_calls
        transport_calls += 1
        if transport_calls < 3:
            raise RetryableDelivery(None)
        return 202

    transport_logs: list[str] = []
    transport_sender = HttpHintSender(
        "http://127.0.0.1:1/api/v1/live/hints",
        "fixed-channel",
        secret,
        transport_logs.append,
        policy=RetryPolicy((0.0, 0.0)),
        sleep=lambda _delay: None,
        transport=flaky_transport,
    )
    if transport_sender.post(lambda: False) != 202 or transport_calls != 3:
        raise AssertionError("transport failures were not retried to a later success")
    if not all("transport failure" in line for line in transport_logs):
        raise AssertionError("transport retry diagnostics lost their safe failure class")
    controls += 1

    # Exercise the actual no-shell subprocess path. Blank lines are ignored, two arbitrary JSON
    # objects each trigger one request, and no content is logged. The second post requests shutdown,
    # proving that signal cleanup unblocks the read and reaps the owned child process group.
    with tempfile.TemporaryDirectory(prefix="live-hint-adapter-test-") as temporary:
        sentinel = Path(temporary) / "shell-was-used"
        literal = f"; touch {sentinel} ; {content_marker}"
        program = (
            "import json,sys; "
            "print(); "
            "print(json.dumps({'payload':sys.argv[1]})); "
            "print(json.dumps({'second':True})); "
            "sys.stdout.flush(); "
            "__import__('time').sleep(30)"
        )
        logs = []
        controller = ShutdownController()
        recording_sender = RecordingHintSender(
            lambda count: controller.handler(signal.SIGTERM, None) if count == 2 else None
        )
        try:
            run_upstream(
                [sys.executable, "-c", program, literal],
                recording_sender,
                controller,
                logs.append,
            )
        except StopRequested:
            pass
        else:
            raise AssertionError("requested shutdown did not stop the upstream loop")
        if recording_sender.calls != 2 or sentinel.exists() or controller.child is not None:
            raise AssertionError("literal argv or one-object/one-hint behavior failed")
        rendered = "\n".join(logs)
        if secret in rendered or content_marker in rendered or literal in rendered:
            raise AssertionError("event content entered adapter logs")
    controls += 1

    clean_sender = RecordingHintSender()
    clean_failure = expect_failure(
        EXIT_UPSTREAM_FAILED,
        lambda: run_upstream(
            [sys.executable, "-c", "print('{}')"],
            clean_sender,
            ShutdownController(),
            lambda _line: None,
        ),
    )
    if clean_sender.calls != 1 or "ended unexpectedly" not in clean_failure.message:
        raise AssertionError("a clean event-stream EOF was not diagnosed as a dead source")
    controls += 1

    for invalid_output in (b"[]\n", b"not-json\n", b'{"value": NaN}\n'):
        recording_sender = RecordingHintSender()
        controller = ShutdownController()
        encoded = invalid_output.hex()
        # The long sleep proves failure cleanup terminates and reaps the child instead of waiting
        # for upstream cooperation.
        program = (
            "import sys,time; "
            f"sys.stdout.buffer.write(bytes.fromhex('{encoded}')); "
            "sys.stdout.flush(); time.sleep(30)"
        )
        started = time.monotonic()
        expect_failure(
            EXIT_UPSTREAM_PROTOCOL,
            lambda: run_upstream(
                [sys.executable, "-c", program],
                recording_sender,
                controller,
                lambda _line: None,
            ),
        )
        if time.monotonic() - started >= CHILD_TERM_GRACE_SECONDS:
            raise AssertionError("invalid input did not promptly clean up the child")
        if recording_sender.calls:
            raise AssertionError("invalid input triggered a hint")
    controls += 1

    oversized_program = (
        f"import sys; sys.stdout.write('{{' + 'x' * {MAX_EVENT_LINE_BYTES + 1} + '\\n'); "
        "sys.stdout.flush()"
    )
    expect_failure(
        EXIT_UPSTREAM_PROTOCOL,
        lambda: run_upstream(
            [sys.executable, "-c", oversized_program],
            RecordingHintSender(),
            ShutdownController(),
            lambda _line: None,
        ),
    )
    controls += 1

    with tempfile.TemporaryDirectory(prefix="live-hint-adapter-token-test-") as temporary:
        token_path = Path(temporary) / "token"
        token_path.write_text(secret + "\n", encoding="utf-8")
        token_path.chmod(0o600)
        if read_private_token(token_path) != secret:
            raise AssertionError("private token file did not round-trip")
        token_alias = Path(temporary) / "token-alias"
        os.link(token_path, token_alias)
        expect_failure(EXIT_USAGE, lambda: read_private_token(token_path))
        token_alias.unlink()
        token_path.chmod(0o644)
        expect_failure(EXIT_USAGE, lambda: read_private_token(token_path))
    controls += 1

    print(f"live-hint-adapter self-test: PASS ({controls} controls)")
    return EXIT_OK


def build_parser() -> argparse.ArgumentParser:
    """Build the operator-side parser; upstream argv is split off before this parser runs."""

    parser = argparse.ArgumentParser(
        usage=(
            "%(prog)s --url URL --channel ID --token-file FILE -- COMMAND [ARG ...]\n"
            "       %(prog)s --self-test"
        ),
        description=(
            "Convert one trusted JSON-lines message-event stream into contentless vibe-talk live "
            "hints. Every nonblank JSON object triggers one hint; filter message events and "
            "heartbeats in the upstream command."
        ),
        epilog=(
            "Example: live-hint-adapter.py --url http://127.0.0.1:8080 --channel C0123 "
            "--token-file /run/credentials/hint-token -- provider-events --json-lines C0123. "
            "Run --self-test for deterministic offline controls."
        ),
        allow_abbrev=False,
    )
    parser.add_argument(
        "--url",
        required=True,
        help=(
            "vibe-talk HTTP(S) origin, with no path (plain HTTP is accepted only on loopback); "
            f"hints are posted to {HINT_PATH}"
        ),
    )
    parser.add_argument(
        "--channel",
        required=True,
        help=(
            "one allowlisted provider channel id; this fixed value is the only event-derived "
            "identity sent to vibe-talk"
        ),
    )
    parser.add_argument(
        "--token-file",
        required=True,
        type=Path,
        help=(
            "private (mode 0600 or stricter) regular file containing the hint bearer token; "
            "the token is never accepted on argv or printed"
        ),
    )
    return parser


def split_operator_and_upstream(argv: Sequence[str]) -> tuple[list[str], list[str]]:
    """Require a literal separator so upstream flags cannot be consumed as adapter flags."""

    try:
        separator = argv.index("--")
    except ValueError as exc:
        raise AdapterFailure(EXIT_USAGE, "a literal -- must precede the upstream command") from exc
    operator = list(argv[:separator])
    upstream = list(argv[separator + 1 :])
    if not upstream:
        raise AdapterFailure(EXIT_USAGE, "the literal -- must be followed by an upstream command")
    return operator, upstream


def main(argv: Sequence[str] | None = None) -> int:
    """CLI entry point."""

    arguments = list(sys.argv[1:] if argv is None else argv)
    if arguments == ["--self-test"]:
        try:
            return self_test()
        except (AssertionError, AdapterFailure) as exc:
            # Self-test failures name only the failed invariant, never a request or credential.
            print(f"live-hint-adapter self-test: FAIL: {exc}", file=sys.stderr)
            return EXIT_CONTROL_FAILED
    if arguments in (["-h"], ["--help"]):
        build_parser().parse_args(arguments)
        return EXIT_OK  # argparse's help action exits before this line.
    controller = ShutdownController()
    try:
        operator_arguments, upstream = split_operator_and_upstream(arguments)
        namespace = build_parser().parse_args(operator_arguments)
        endpoint = parse_base_url(cast(str, namespace.url))
        channel = validate_channel(cast(str, namespace.channel))
        token = read_private_token(cast(Path, namespace.token_file))
        sender = HttpHintSender(endpoint, channel, token, stderr_log)
        with installed_signal_handlers(controller):
            run_upstream(upstream, sender, controller, stderr_log)
    except StopRequested:
        signum = controller.signal_number or signal.SIGTERM
        return 128 + int(signum or signal.SIGTERM)
    except AdapterFailure as exc:
        stderr_log(exc.message)
        return exc.code


if __name__ == "__main__":
    raise SystemExit(main())
