"""Account-global lock-path and permission invariants."""

from __future__ import annotations

import os
import pwd
from types import SimpleNamespace

import pytest

import herdr_run.state as state


def test_state_root_comes_from_passwd_not_environment(
    tmp_path: object, monkeypatch: pytest.MonkeyPatch
) -> None:
    account_home = os.path.join(str(tmp_path), "account-home")
    monkeypatch.setenv("HOME", os.path.join(str(tmp_path), "caller-home"))
    monkeypatch.setenv("XDG_STATE_HOME", os.path.join(str(tmp_path), "caller-state"))
    monkeypatch.setattr(
        pwd, "getpwuid", lambda _uid: SimpleNamespace(pw_dir=account_home)
    )

    assert state.account_state_root() == os.path.join(
        account_home, ".local", "state", "herdr-run"
    )


def test_lock_tree_is_private_and_pane_identifier_is_hashed(
    tmp_path: object, monkeypatch: pytest.MonkeyPatch
) -> None:
    account_home = os.path.join(str(tmp_path), "account-home")
    monkeypatch.setattr(state, "_account_home", lambda: account_home)

    session = state.session_lock_path()
    pane = state.pane_lock_path("opaque:pane/id")
    assert session == os.path.join(
        account_home, ".local", "state", "herdr-run", "locks", "session-resolve.lock"
    )
    assert os.path.dirname(pane) == os.path.join(
        account_home, ".local", "state", "herdr-run", "locks", "panes"
    )
    assert "opaque:pane/id" not in pane

    with state.open_lock_file(pane):
        pass
    for directory in (
        state.account_state_root(),
        os.path.join(state.account_state_root(), "locks"),
        os.path.join(state.account_state_root(), "locks", "panes"),
    ):
        assert os.stat(directory).st_mode & 0o777 == 0o700
    assert os.stat(pane).st_mode & 0o777 == 0o600
