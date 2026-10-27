"""Store the scorecard's detailed history, scorecard/cells.json, in parts.

GitHub refuses a pushed file larger than 100 MiB (GH001). The renderer's
detailed history passed that on 2026-09-30: the derived ledger commit 317771bb
carried a 107,322,430-byte scorecard/cells.json, GitHub refused the push, and
the unpushed commit stopped every publication queued behind it. The history
grows with every publication, so the ledger stores the renderer's exact bytes
as consecutive byte ranges of at most PART_MAX_BYTES each under
scorecard/cells/, with a manifest recording each part's size and SHA-256 and
the whole document's. Readers go through read_tree or read_commit, which check
every size and digest before they return the reassembled bytes.

The renderer is the measured Hermit checkout's own scorecard.rs, and every
version of it reads and writes the single file, on disk and through
`git show HEAD:scorecard/cells.json`. mirror.py therefore gives its throwaway
render copy a Git replacement for the published commit (install_render_view):
the commit keeps its name, and in the copy its tree holds the reassembled
single file in place of the parts. The renderer's input and output bytes do
not change. mirror.py splits the output before anything reaches the ledger
writer, and the replacement exists only in the copy's refs.

The parts may also live outside the commit (manifest schema v2, the
"external" layout): the commit then stores only scorecard/cells/manifest.json,
which still pins every part's size and SHA-256 and the whole document's, and
names in `parts_commit` the commit whose tree holds the parts at the same
paths. mirror.py keeps that commit on HISTORY_REF, replaced at every
regeneration, so the ledger's main history does not accumulate every
regeneration's parts (storage audit, 2026-10-12:
ai_docs/2026-10-12-ledger-and-site-storage-audit.md; 8.49 GiB of the 9.69 GiB
packed ledger was this history). read_commit, read_tree and
install_render_view follow the pointer with every check unchanged; a reader
whose repository lacks the parts commit is refused, never shown no history.

No file remains at scorecard/cells.json in this layout. A mirror.py from before
it gives the renderer no history, and every renderer since Hermit b912277877d
(https://github.com/rrnewton/hermit/pull/3141) then stops with "history
unavailable" before mirror.py copies anything back or commits. A ledger commit
from before this layout carries the single file and reads as before. A commit
carrying both forms is refused.
"""
from __future__ import annotations

import hashlib
import json
import os
import re
import stat
import subprocess
from pathlib import Path
from typing import Callable, Mapping

LOGICAL_PATH = "scorecard/cells.json"
PARTS_DIRECTORY = "scorecard/cells"
MANIFEST_PATH = f"{PARTS_DIRECTORY}/manifest.json"
MANIFEST_SCHEMA = "hermit-test-ledger-scorecard-cells/v1"
#: The external layout: the manifest names the commit holding the parts.
EXTERNAL_MANIFEST_SCHEMA = "hermit-test-ledger-scorecard-cells/v2"
#: Where mirror.py keeps the parts commit of the external layout.
HISTORY_REF = "refs/heads/scorecard-history"
# A publication that writes the scorecard history refuses, before the writer
# changes, any generated file or history part over this size (mirror._copy_back):
# more than twice below GitHub's 100 MiB refusal, and below its 50 MiB warning.
# The same commit's other files are not checked against it: latest.json, which
# mirror._publish_scorecard writes itself; the finalized-proof carrier files,
# which finalized_proof bounds only at ARTIFACT_BYTES_PER_ROW (256 MiB); and a
# legacy export's archive.
FILE_CEILING_BYTES = 48 * 1024 * 1024
PART_MAX_BYTES = FILE_CEILING_BYTES
MAX_PARTS = 10_000
MANIFEST_MAX_BYTES = 4 * 1024 * 1024
_PART = re.compile(r"scorecard/cells/part-([0-9]{4})")
_SHA256 = re.compile(r"[0-9a-f]{64}")
_OBJECT = re.compile(rb"[0-9a-f]{40}")
_MANIFEST_FIELDS = {"schema", "path", "bytes", "sha256", "part_max_bytes", "parts"}
_EXTERNAL_MANIFEST_FIELDS = _MANIFEST_FIELDS | {"parts_commit"}
_COMMIT_NAME = re.compile(r"[0-9a-f]{40}")
_PART_FIELDS = {"path", "bytes", "sha256"}


