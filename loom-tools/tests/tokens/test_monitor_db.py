"""Tests for the claude-monitor live-credential importer (#4006).

The bug these guard against is silent and total: ``accounts.env`` is a snapshot,
so after an operator rolls every account the pool keeps authenticating with
revoked tokens and the daemon's concurrency cap collapses to zero. The importer
must read the *live* store instead, and must actually replace on-disk tokens
when ``--force`` is given.
"""

from __future__ import annotations

import json
import sqlite3
import stat
from pathlib import Path

import pytest

from loom_tools.tokens import monitor_db as monitor_db_mod
from loom_tools.tokens.cli import main
from loom_tools.tokens.monitor_db import (
    MonitorDbUnavailable,
    credentials_to_accounts,
    import_from_monitor,
    monitor_db_path,
    read_monitor_credentials,
)

from .conftest import make_monitor_db  # relocated shared usage.db builder

# The ``monitor_db`` fixture lives in ``conftest.py`` (shared with
# test_bootstrap); ``make_monitor_db`` is imported above for the tests that
# build one-off stores inline.


# --------------------------------------------------------------------------
# read_monitor_credentials
# --------------------------------------------------------------------------


def test_reads_only_active_credentials(monitor_db: Path) -> None:
    creds = read_monitor_credentials(monitor_db)
    assert [c.email for c in creds] == [
        "robb@2amlogic.com",
        "agent-1@2amlogic.com",
    ]


def test_email_recovered_from_org_label_without_accounts_row(tmp_path: Path) -> None:
    """A credential with no joined accounts row still resolves via its label."""
    db = make_monitor_db(
        tmp_path / "usage.db",
        [{"label": "solo@example.com's Organization", "token": "tok"}],
    )
    creds = read_monitor_credentials(db)
    assert [c.email for c in creds] == ["solo@example.com"]


def test_row_without_resolvable_email_is_skipped(tmp_path: Path) -> None:
    """A display-name label with no accounts row has no stable filename."""
    db = make_monitor_db(
        tmp_path / "usage.db",
        [
            {"label": "Claude Code (rwalters)", "token": "tok"},
            {"label": "ok@example.com", "email": "ok@example.com", "token": "tok2"},
        ],
    )
    creds = read_monitor_credentials(db)
    assert [c.email for c in creds] == ["ok@example.com"]


def test_row_without_token_is_skipped(tmp_path: Path) -> None:
    db = make_monitor_db(
        tmp_path / "usage.db",
        [
            {"label": "a@example.com", "email": "a@example.com", "token": None},
            {"label": "b@example.com", "email": "b@example.com", "token": "  "},
            {"label": "c@example.com", "email": "c@example.com", "token": "tok"},
        ],
    )
    assert [c.email for c in read_monitor_credentials(db)] == ["c@example.com"]


def test_duplicate_email_keeps_highest_row_id(tmp_path: Path) -> None:
    """Rows are append-ordered, so the last active row is the current token."""
    db = make_monitor_db(
        tmp_path / "usage.db",
        [
            {"label": "dup@example.com", "email": "dup@example.com", "token": "old"},
            {"label": "dup@example.com", "email": "dup@example.com", "token": "new"},
        ],
    )
    creds = read_monitor_credentials(db)
    assert len(creds) == 1
    assert creds[0].token == "new"


def test_past_expires_at_does_not_filter(tmp_path: Path) -> None:
    """Observed rows carry stale expires_at while still authenticating."""
    db = make_monitor_db(
        tmp_path / "usage.db",
        [
            {
                "label": "x@example.com",
                "email": "x@example.com",
                "token": "tok",
                "expires_at": 1,  # 1970
            }
        ],
    )
    assert len(read_monitor_credentials(db)) == 1


def test_missing_database_raises(tmp_path: Path) -> None:
    with pytest.raises(MonitorDbUnavailable, match="not found"):
        read_monitor_credentials(tmp_path / "nope" / "usage.db")


def test_schema_without_credentials_table_raises(tmp_path: Path) -> None:
    """An older claude-monitor has no oauth_credentials table."""
    db = tmp_path / "usage.db"
    conn = sqlite3.connect(db)
    conn.execute("CREATE TABLE unrelated (id INTEGER)")
    conn.commit()
    conn.close()
    with pytest.raises(MonitorDbUnavailable, match="oauth_credentials"):
        read_monitor_credentials(db)


