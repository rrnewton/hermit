"""Every default the user guide states in PROSE must be the default the code actually has.

`herdr-run init` exists so the guide can point at a generated file instead of listing the knobs,
and the guide does that everywhere except two sentences: the ones explaining why retention and the
pane cap exist at all. Those read badly with a cross-reference dropped into the middle of them, so
they name the number — "(four by default)", "(32 by default)". That is the same duplicate-free-to-
drift the generated template was introduced to remove, one layer up, and nothing could see it.

It is pinned here in three places at once, deliberately not parametrised over the production
constant: the number is written LITERALLY in `_GUIDE_DEFAULTS`, the constant is READ from
`Config`, and the spelling is MATCHED in the shipped guide. A test that read the constant on both
sides would agree with whatever the code said, including a typo, and so would pin nothing.

Changing any one of the three alone goes red:

* constant only -> `test_the_defaults_are_the_numbers_the_guide_names`
* prose only    -> `test_the_guide_states_exactly_these_prose_defaults`
* literal only  -> both
"""

from __future__ import annotations

import re
from importlib.resources import files
from pathlib import Path

from herdr_run.config import Config

_REPO_ROOT = Path(__file__).resolve().parents[2]

#: (configuration key, how the guide spells its default, the default itself).
_GUIDE_DEFAULTS: tuple[tuple[str, str, int], ...] = (
    ("retention_days", "four", 4),
    ("max_panes", "32", 32),
)

#: The prose shape the guide uses to restate a default: ``  `key` (value by default)  ``.
_PROSE_DEFAULT = re.compile(r"`([a-z_]+)` \(([^()]*?) by default\)")


def _shipped_userguide() -> str:
    """The guide as it actually ships, not the template it is rendered from."""
    return (files("herdr_run") / "USER_GUIDE.md").read_text(encoding="utf-8")


def test_the_defaults_are_the_numbers_the_guide_names() -> None:
    """The literal number in this file is the built-in default."""
    defaults = Config()
    for key, _spelling, number in _GUIDE_DEFAULTS:
        assert getattr(defaults, key) == number, (
            f"the built-in {key} is {getattr(defaults, key)}, but the user guide is pinned to "
            f"{number}; change the guide prose and the number here together"
        )


def test_the_guide_states_exactly_these_prose_defaults() -> None:
    """Every prose default in the shipped guide is one this file pins, spelled the pinned way."""
    guide = _shipped_userguide()
    expected = {key: spelling for key, spelling, _number in _GUIDE_DEFAULTS}

    found = _PROSE_DEFAULT.findall(guide)
    assert found, (
        "the user guide no longer restates any default in prose. If that is deliberate, delete "
        f"the entries in _GUIDE_DEFAULTS that no longer exist: {sorted(expected)}"
    )
    for key, spelling in found:
        assert key in expected, (
            f"the user guide restates a default for '{key}' that nothing pins to the constant; "
            "add it to _GUIDE_DEFAULTS or point the prose at 'herdr-run init' instead"
        )
        assert spelling == expected[key], (
            f"the user guide says {key} is '{spelling}' by default; the pinned spelling is "
            f"'{expected[key]}'"
        )
    assert {key for key, _spelling in found} == set(expected), (
        "a pinned prose default is no longer in the shipped guide: "
        f"{sorted(set(expected) - {key for key, _spelling in found})}"
    )


def test_the_skill_card_states_the_same_retention_window() -> None:
    """The agent-facing card restates retention too, in words, and nothing else could see it.

    It is not a packaged resource, so it is read from the source tree — the same way the
    parallel-experiment-runner card is pinned to its canonical example.
    """
    card = (_REPO_ROOT / "skills" / "herdr-run" / "SKILL.md").read_text(encoding="utf-8")
    spelling = dict((key, value) for key, value, _number in _GUIDE_DEFAULTS)["retention_days"]
    assert f"({spelling} days by default)" in card, (
        f"the herdr-run skill card no longer says retention is '{spelling} days by default'; "
        "it and the constant have to move together"
    )
