"""JSON request transport for an operator-owned persistent local adapter."""

from __future__ import annotations

import json
import os
from pathlib import Path
import socket
import stat
import struct
import time

from agentctl.errors import HerdrUnavailable
from agentctl.jsonx import as_mapping


class SocketTransport:
    """Exchange one newline-delimited JSON request per private Unix connection.

    The server owns its credentials and connection pool. Calls are independent
    and can run concurrently. The socket and its parent must belong to the
    current account; the parent must be private. A server error or lost response
    is an unconfirmed operation, so callers retain their durable retry identity.
    """

    def __init__(self, path: str, *, timeout: float = 60) -> None:
        self.path = Path(path)
        self.timeout = timeout

    def __call__(self, request: dict[str, object]) -> dict[str, object]:
        """Send a bounded JSON request and validate its complete object response."""
        parent = self.path.parent.lstat()
        endpoint = self.path.lstat()
        if (not stat.S_ISDIR(parent.st_mode) or parent.st_uid != os.getuid() or parent.st_mode & 0o077
                or not stat.S_ISSOCK(endpoint.st_mode) or endpoint.st_uid != os.getuid()):
            raise HerdrUnavailable("chat adapter socket must be owned by this account in a private directory")
        payload = json.dumps(request, ensure_ascii=False, allow_nan=False).encode("utf-8") + b"\n"
        if len(payload) > 1024 * 1024:
            raise ValueError("chat adapter request exceeds 1 MiB")
        with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as connection:
            deadline = time.monotonic() + self.timeout
            connection.settimeout(max(0.001, deadline - time.monotonic()))
            connection.connect(str(self.path))
            if hasattr(socket, "SO_PEERCRED"):
                _, uid, _ = struct.unpack("3i", connection.getsockopt(socket.SOL_SOCKET, socket.SO_PEERCRED, 12))
                if uid != os.getuid():
                    raise HerdrUnavailable("chat adapter peer belongs to a different account")
            connection.settimeout(max(0.001, deadline - time.monotonic()))
            connection.sendall(payload)
            response = bytearray()
            while b"\n" not in response:
                remaining = deadline - time.monotonic()
                if remaining <= 0:
                    raise TimeoutError("chat adapter response exceeded its deadline")
                connection.settimeout(remaining)
                chunk = connection.recv(65536)
                if not chunk:
                    raise HerdrUnavailable("chat adapter disconnected before a complete response")
                response.extend(chunk)
                if len(response) > 8 * 1024 * 1024:
                    raise HerdrUnavailable("chat adapter response exceeds 8 MiB")
        line, remainder = bytes(response).split(b"\n", 1)
        if remainder.strip():
            raise HerdrUnavailable("chat adapter sent more than one response")
        result = as_mapping(json.loads(line.decode("utf-8")), "chat adapter response")
        if "error" in result:
            raise HerdrUnavailable("chat socket adapter reported failure; inspect its private diagnostics")
        return result
