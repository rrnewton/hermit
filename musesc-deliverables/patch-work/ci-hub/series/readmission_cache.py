#!/usr/bin/env python3
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

"""Per-blob cache of what readmission derives from the published ledger.

Background: before each series publication, ``series._readmission_plan``
replays every published refused cell result through the current writer. Most
of its time was spent re-deriving the same facts from the same published
bytes on every run, not replaying: on devbig014 on 2026-10-11 (ledger
d3727b5f, 9,271 series blobs holding 1.48 GB, 26,400 published refusals) the
plan took 47.9 s, of which reading and scanning the series blobs for event
ids and batch contexts took about 16 s, and validating, expanding and
restoring the published refusals, then deriving the row identity of each
refused input, about 18 s. The replay itself (11.3 s) is not cached.

A git blob is named by its content, so whatever a pure function derives from
one blob's bytes can be kept under the blob's object id for as long as the
code that derived it is unchanged. ``code_key`` digests that code: every
loaded ci-hub module, the Python version, and the extra values the caller
names (the owner-home prefixes a restoration tries). Each cache directory
holds one key; the newest ``RETAINED_KEYS`` are kept, so a tool tree one
commit older or newer running concurrently does not thrash it.

An entry that is missing, unreadable, of another format, or names another
blob is treated as absent, and the caller derives the facts again. A failed
write costs only that rerun. ``CI_HUB_READMISSION_CACHE=off`` disables the
cache.
"""

from __future__ import annotations

import contextlib
import hashlib
import json
import os
import re
import shutil
import sys
import tempfile
from collections.abc import Iterable
from pathlib import Path
from typing import Any

HERE = Path(__file__).resolve().parent
TOOL_ROOT = HERE.parents[1]
CACHE_FORMAT = 1
# Set to "off" to derive everything from the ledger bytes on every run.
DISABLE_ENV = "CI_HUB_READMISSION_CACHE"
# Set to "1" to print one line of cache statistics to stderr.
STATS_ENV = "CI_HUB_READMISSION_CACHE_STATS"
RETAINED_KEYS = 3
_OID_RE = re.compile(r"[0-9a-f]{40}(?:[0-9a-f]{24})?")


def enabled() -> bool:
    return os.environ.get(DISABLE_ENV, "").lower() not in {"off", "0", "no"}


def default_cache_root(parent: Any) -> Path:
    """Mutable state lives under the parent checkout, never the tool tree."""
    return Path(parent) / "ignored" / "ci-hub" / "readmission-cache"


def _sha256(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def code_key(extra: Iterable[str] = ()) -> str:
    """Digest of everything besides a blob's bytes that its entry depends on."""
    tool = TOOL_ROOT.resolve()
    files: set[Path] = set()
    for module in list(sys.modules.values()):
        path = getattr(module, "__file__", None)
        if not path:
            continue
        resolved = Path(path).resolve()
        if resolved.suffix == ".py" and (tool / "ci-hub") in resolved.parents:
            files.add(resolved)
    digest = hashlib.sha256()
    for item in (f"format={CACHE_FORMAT}", f"python={sys.version}", *extra):
        digest.update(item.encode() + b"\0")
    for path in sorted(files):
        try:
            content = _sha256(path.read_bytes())
        except OSError:
            content = "unreadable"
        digest.update(f"{path.relative_to(tool).as_posix()}={content}".encode() + b"\0")
    return digest.hexdigest()


class BlobCache:
    """Entries of one kind (``series`` or ``quarantine``), one file per blob."""

    def __init__(self, root: Path, key: str, kind: str):
        self.directory = root / key / kind
        self.hits = 0
        self.misses = 0
        self.writes = 0

    def _path(self, oid: str) -> Path:
        return self.directory / oid[:2] / f"{oid}.json"

    def load(self, oid: str) -> dict | None:
        if _OID_RE.fullmatch(oid) is None:
            return None
        try:
            entry = json.loads(self._path(oid).read_text())
        except (OSError, ValueError):
            entry = None
        if (not isinstance(entry, dict) or entry.get("format") != CACHE_FORMAT
                or entry.get("oid") != oid):
            self.misses += 1
            return None
        self.hits += 1
        return entry

    def store(self, oid: str, entry: dict) -> None:
        """Atomically replace one blob's entry; a failure only costs a rerun."""
        if _OID_RE.fullmatch(oid) is None:
            return
        path = self._path(oid)
        try:
            path.parent.mkdir(parents=True, exist_ok=True)
            handle, temporary = tempfile.mkstemp(dir=path.parent, prefix=".tmp-")
            try:
                with os.fdopen(handle, "w") as out:
                    json.dump({**entry, "format": CACHE_FORMAT, "oid": oid}, out,
                              separators=(",", ":"))
                os.replace(temporary, path)
            except BaseException:
                with contextlib.suppress(OSError):
                    os.unlink(temporary)
                raise
        except OSError:
            return
        self.writes += 1


def open_caches(parent: Any, extra: Iterable[str], cache_root: Path | None = None,
                ) -> tuple[BlobCache, BlobCache] | None:
    """The ``(series, quarantine)`` caches of the current key, or None when disabled.

    Opening one retires all but the newest ``RETAINED_KEYS`` key directories.
    """
    if not enabled():
        return None
    root = cache_root if cache_root is not None else default_cache_root(parent)
    key = code_key(extra)
    current = root / key
    try:
        current.mkdir(parents=True, exist_ok=True)
        os.utime(current)
    except OSError:
        return None
    _retire_old_keys(root, current)
    return BlobCache(root, key, "series"), BlobCache(root, key, "quarantine")


def _retire_old_keys(root: Path, current: Path) -> None:
    try:
        others = [path for path in root.iterdir() if path.is_dir() and path != current]
    except OSError:
        return

    def modified(path: Path) -> float:
        try:
            return path.stat().st_mtime
        except OSError:
            return 0.0

    others.sort(key=modified, reverse=True)
    for stale in others[RETAINED_KEYS - 1:]:
        shutil.rmtree(stale, ignore_errors=True)


def report(series: BlobCache, quarantine: BlobCache, seconds: float) -> None:
    if os.environ.get(STATS_ENV) == "1":
        print(
            "readmission cache: "
            f"series hits={series.hits} misses={series.misses} writes={series.writes} "
            f"quarantine hits={quarantine.hits} misses={quarantine.misses} "
            f"writes={quarantine.writes} seconds={seconds:.2f}",
            file=sys.stderr,
        )
