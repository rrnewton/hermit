#!/usr/bin/env python3
"""Reject the names of projects that CONSUME this repository.

`agent-utils` is a reusable collection of libraries. Other repositories submodule it and use it;
they are its clients, not its subject. A reader who has never heard of a client must be able to
read this tree end to end without meeting a name they cannot resolve, and without having to work
out whether it is a dependency, a fork, or a place they are supposed to have access to.

Three narrower guards already forbade these names, but only inside PACKAGED OUTPUT -- rendered
user guides and the docstrings that ship in a wheel or a crate. That left most of the tree
uncovered, and the paths where the names actually accumulated -- `ai_docs/`, `reviews/`, dated
design records -- are exactly the ones `scripts/validate.py` maps to no checks at all. This scans
every tracked text file instead, so there is nowhere left for a client name to sit unobserved.

Two safety properties, both covered by `--self-test`:

* **An exemption must still be earned.** Every allowlisted path has to exist AND still contain a
  match. An exemption that has outlived its reason is a failure, not a quiet no-op, so the
  allowlist cannot grow into a second place where these names are tolerated.
* **Ordinary English is not a violation.** The pattern is anchored so that `thermite` and
  `hermitage` pass while `dev-hermit` and `DeepScryFoo` do not.

See `#86 scrub-client-names`.

Usage:
    python3 scripts/check_client_names.py
    python3 scripts/check_client_names.py --self-test
"""

from __future__ import annotations

import argparse
import re
import subprocess
import sys
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent

#: The client projects, case-insensitively, so `deepscry-bot`, `DeepScryFoo` and `dev-hermit`
#: are all caught while `thermite` and `hermitage` are not.
#:
#: The trailing guard is `[sS]?(?![a-z])` rather than a plain `\b`, because `\b` does not sit
#: between `y` and `F`: `\bDeepScry\b` misses `DeepScryFoo` entirely, and a name glued to a
#: CamelCase suffix is exactly how one survived in a test fixture. Refusing only a LOWERCASE
#: continuation keeps `hermitage` out while still catching a CamelCase or hyphenated compound,
#: and the optional `s` keeps the plural.
#:
#: The case-fold is SCOPED to the name with `(?i:...)` instead of being a flag on the whole
#: pattern. Under a global `re.IGNORECASE` the lookahead `[a-z]` also matches `A-Z`, which turns
#: "not followed by a lowercase letter" into "not followed by a letter" and silently reinstates
#: the CamelCase hole this trailing guard exists to close.
FORBIDDEN = re.compile(r"\b(?i:DeepScry|Hermit)[sS]?(?![a-z])")

#: Paths permitted to contain a client name, each because it DEFINES or TESTS the prohibition.
#: This is the whole allowlist; a file that merely mentions a client does not belong here.
ALLOWED: dict[str, str] = {
    "scripts/check_client_names.py": "states the prohibition",
    "scripts/check_python_packages.py": "forbids the names in packaged Python docs",
    "scripts/check_rust_packages.py": "forbids the names in packaged Rust docs",
    "scripts/embed_userguides.py": "forbids the names in rendered user guides",
    "py/tests/test_packaging_infrastructure.py": "proves the packaged-docs guard fires",
}


def repo_files() -> list[str]:
    """Every tracked file, PLUS untracked files git is not ignoring.

    Untracked matters: this runs before a commit, and a brand-new file is the most likely place
    for a client name to arrive. Scanning only the index would wave through the one case the
    guard exists to stop -- as it did to this very script, which was untracked when first run.
    """
    names: set[str] = set()
    for argv in (
        ("git", "ls-files", "-z"),
        ("git", "ls-files", "--others", "--exclude-standard", "-z"),
    ):
        done = subprocess.run(argv, cwd=REPO_ROOT, capture_output=True, text=True, check=False)
        if done.returncode != 0:
            raise SystemExit(
                f"check-client-names: `{' '.join(argv)}` failed:\n{done.stderr.strip()}"
            )
        names.update(name for name in done.stdout.split("\0") if name)
    return sorted(names)


def _matches(path: Path) -> list[tuple[int, str]]:
    """Whole-word client-name hits in a text file, as `(line number, line)`."""
    try:
        text = path.read_text(encoding="utf-8")
    except (UnicodeDecodeError, OSError):
        return []  # binary or unreadable: nothing a name can hide in as text
    if not FORBIDDEN.search(text):
        return []
    return [
        (number, line.strip())
        for number, line in enumerate(text.splitlines(), 1)
        if FORBIDDEN.search(line)
    ]