class CellsRefused(ValueError):
    """The stored history is malformed or ambiguous, or would exceed the ceiling."""


def _digest(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def part_path(index: int) -> str:
    return f"{PARTS_DIRECTORY}/part-{index:04d}"


def is_storage_path(path: str) -> bool:
    """Whether PATH is the single file, the manifest, or a part."""
    return path in {LOGICAL_PATH, MANIFEST_PATH} or _PART.fullmatch(path) is not None


def split(document: bytes, *, part_max_bytes: int | None = None) -> dict[str, bytes]:
    """Map each part path, then the manifest path, to the bytes storing DOCUMENT."""
    size = PART_MAX_BYTES if part_max_bytes is None else part_max_bytes
    if type(size) is not int or not 0 < size <= FILE_CEILING_BYTES:
        raise CellsRefused(f"part size {size!r} is outside 1..{FILE_CEILING_BYTES} bytes")
    if not document:
        raise CellsRefused("refusing to store an empty scorecard history")
    count = -(-len(document) // size)
    if count > MAX_PARTS:
        raise CellsRefused(f"scorecard history of {len(document)} bytes needs {count} parts; "
                           f"at most {MAX_PARTS} are allowed")
    view = memoryview(document)
    files = {part_path(index): bytes(view[index * size:(index + 1) * size]) for index in range(count)}
    manifest = {
        "schema": MANIFEST_SCHEMA, "path": LOGICAL_PATH, "bytes": len(document),
        "sha256": _digest(document), "part_max_bytes": size,
        "parts": [{"path": path, "bytes": len(data), "sha256": _digest(data)}
                  for path, data in files.items()],
    }
    files[MANIFEST_PATH] = (json.dumps(manifest, indent=2, sort_keys=True) + "\n").encode()
    return files


def _count(value: object, name: str, *, maximum: int) -> int:
    if type(value) is not int or not 0 < value <= maximum:
        raise CellsRefused(f"scorecard history manifest {name} {value!r} is outside 1..{maximum}")
    return value


def _manifest(encoded: bytes) -> dict:
    if len(encoded) > MANIFEST_MAX_BYTES:
        raise CellsRefused(f"{MANIFEST_PATH} is larger than {MANIFEST_MAX_BYTES} bytes")
    try:
        value = json.loads(encoded)
    except (UnicodeDecodeError, json.JSONDecodeError) as error:
        raise CellsRefused(f"{MANIFEST_PATH} is not JSON: {error}") from error
    external = isinstance(value, dict) and value.get("schema") == EXTERNAL_MANIFEST_SCHEMA
    fields = _EXTERNAL_MANIFEST_FIELDS if external else _MANIFEST_FIELDS
    if not isinstance(value, dict) or set(value) != fields:
        raise CellsRefused(f"{MANIFEST_PATH} must contain exactly " + ", ".join(sorted(fields)))
    if value["schema"] not in {MANIFEST_SCHEMA, EXTERNAL_MANIFEST_SCHEMA} or value["path"] != LOGICAL_PATH:
        raise CellsRefused(f"{MANIFEST_PATH} names schema {value['schema']!r} and path {value['path']!r}")
    if external and (not isinstance(value["parts_commit"], str)
                     or not _COMMIT_NAME.fullmatch(value["parts_commit"])):
        raise CellsRefused(f"{MANIFEST_PATH} parts_commit is not a forty-hex commit name")
    total = _count(value["bytes"], "bytes", maximum=MAX_PARTS * FILE_CEILING_BYTES)
    size = _count(value["part_max_bytes"], "part_max_bytes", maximum=FILE_CEILING_BYTES)
    if not isinstance(value["sha256"], str) or not _SHA256.fullmatch(value["sha256"]):
        raise CellsRefused(f"{MANIFEST_PATH} sha256 is not a lowercase SHA-256")
    parts = value["parts"]
    if not isinstance(parts, list) or len(parts) != -(-total // size):
        raise CellsRefused(f"{MANIFEST_PATH} must list {-(-total // size)} parts for {total} bytes")
    for index, part in enumerate(parts):
        # Only full parts, then one final part, so each document has one layout.
        expected = size if index + 1 < len(parts) else total - size * index
        if (not isinstance(part, dict) or set(part) != _PART_FIELDS
                or part["path"] != part_path(index) or type(part["bytes"]) is not int
                or part["bytes"] != expected
                or not isinstance(part["sha256"], str) or not _SHA256.fullmatch(part["sha256"])):
            raise CellsRefused(f"{MANIFEST_PATH} part {index} must be exactly "
                               f"{{path: {part_path(index)}, bytes: {expected}, sha256}}")
    return value


def _assemble(listed: set[str], read: Callable[[str], bytes],
              read_external: Callable[[str, str], bytes] | None = None) -> bytes | None:
    """The document LISTED stores, read through READ; external parts through READ_EXTERNAL(commit, path)."""
    stored = listed - {LOGICAL_PATH}
    if not stored:
        return read(LOGICAL_PATH) if LOGICAL_PATH in listed else None
    if LOGICAL_PATH in listed:
        raise CellsRefused(f"both {LOGICAL_PATH} and {PARTS_DIRECTORY}/ are present; "
                           "refusing the ambiguous scorecard history")
    if MANIFEST_PATH not in stored:
        raise CellsRefused(f"{PARTS_DIRECTORY}/ has no manifest.json")
    manifest = _manifest(read(MANIFEST_PATH))
    named = {part["path"] for part in manifest["parts"]}
    if manifest["schema"] == EXTERNAL_MANIFEST_SCHEMA:
        if stored != {MANIFEST_PATH}:
            raise CellsRefused(f"{PARTS_DIRECTORY}/ stores parts beside a manifest naming them in "
                               f"{manifest['parts_commit']}; refusing the ambiguous scorecard history")
        if read_external is None:
            raise CellsRefused(f"the scorecard history parts are in commit {manifest['parts_commit']}, "
                               "which this reader cannot read")
        commit = manifest["parts_commit"]
        read = lambda path: read_external(commit, path)  # noqa: E731
    elif stored != named | {MANIFEST_PATH}:
        raise CellsRefused(f"{PARTS_DIRECTORY}/ differs from its manifest: unexpected "
                           + (", ".join(sorted(stored - named - {MANIFEST_PATH})) or "none")
                           + "; missing " + (", ".join(sorted(named - stored)) or "none"))
    chunks = []
    for part in manifest["parts"]:
        data = read(part["path"])
        if len(data) != part["bytes"] or _digest(data) != part["sha256"]:
            raise CellsRefused(f"{part['path']} differs from its manifest entry")
        chunks.append(data)
    document = b"".join(chunks)
    if len(document) != manifest["bytes"] or _digest(document) != manifest["sha256"]:
        raise CellsRefused("reassembled scorecard history differs from its manifest")
    return document


def _read_regular(path: Path) -> bytes:
    try:
        descriptor = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
    except OSError as error:
        raise CellsRefused(f"cannot open {path}: {error}") from error
    with os.fdopen(descriptor, "rb") as source:
        if not stat.S_ISREG(os.fstat(source.fileno()).st_mode):
            raise CellsRefused(f"{path} is not a regular file")
        return source.read()


def _no_links(root: Path) -> None:
    """Refuse when scorecard/ or scorecard/cells/ under ROOT is a symbolic link."""
    current = root
    for name in PARTS_DIRECTORY.split("/"):
        current = current / name
        if os.path.islink(current):
            raise CellsRefused(f"{current.relative_to(root)} is a symbolic link")


def read_tree(root: Path) -> bytes | None:
    """The history stored in the files under ROOT, or None when there is none."""
    _no_links(root)
    listed = set()
    if os.path.lexists(root / LOGICAL_PATH):
        listed.add(LOGICAL_PATH)
    directory = root / PARTS_DIRECTORY
    if os.path.lexists(directory):
        if directory.is_symlink() or not directory.is_dir():
            raise CellsRefused(f"{PARTS_DIRECTORY} is not a directory")
        listed.update(f"{PARTS_DIRECTORY}/{entry.name}" for entry in os.scandir(directory))
    # An external manifest's parts are read from ROOT's own Git object store.
    return _assemble(listed, lambda path: _read_regular(root / path), _external_reader(root, None))


def _git(repo: Path, *args: str, environment: Mapping[str, str] | None = None,
         data: bytes | None = None) -> bytes:
    try:
        return subprocess.run(["git", "-C", str(repo), *args], input=data, capture_output=True,
                              check=True, timeout=300, env=environment).stdout
    except (OSError, subprocess.SubprocessError) as error:
        detail = getattr(error, "stderr", b"") or b""
        raise CellsRefused(f"git {' '.join(args[:2])} failed in {repo}: "
                           f"{detail.decode(errors='replace').strip() or error}") from error


def _object(output: bytes, what: str) -> str:
    name = output.strip()
    if not _OBJECT.fullmatch(name):
        raise CellsRefused(f"git returned {name!r} for {what}, not an object name")
    return name.decode()


def _stored(repo: Path, commit: str, environment: Mapping[str, str] | None) -> dict[str, str]:
    """Map each history path COMMIT itself stores (never a replacement) to its blob."""
    blobs = {}
    for entry in _git(repo, "--no-replace-objects", "ls-tree", "-r", "-z", "--full-tree", commit,
                      "--", LOGICAL_PATH, PARTS_DIRECTORY, environment=environment).split(b"\0"):
        if not entry:
            continue
        metadata, _, name = entry.partition(b"\t")
        mode, kind, object_id = metadata.decode().split()
        path = name.decode(errors="strict")
        if kind != "blob" or mode not in {"100644", "100755"}:
            raise CellsRefused(f"{path} at {commit} is not a regular file (mode {mode}, {kind})")
        blobs[path] = object_id
    return blobs


def read_commit(repo: Path, commit: str, *,
                environment: Mapping[str, str] | None = None) -> bytes | None:
    """The history stored at COMMIT in REPO, or None when there is none.

    Replacement objects are ignored, so this reads what COMMIT itself stores
    even in a render copy.
    """
    blobs = _stored(repo, commit, environment)
    return _assemble(set(blobs), lambda path: _git(
        repo, "--no-replace-objects", "cat-file", "blob", blobs[path], environment=environment),
        _external_reader(repo, environment))


def _external_reader(repo: Path, environment: Mapping[str, str] | None) -> Callable[[str, str], bytes]:
    """Read PATH at the parts commit an external manifest names, from REPO's object store."""
    def read(commit: str, path: str) -> bytes:
        found = subprocess.run(["git", "-C", str(repo), "--no-replace-objects", "cat-file", "-e",
                                f"{commit}^{{commit}}"], capture_output=True, env=environment, timeout=300)
        if found.returncode != 0:
            raise CellsRefused(f"the scorecard history parts commit {commit} is not in {repo}; "
                               f"fetch {HISTORY_REF}")
        return _git(repo, "--no-replace-objects", "cat-file", "blob", f"{commit}:{path}",
                    environment=environment)
    return read


def external_manifest(files: Mapping[str, bytes], parts_commit: str) -> bytes:
    """The schema-v2 manifest for FILES (split's output) whose parts PARTS_COMMIT stores."""
    if not _COMMIT_NAME.fullmatch(parts_commit):
        raise CellsRefused(f"parts commit {parts_commit!r} is not a forty-hex commit name")
    manifest = _manifest(files[MANIFEST_PATH])
    if manifest["schema"] != MANIFEST_SCHEMA:
        raise CellsRefused("only a split manifest can be made external")
    manifest.update(schema=EXTERNAL_MANIFEST_SCHEMA, parts_commit=parts_commit)
    return (json.dumps(manifest, indent=2, sort_keys=True) + "\n").encode()


def store_parts_commit(repo: Path, files: Mapping[str, bytes], *,
                       environment: Mapping[str, str] | None = None) -> str:
    """Write a parentless commit holding FILES' parts (split's output) at their paths; return it.

    Only Git objects are written, never REPO's work tree, index or refs; the
    caller names the commit by HISTORY_REF when it publishes. The commit's
    author, committer and dates come from ENVIRONMENT as for any commit.
    """
    if LOGICAL_PATH in files or _assemble(set(files), files.__getitem__) is None:
        raise CellsRefused("refusing to store a part set that split did not produce")
    entries = []
    for path, data in sorted(files.items()):
        if path == MANIFEST_PATH:
            continue
        blob = _object(_git(repo, "hash-object", "-w", "--stdin", data=data, environment=environment),
                       f"history part {path}")
        entries.append(f"100644 blob {blob}\t{path.rpartition('/')[2]}".encode())
    tree = _object(_git(repo, "mktree", "-z", environment=environment,
                        data=b"".join(entry + b"\0" for entry in entries)), "the history parts")
    for name in reversed(PARTS_DIRECTORY.split("/")):
        tree = _object(_git(repo, "mktree", "-z", environment=environment,
                            data=f"040000 tree {tree}\t{name}".encode() + b"\0"), f"the {name} tree")
    return _object(_git(repo, "commit-tree", tree, "-m", "Scorecard history parts", environment=environment),
                   "the history parts commit")


def replace_tree_external(root: Path, manifest: bytes, *,
                          install: Callable[[dict[str, bytes], list[str]], None]) -> None:
    """Make ROOT store exactly the external MANIFEST: its parts and any single file removed."""
    if _manifest(manifest)["schema"] != EXTERNAL_MANIFEST_SCHEMA:
        raise CellsRefused("replace_tree_external stores only an external manifest")
    _no_links(root)
    directory = root / PARTS_DIRECTORY
    if os.path.lexists(directory) and (directory.is_symlink() or not directory.is_dir()):
        raise CellsRefused(f"{PARTS_DIRECTORY} is not a directory")
    removals = []
    if directory.is_dir():
        for entry in os.scandir(directory):
            path = f"{PARTS_DIRECTORY}/{entry.name}"
            if entry.is_symlink() or not entry.is_file():
                raise CellsRefused(f"{path} is not a regular file")
            if path != MANIFEST_PATH:
                removals.append(path)
    if os.path.lexists(root / LOGICAL_PATH):
        removals.append(LOGICAL_PATH)
    install({MANIFEST_PATH: manifest}, sorted(removals))


def _replace_entry(listing: bytes, name: bytes, entry: bytes) -> bytes:
    """ls-tree -z LISTING with NAME's entry replaced by ENTRY, for mktree -z."""
    kept = [item for item in listing.split(b"\0") if item and item.partition(b"\t")[2] != name]
    if len(kept) == len([item for item in listing.split(b"\0") if item]):
        raise CellsRefused(f"tree entry {name.decode(errors='replace')} is missing")
    return b"".join(item + b"\0" for item in [*kept, entry])


def install_render_view(repo: Path, commit: str, environment: Mapping[str, str]) -> str | None:
    """Show the history COMMIT stores in parts to REPO's readers as one file.

    REPO is mirror.py's throwaway render copy, before COMMIT is checked out.
    When COMMIT stores parts, write a replacement commit whose tree differs
    from COMMIT's only in holding the reassembled scorecard/cells.json in place
    of scorecard/cells/, and name it COMMIT's replacement in REPO
    (refs/replace/COMMIT). Git then reads COMMIT's tree as the replacement's
    for checkout, status and `git show HEAD:scorecard/cells.json`, while COMMIT
    and HEAD keep their names. Git ignores the replacement when
    GIT_NO_REPLACE_OBJECTS is set or core.useReplaceRefs is false, so
    ENVIRONMENT must be the one every reader of REPO uses.

    Returns the replacement, or None when COMMIT stores the single file or no
    history; readers then see COMMIT itself.
    """
    blobs = _stored(repo, commit, environment)
    if set(blobs) <= {LOGICAL_PATH}:
        return None
    document = _assemble(set(blobs), lambda path: _git(
        repo, "--no-replace-objects", "cat-file", "blob", blobs[path], environment=environment),
        _external_reader(repo, environment))
    blob = _object(_git(repo, "hash-object", "-w", "--stdin", data=document,
                        environment=environment), "the reassembled history")
    directory, _, name = LOGICAL_PATH.rpartition("/")
    parent = _object(_git(repo, "mktree", "-z", environment=environment, data=_replace_entry(
        _git(repo, "--no-replace-objects", "ls-tree", "-z", f"{commit}:{directory}",
             environment=environment),
        PARTS_DIRECTORY.rpartition("/")[2].encode(),
        f"100644 blob {blob}\t{name}".encode())), f"{directory} with the single file")
    root = _object(_git(repo, "mktree", "-z", environment=environment, data=_replace_entry(
        _git(repo, "--no-replace-objects", "ls-tree", "-z", commit, environment=environment),
        directory.encode(), f"040000 tree {parent}\t{directory}".encode())), "the viewed tree")
    header, separator, rest = _git(repo, "--no-replace-objects", "cat-file", "commit", commit,
                                   environment=environment).partition(b"\n")
    if not separator or not header.startswith(b"tree ") or not _OBJECT.fullmatch(header[5:]):
        raise CellsRefused(f"commit {commit} does not start with its tree")
    replacement = _object(_git(repo, "hash-object", "-t", "commit", "-w", "--stdin",
                               data=b"tree " + root.encode() + b"\n" + rest,
                               environment=environment), "the replacement commit")
    _git(repo, "replace", commit, replacement, environment=environment)
    return replacement


def replacements(repo: Path, environment: Mapping[str, str]) -> dict[str, str]:
    """Map each object REPO replaces to its replacement."""
    found = {}
    for line in _git(repo, "for-each-ref", "--format=%(refname) %(objectname)", "refs/replace/",
                     environment=environment).decode().splitlines():
        name, _, replacement = line.partition(" ")
        found[name.removeprefix("refs/replace/")] = replacement
    return found


def replace_tree(root: Path, files: Mapping[str, bytes], *,
                 install: Callable[[dict[str, bytes], list[str]], None] | None = None) -> None:
    """Make ROOT store exactly FILES, which split produced.

    FILES must pass every check a reader makes, which bounds each part by
    FILE_CEILING_BYTES. The parts and the manifest are written first; the
    single file and any part FILES does not name are removed after. Every path
    is checked before anything is written, so a refusal leaves ROOT unchanged.
    INSTALL, when given, makes those writes and removals instead (paths
    relative to ROOT, in that order).
    """
    if LOGICAL_PATH in files or _assemble(set(files), files.__getitem__) is None:
        raise CellsRefused("refusing to store a part set that split did not produce")
    _no_links(root)
    directory = root / PARTS_DIRECTORY
    if os.path.lexists(directory) and (directory.is_symlink() or not directory.is_dir()):
        raise CellsRefused(f"{PARTS_DIRECTORY} is not a directory")
    stale = []
    if directory.is_dir():
        for entry in os.scandir(directory):
            path = f"{PARTS_DIRECTORY}/{entry.name}"
            if entry.is_symlink() or not entry.is_file():
                raise CellsRefused(f"{path} is not a regular file")
            if path not in files:
                stale.append(root / path)
    single = root / LOGICAL_PATH
    if os.path.lexists(single):
        if single.is_symlink() or not single.is_file():
            raise CellsRefused(f"{LOGICAL_PATH} is not a regular file")
        stale.append(single)
    directory.mkdir(parents=True, exist_ok=True)
    if install is not None:
        install(dict(files), [path.relative_to(root).as_posix() for path in stale])
        return
    for path, data in files.items():
        (root / path).write_bytes(data)
    for path in stale:
        path.unlink()