def test_database_is_not_modified(monitor_db: Path) -> None:
    """The store belongs to claude-monitor — we only ever read it."""
    before = monitor_db.read_bytes()
    read_monitor_credentials(monitor_db)
    assert monitor_db.read_bytes() == before


def test_monitor_db_path_honors_env(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setenv("LOOM_CLAUDE_MONITOR_DIR", "/custom/monitor")
    assert monitor_db_path() == Path("/custom/monitor/usage.db")


# --------------------------------------------------------------------------
# Read-only URI construction — #4029
#
# A '?' or '#' in the path terminates the URI early and silently drops the
# mode=ro query parameter, so the connection opens read-WRITE against another
# tool's live database. The regression is that the pre-fix tests only asserted
# the open succeeded (which it did, dangerously), not that the handle is
# genuinely read-only.
# --------------------------------------------------------------------------


@pytest.mark.parametrize("dirname", ["q?dir", "h#ash", "pct%20", "sp ace"])
def test_special_char_path_opens_and_is_read_only(
    tmp_path: Path, dirname: str
) -> None:
    """A path with URI-special characters must still open read-only.

    Asserts both halves the old tests missed: the store opens (so mode=ro is not
    lost in the ``%`` direction, which fails the open outright), AND a write
    attempt raises — i.e. mode=ro survived rather than being silently dropped.
    """
    db = make_monitor_db(
        tmp_path / dirname / "usage.db",
        [{"label": "x@example.com", "email": "x@example.com", "token": "tok"}],
    )

    # (a) opens via the module and returns the credential
    creds = read_monitor_credentials(db)
    assert [c.email for c in creds] == ["x@example.com"]

    # (b) the connection the module builds is genuinely read-only: a write
    # attempt must raise, proving mode=ro was not silently dropped.
    conn = sqlite3.connect(monitor_db_mod._read_only_uri(db), uri=True)
    try:
        with pytest.raises(sqlite3.OperationalError):
            conn.execute("CREATE TABLE z (y)")
    finally:
        conn.close()


def test_relative_monitor_dir_does_not_raise_value_error(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """A relative LOOM_CLAUDE_MONITOR_DIR must not escape as a bare ValueError.

    ``monitor_db_path()`` does not absolutize (``claude_monitor_dir()`` only
    ``expanduser()``s), so a relative override reaches URI construction. The fix
    must surface as success or ``MonitorDbUnavailable`` — never ``ValueError``
    (which ``Path.as_uri()`` would raise on a relative path).
    """
    monkeypatch.chdir(tmp_path)
    make_monitor_db(
        tmp_path / "rel" / "usage.db",
        [{"label": "x@example.com", "email": "x@example.com", "token": "tok"}],
    )
    monkeypatch.setenv("LOOM_CLAUDE_MONITOR_DIR", "rel")

    creds = read_monitor_credentials()
    assert [c.email for c in creds] == ["x@example.com"]


def test_percent_in_path_opens_where_it_failed_before(tmp_path: Path) -> None:
    """A '%' in the path was a hard open failure pre-fix; now it opens."""
    db = make_monitor_db(
        tmp_path / "pct%dir" / "usage.db",
        [{"label": "x@example.com", "email": "x@example.com", "token": "tok"}],
    )
    assert [c.email for c in read_monitor_credentials(db)] == ["x@example.com"]


# --------------------------------------------------------------------------
# SELECT error hint narrowing — #4029 fold-in item 1
# --------------------------------------------------------------------------


def test_missing_table_keeps_predate_hint(tmp_path: Path) -> None:
    """A missing oauth_credentials table still earns the "predate" hint."""
    db = tmp_path / "usage.db"
    conn = sqlite3.connect(db)
    conn.execute("CREATE TABLE unrelated (id INTEGER)")
    conn.commit()
    conn.close()
    with pytest.raises(
        MonitorDbUnavailable, match="predate the credential store"
    ):
        read_monitor_credentials(db)


def test_non_missing_table_error_omits_predate_hint(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """A non-missing-table sqlite3.Error must not claim a missing table.

    Represents the hot-WAL-without-shm family: a real ``OperationalError`` on
    the SELECT whose cause is not a missing table. The narrowed handler must
    surface the underlying error without the misleading "predate" hint.
    """
    db = make_monitor_db(
        tmp_path / "usage.db",
        [{"label": "x@example.com", "email": "x@example.com", "token": "tok"}],
    )

    real_connect = sqlite3.connect

    class _BoomOnExecute:
        def __init__(self, inner: sqlite3.Connection) -> None:
            self._inner = inner

        def execute(self, *args: object, **kwargs: object) -> object:
            raise sqlite3.OperationalError("database is locked")

        def close(self) -> None:
            self._inner.close()

    def fake_connect(*args: object, **kwargs: object) -> object:
        return _BoomOnExecute(real_connect(*args, **kwargs))

    monkeypatch.setattr(monitor_db_mod.sqlite3, "connect", fake_connect)

    with pytest.raises(MonitorDbUnavailable) as excinfo:
        read_monitor_credentials(db)

    message = str(excinfo.value)
    assert "database is locked" in message
    assert "predate the credential store" not in message


# --------------------------------------------------------------------------
# Filename derivation
# --------------------------------------------------------------------------


def test_filenames_match_bootstrap_derivation(monitor_db: Path) -> None:
    """Same derivation as bootstrap, so identities don't fork across paths."""
    accounts = credentials_to_accounts(read_monitor_credentials(monitor_db))
    assert {a.file for a in accounts} == {
        "robb-2amlogic.token",
        "agent1-2amlogic.token",
    }
    assert {a.source for a in accounts} == {"monitor-db"}


def test_colliding_derived_filenames_raise(tmp_path: Path) -> None:
    db = make_monitor_db(
        tmp_path / "usage.db",
        [
            {"label": "a.jones@x.com", "email": "a.jones@x.com", "token": "t1"},
            {"label": "ajones@x.com", "email": "ajones@x.com", "token": "t2"},
        ],
    )
    with pytest.raises(ValueError, match="duplicate token filename"):
        import_from_monitor(tmp_path / "pool", db_path=db)


# --------------------------------------------------------------------------
# import_from_monitor — materialization
# --------------------------------------------------------------------------


def test_import_writes_tokens_and_manifest(monitor_db: Path, tmp_path: Path) -> None:
    pool = tmp_path / "pool"
    result = import_from_monitor(pool, db_path=monitor_db)

    assert sorted(result.written) == ["agent1-2amlogic.token", "robb-2amlogic.token"]
    assert (pool / "robb-2amlogic.token").read_text() == "sk-ant-oat01-fresh-robb"
    assert (pool / "agent1-2amlogic.token").read_text() == "sk-ant-oat01-fresh-agent1"

    manifest = json.loads((pool / "index.json").read_text())
    assert {a["source"] for a in manifest["accounts"]} == {"monitor-db"}
    assert {a["email"] for a in manifest["accounts"]} == {
        "robb@2amlogic.com",
        "agent-1@2amlogic.com",
    }
    # The manifest must never carry secret material.
    blob = json.dumps(manifest)
    assert "sk-ant-oat01-fresh-robb" not in blob
    assert "sk-ant-oat01-fresh-agent1" not in blob


def test_token_and_directory_modes(monitor_db: Path, tmp_path: Path) -> None:
    pool = tmp_path / "pool"
    import_from_monitor(pool, db_path=monitor_db)
    assert stat.S_IMODE((pool / "robb-2amlogic.token").stat().st_mode) == 0o600
    assert stat.S_IMODE(pool.stat().st_mode) == 0o700


def test_import_is_idempotent(monitor_db: Path, tmp_path: Path) -> None:
    pool = tmp_path / "pool"
    import_from_monitor(pool, db_path=monitor_db)
    second = import_from_monitor(pool, db_path=monitor_db)
    assert second.written == []
    assert sorted(second.unchanged) == [
        "agent1-2amlogic.token",
        "robb-2amlogic.token",
    ]


def test_dry_run_writes_nothing(monitor_db: Path, tmp_path: Path) -> None:
    pool = tmp_path / "pool"
    result = import_from_monitor(pool, db_path=monitor_db, dry_run=True)
    assert sorted(result.written) == ["agent1-2amlogic.token", "robb-2amlogic.token"]
    assert not pool.exists()


def test_rolled_token_reports_drift_and_is_not_applied(
    monitor_db: Path, tmp_path: Path
) -> None:
    """Without --force a stale pool stays stale — and says so."""
    pool = tmp_path / "pool"
    pool.mkdir()
    (pool / "robb-2amlogic.token").write_text("sk-ant-oat01-REVOKED")

    result = import_from_monitor(pool, db_path=monitor_db)

    assert result.drifted == ["robb-2amlogic.token"]
    assert (pool / "robb-2amlogic.token").read_text() == "sk-ant-oat01-REVOKED"


def test_force_applies_rolled_tokens(monitor_db: Path, tmp_path: Path) -> None:
    """The core regression: --force replaces revoked tokens with live ones."""
    pool = tmp_path / "pool"
    pool.mkdir()
    (pool / "robb-2amlogic.token").write_text("sk-ant-oat01-REVOKED")

    result = import_from_monitor(pool, db_path=monitor_db, force=True)

    assert result.drifted == []
    assert (pool / "robb-2amlogic.token").read_text() == "sk-ant-oat01-fresh-robb"


def test_no_active_accounts_is_not_an_error(tmp_path: Path) -> None:
    db = make_monitor_db(
        tmp_path / "usage.db",
        [{"label": "x@example.com", "email": "x@example.com", "token": "t",
          "is_active": 0}],
    )
    result = import_from_monitor(tmp_path / "pool", db_path=db)
    assert result.written == []
    assert result.effective == []


# --------------------------------------------------------------------------
# Pruning
# --------------------------------------------------------------------------


def test_prune_removes_only_inactive_accounts(
    monitor_db: Path, tmp_path: Path
) -> None:
    pool = tmp_path / "pool"
    pool.mkdir()
    (pool / "gone-example.token").write_text("stale")

    result = import_from_monitor(pool, db_path=monitor_db, prune=True)

    assert result.pruned == ["gone-example.token"]
    assert not (pool / "gone-example.token").exists()
    assert (pool / "robb-2amlogic.token").exists()


def test_prune_is_off_by_default(monitor_db: Path, tmp_path: Path) -> None:
    pool = tmp_path / "pool"
    pool.mkdir()
    (pool / "other-example.token").write_text("provisioned elsewhere")

    result = import_from_monitor(pool, db_path=monitor_db)

    assert result.pruned == []
    assert (pool / "other-example.token").exists()


def test_prune_never_touches_pool_state_files(
    monitor_db: Path, tmp_path: Path
) -> None:
    """Rotation state must survive a prune (#3938: one pool, one truth)."""
    pool = tmp_path / "pool"
    pool.mkdir()
    for name in (".ranking", ".bad_tokens", ".failure_counts", ".allowlist"):
        (pool / name).write_text("state")

    import_from_monitor(pool, db_path=monitor_db, prune=True)

    for name in (".ranking", ".bad_tokens", ".failure_counts", ".allowlist"):
        assert (pool / name).read_text() == "state"


def test_prune_dry_run_reports_without_deleting(
    monitor_db: Path, tmp_path: Path
) -> None:
    pool = tmp_path / "pool"
    pool.mkdir()
    (pool / "gone-example.token").write_text("stale")

    result = import_from_monitor(pool, db_path=monitor_db, prune=True, dry_run=True)

    assert result.pruned == ["gone-example.token"]
    assert (pool / "gone-example.token").exists()


# --------------------------------------------------------------------------
# CLI surface
# --------------------------------------------------------------------------


def test_cli_imports_into_shared_pool(
    monitor_db: Path, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    shared = tmp_path / "shared-pool"
    monkeypatch.setenv("LOOM_SHARED_TOKENS_DIR", str(shared))

    rc = main(["import-from-monitor", "--shared", "--db", str(monitor_db)])

    assert rc == 0
    assert (shared / "robb-2amlogic.token").read_text() == "sk-ant-oat01-fresh-robb"


def test_cli_exits_2_on_unapplied_drift(
    monitor_db: Path, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """Scripts can detect "pool still stale" without parsing logs."""
    shared = tmp_path / "shared-pool"
    shared.mkdir()
    (shared / "robb-2amlogic.token").write_text("sk-ant-oat01-REVOKED")
    monkeypatch.setenv("LOOM_SHARED_TOKENS_DIR", str(shared))

    rc = main(["import-from-monitor", "--shared", "--db", str(monitor_db)])
    assert rc == 2

    rc_forced = main(
        ["import-from-monitor", "--shared", "--force", "--db", str(monitor_db)]
    )
    assert rc_forced == 0


def test_cli_reports_missing_store(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    monkeypatch.setenv("LOOM_SHARED_TOKENS_DIR", str(tmp_path / "pool"))
    rc = main(
        ["import-from-monitor", "--shared", "--db", str(tmp_path / "absent.db")]
    )
    assert rc == 1


def test_cli_refuses_shared_when_disabled(
    monitor_db: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    monkeypatch.setenv("LOOM_SHARED_TOKENS_DIR", "")
    rc = main(["import-from-monitor", "--shared", "--db", str(monitor_db)])
    assert rc == 1