def violations(names: list[str], root: Path) -> tuple[list[str], list[str]]:
    """Return `(hits, stale)`: forbidden occurrences, and exemptions that are no longer earned."""
    hits: list[str] = []
    exercised: set[str] = set()
    for name in names:
        found = _matches(root / name)
        if not found:
            continue
        if name in ALLOWED:
            exercised.add(name)
            continue
        hits.extend(f"{name}:{number}: {line}" for number, line in found)
    stale = [
        f"{name}: exempt to {reason}, but it no longer contains a client name"
        for name, reason in sorted(ALLOWED.items())
        if name not in exercised
    ]
    return hits, stale


def self_test() -> int:
    """Check the guard itself. No git, no network, no build."""
    import tempfile

    failures: list[str] = []

    def expect(condition: bool, why: str) -> None:
        if not condition:
            failures.append(why)

    # The pattern is the part that has to be right.
    expect(bool(FORBIDDEN.search("the DeepScry workspace")), "must catch a bare client name")
    expect(bool(FORBIDDEN.search("dev-hermit")), "must catch a name inside a hyphenated identifier")
    expect(bool(FORBIDDEN.search("DeepScryFoo")), "must catch a name inside CamelCase")
    expect(bool(FORBIDDEN.search("HERMIT")), "must be case-insensitive")
    expect(not FORBIDDEN.search("thermite"), "must not fire on a word merely containing the letters")
    expect(not FORBIDDEN.search("hermitage"), "must not fire on an unrelated English word")
    expect(bool(FORBIDDEN.search("Hermits")), "must catch the plural")
    expect(bool(FORBIDDEN.search("deepscry-bot")), "must catch a name before a hyphen")

    with tempfile.TemporaryDirectory() as raw:
        root = Path(raw)
        (root / "scripts").mkdir()
        (root / "clean.md").write_text("nothing to see\n", encoding="utf-8")
        (root / "dirty.md").write_text("built for Hermit\n", encoding="utf-8")
        (root / "scripts/check_client_names.py").write_text("DeepScry|Hermit\n", encoding="utf-8")

        hits, stale = violations(["clean.md", "dirty.md", "scripts/check_client_names.py"], root)
        expect(len(hits) == 1 and hits[0].startswith("dirty.md:1:"), f"one unexempt hit, got {hits}")
        expect(
            not any(h.startswith("scripts/check_client_names.py") for h in hits),
            "an allowlisted path must not be reported as a violation",
        )
        # Every other allowlisted path is absent from this fixture, so all of them read as stale.
        expect(
            any(s.startswith("py/tests/test_packaging_infrastructure.py") for s in stale),
            f"an exemption with no remaining match must be reported stale, got {stale}",
        )
        expect(
            not any(s.startswith("scripts/check_client_names.py") for s in stale),
            "an exemption that IS still exercised must not be reported stale",
        )

    for failure in failures:
        print(f"FAIL  {failure}", file=sys.stderr)
    if failures:
        print(f"\n{len(failures)} self-test failure(s)", file=sys.stderr)
        return 1
    print("check-client-names --self-test: PASSED (12 controls)")
    return 0


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(
        description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter
    )
    parser.add_argument("--self-test", action="store_true", help="check the guard offline, then exit")
    args = parser.parse_args(argv)

    if args.self_test:
        return self_test()

    names = repo_files()
    hits, stale = violations(names, REPO_ROOT)

    if hits or stale:
        print("check-client-names: FAIL", file=sys.stderr)
        for hit in hits:
            print(f"  {hit}", file=sys.stderr)
        if hits:
            print(
                "\n  agent-utils is a reusable library; the projects that consume it are not its\n"
                "  subject. Describe the BEHAVIOUR instead of naming the client, or use a neutral\n"
                "  label. See `#86 scrub-client-names`.",
                file=sys.stderr,
            )
        for entry in stale:
            print(f"  {entry}", file=sys.stderr)
        if stale:
            print(
                "\n  Remove the exemption from ALLOWED: it no longer protects anything.",
                file=sys.stderr,
            )
        return 1

    print(f"check-client-names: ok — {len(names)} files name no consuming project")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
