"""Tests for loom_tools.tokens.bootstrap."""

from __future__ import annotations

import json
import os
import pathlib
import sys

import pytest

from loom_tools.common.repo import clear_repo_cache
from loom_tools.tokens.bootstrap import (
    INDEX_VERSION,
    BootstrapResult,
    _strip_value,
    bootstrap_tokens,
    fingerprint,
    parse_env_accounts,
)
from loom_tools.tokens.cli import main as cli_main


@pytest.fixture
def mock_repo(tmp_path: pathlib.Path) -> pathlib.Path:
    """Create a mock repo with .git and .loom directories."""
    clear_repo_cache()
    (tmp_path / ".git").mkdir()
    (tmp_path / ".loom").mkdir()
    return tmp_path


def _write_env(repo: pathlib.Path, body: str) -> pathlib.Path:
    env = repo / ".env"
    env.write_text(body, encoding="utf-8")
    return env


# ---------------------------------------------------------------------------
# parse_env_accounts / _strip_value
# ---------------------------------------------------------------------------


class TestStripValue:
    def test_plain(self) -> None:
        assert _strip_value("hello") == "hello"

    def test_double_quoted(self) -> None:
        assert _strip_value('"hello"') == "hello"

    def test_single_quoted(self) -> None:
        assert _strip_value("'hello'") == "hello"

    def test_trims_whitespace(self) -> None:
        assert _strip_value("  hello  ") == "hello"

    def test_strips_embedded_newline(self) -> None:
        # Mirrors lean-genius `tr -d "'\"\n"` behavior.
        assert _strip_value("hel\nlo") == "hello"

    def test_strips_embedded_quotes(self) -> None:
        assert _strip_value('he"l"lo') == "hello"


class TestParseEnv:
    def test_complete_triple(self, mock_repo: pathlib.Path) -> None:
        env = _write_env(
            mock_repo,
            "ACCOUNT_EMAIL_1=a@b.com\n"
            "ACCOUNT_KEY_1=sk-ant-oat01-aaa\n"
            "ACCOUNT_TOKEN_FILE_1=alice.token\n",
        )
        accounts = parse_env_accounts(env)
        assert set(accounts) == {1}
        assert accounts[1] == {
            "email": "a@b.com",
            "key": "sk-ant-oat01-aaa",
            "file": "alice.token",
        }

    def test_multiple_accounts(self, mock_repo: pathlib.Path) -> None:
        env = _write_env(
            mock_repo,
            "ACCOUNT_EMAIL_1=a@b.com\n"
            "ACCOUNT_KEY_1=sk-1\n"
            "ACCOUNT_TOKEN_FILE_1=a.token\n"
            "ACCOUNT_EMAIL_2=c@d.com\n"
            "ACCOUNT_KEY_2=sk-2\n"
            "ACCOUNT_TOKEN_FILE_2=c.token\n",
        )
        accounts = parse_env_accounts(env)
        assert set(accounts) == {1, 2}

    def test_gap_in_numbering(self, mock_repo: pathlib.Path) -> None:
        # Per AC: gaps must not error.
        env = _write_env(
            mock_repo,
            "ACCOUNT_EMAIL_1=a@b.com\n"
            "ACCOUNT_KEY_1=sk-1\n"
            "ACCOUNT_TOKEN_FILE_1=a.token\n"
            "ACCOUNT_EMAIL_3=c@d.com\n"
            "ACCOUNT_KEY_3=sk-3\n"
            "ACCOUNT_TOKEN_FILE_3=c.token\n",
        )
        accounts = parse_env_accounts(env)
        assert set(accounts) == {1, 3}

    def test_partial_triple_returned(self, mock_repo: pathlib.Path) -> None:
        # Parser returns partial triples; bootstrap will warn and skip.
        env = _write_env(mock_repo, "ACCOUNT_EMAIL_5=a@b.com\n")
        accounts = parse_env_accounts(env)
        assert accounts == {5: {"email": "a@b.com"}}

    def test_ignores_unrelated_lines(self, mock_repo: pathlib.Path) -> None:
        env = _write_env(
            mock_repo,
            "# comment\n"
            "OTHER_VAR=xyz\n"
            "ACCOUNT_EMAIL_1=a@b.com\n"
            "ACCOUNT_KEY_1=sk-1\n"
            "ACCOUNT_TOKEN_FILE_1=a.token\n",
        )
        accounts = parse_env_accounts(env)
        assert set(accounts) == {1}

    def test_quoted_values(self, mock_repo: pathlib.Path) -> None:
        env = _write_env(
            mock_repo,
            'ACCOUNT_EMAIL_1="a@b.com"\n'
            "ACCOUNT_KEY_1='sk-1'\n"
            "ACCOUNT_TOKEN_FILE_1=a.token\n",
        )
        accounts = parse_env_accounts(env)
        assert accounts[1]["email"] == "a@b.com"
        assert accounts[1]["key"] == "sk-1"


# ---------------------------------------------------------------------------
# fingerprint
# ---------------------------------------------------------------------------


def test_fingerprint_is_8_hex_chars() -> None:
    fp = fingerprint("sk-ant-oat01-something")
    assert len(fp) == 8
    assert all(c in "0123456789abcdef" for c in fp)


def test_fingerprint_changes_on_drift() -> None:
    assert fingerprint("a") != fingerprint("b")


def test_fingerprint_stable() -> None:
    assert fingerprint("same") == fingerprint("same")


# ---------------------------------------------------------------------------
# bootstrap_tokens
# ---------------------------------------------------------------------------


def _make_env(repo: pathlib.Path, n_accounts: int = 2) -> pathlib.Path:
    lines = []
    for i in range(1, n_accounts + 1):
        lines.append(f"ACCOUNT_EMAIL_{i}=user{i}@example.com")
        lines.append(f"ACCOUNT_KEY_{i}=sk-ant-oat01-key{i}")
        lines.append(f"ACCOUNT_TOKEN_FILE_{i}=user{i}.token")
    return _write_env(repo, "\n".join(lines) + "\n")


class TestBootstrap:
    def test_creates_token_files(self, mock_repo: pathlib.Path) -> None:
        _make_env(mock_repo, 2)
        result = bootstrap_tokens(mock_repo)
        tokens_dir = mock_repo / ".loom" / "tokens"
        assert (tokens_dir / "user1.token").read_text() == "sk-ant-oat01-key1"
        assert (tokens_dir / "user2.token").read_text() == "sk-ant-oat01-key2"
        assert sorted(result.written) == ["user1.token", "user2.token"]
        assert result.unchanged == []
        assert result.drifted == []

    @pytest.mark.skipif(sys.platform == "win32", reason="POSIX modes only")
    def test_token_file_mode_0600(self, mock_repo: pathlib.Path) -> None:
        _make_env(mock_repo, 1)
        bootstrap_tokens(mock_repo)
        token_path = mock_repo / ".loom" / "tokens" / "user1.token"
        mode = os.stat(token_path).st_mode & 0o777
        assert mode == 0o600

    def test_writes_index_json(self, mock_repo: pathlib.Path) -> None:
        _make_env(mock_repo, 2)
        bootstrap_tokens(mock_repo)
        idx_path = mock_repo / ".loom" / "tokens" / "index.json"
        assert idx_path.exists()
        idx = json.loads(idx_path.read_text())
        assert idx["version"] == INDEX_VERSION
        assert "generated_at" in idx
        assert len(idx["accounts"]) == 2
        # No secret material — only fingerprints.
        for acct in idx["accounts"]:
            assert "key" not in acct
            assert "ACCOUNT_KEY" not in str(acct)
            assert len(acct["key_fingerprint"]) == 8
            assert acct["env_index"] in (1, 2)
            assert acct["file"].endswith(".token")
            assert "email" in acct
            assert acct["name"] == acct["file"].removesuffix(".token")

    def test_idempotent_unchanged(self, mock_repo: pathlib.Path) -> None:
        _make_env(mock_repo, 2)
        bootstrap_tokens(mock_repo)
        result = bootstrap_tokens(mock_repo)
        assert result.written == []
        assert sorted(result.unchanged) == ["user1.token", "user2.token"]
        assert result.drifted == []

    def test_drift_detected_without_force(self, mock_repo: pathlib.Path) -> None:
        _make_env(mock_repo, 1)
        bootstrap_tokens(mock_repo)
        token_path = mock_repo / ".loom" / "tokens" / "user1.token"
        token_path.write_text("manually-edited", encoding="utf-8")

        result = bootstrap_tokens(mock_repo)
        assert result.drifted == ["user1.token"]
        assert result.written == []
        # Disk content is preserved when drift is detected w/o --force.
        assert token_path.read_text() == "manually-edited"

    def test_force_overwrites_drift(self, mock_repo: pathlib.Path) -> None:
        _make_env(mock_repo, 1)
        bootstrap_tokens(mock_repo)
        token_path = mock_repo / ".loom" / "tokens" / "user1.token"
        token_path.write_text("manually-edited", encoding="utf-8")

        result = bootstrap_tokens(mock_repo, force=True)
        assert result.drifted == []
        assert result.written == ["user1.token"]
        assert token_path.read_text() == "sk-ant-oat01-key1"

    def test_dry_run_writes_nothing(self, mock_repo: pathlib.Path) -> None:
        _make_env(mock_repo, 2)
        result = bootstrap_tokens(mock_repo, dry_run=True)
        tokens_dir = mock_repo / ".loom" / "tokens"
        assert sorted(result.written) == ["user1.token", "user2.token"]
        # Nothing materialized.
        assert not tokens_dir.exists() or list(tokens_dir.iterdir()) == []

    def test_partial_triple_skipped(self, mock_repo: pathlib.Path) -> None:
        env_body = (
            "ACCOUNT_EMAIL_1=a@b.com\n"
            "ACCOUNT_KEY_1=sk-1\n"
            "ACCOUNT_TOKEN_FILE_1=a.token\n"
            # _2 is missing TOKEN_FILE
            "ACCOUNT_EMAIL_2=c@d.com\n"
            "ACCOUNT_KEY_2=sk-2\n"
        )
        _write_env(mock_repo, env_body)
        result = bootstrap_tokens(mock_repo)
        assert result.written == ["a.token"]
        # _2 was silently dropped (with a warning).
        tokens_dir = mock_repo / ".loom" / "tokens"
        assert not (tokens_dir / "c@d.token").exists()

    def test_unsafe_filename_skipped(self, mock_repo: pathlib.Path) -> None:
        env_body = (
            "ACCOUNT_EMAIL_1=a@b.com\n"
            "ACCOUNT_KEY_1=sk-1\n"
            "ACCOUNT_TOKEN_FILE_1=../escape.token\n"
        )
        _write_env(mock_repo, env_body)
        result = bootstrap_tokens(mock_repo)
        assert result.written == []

    def test_duplicate_filenames_aborts(self, mock_repo: pathlib.Path) -> None:
        env_body = (
            "ACCOUNT_EMAIL_1=a@b.com\n"
            "ACCOUNT_KEY_1=sk-1\n"
            "ACCOUNT_TOKEN_FILE_1=same.token\n"
            "ACCOUNT_EMAIL_2=c@d.com\n"
            "ACCOUNT_KEY_2=sk-2\n"
            "ACCOUNT_TOKEN_FILE_2=same.token\n"
        )
        _write_env(mock_repo, env_body)
        with pytest.raises(ValueError, match="duplicate"):
            bootstrap_tokens(mock_repo)

    def test_missing_env_raises(self, mock_repo: pathlib.Path) -> None:
        with pytest.raises(FileNotFoundError):
            bootstrap_tokens(mock_repo)

    def test_no_accounts_returns_empty(self, mock_repo: pathlib.Path) -> None:
        _write_env(mock_repo, "OTHER_VAR=xyz\n")
        result = bootstrap_tokens(mock_repo)
        assert result.written == []
        assert result.unchanged == []
        assert result.drifted == []

    def test_index_records_drift_flag(self, mock_repo: pathlib.Path) -> None:
        _make_env(mock_repo, 1)
        bootstrap_tokens(mock_repo)
        token_path = mock_repo / ".loom" / "tokens" / "user1.token"
        token_path.write_text("manually-edited", encoding="utf-8")
        bootstrap_tokens(mock_repo)
        idx = json.loads(
            (mock_repo / ".loom" / "tokens" / "index.json").read_text()
        )
        drifted_entry = idx["accounts"][0]
        assert drifted_entry.get("drift") is True
        assert "env_fingerprint" in drifted_entry


# ---------------------------------------------------------------------------
# CLI surface
# ---------------------------------------------------------------------------


class TestCli:
    def test_help_exits_zero(self, capsys: pytest.CaptureFixture[str]) -> None:
        with pytest.raises(SystemExit) as exc:
            cli_main(["--help"])
        assert exc.value.code == 0

    def test_bootstrap_help_exits_zero(self) -> None:
        with pytest.raises(SystemExit) as exc:
            cli_main(["bootstrap", "--help"])
        assert exc.value.code == 0

    def test_bootstrap_dry_run_via_cli(
        self,
        mock_repo: pathlib.Path,
        monkeypatch: pytest.MonkeyPatch,
        capsys: pytest.CaptureFixture[str],
    ) -> None:
        _make_env(mock_repo, 1)
        monkeypatch.chdir(mock_repo)
        rc = cli_main(["bootstrap", "--dry-run", "--json"])
        assert rc == 0
        out = capsys.readouterr().out
        payload = json.loads(out)
        assert payload["dry_run"] is True
        assert payload["written"] == ["user1.token"]

    def test_bootstrap_drift_returns_2(
        self,
        mock_repo: pathlib.Path,
        monkeypatch: pytest.MonkeyPatch,
    ) -> None:
        _make_env(mock_repo, 1)
        monkeypatch.chdir(mock_repo)
        cli_main(["bootstrap"])
        # Tamper with disk to create drift.
        token_path = mock_repo / ".loom" / "tokens" / "user1.token"
        token_path.write_text("tampered", encoding="utf-8")
        rc = cli_main(["bootstrap"])
        assert rc == 2

    def test_bootstrap_force_clears_drift(
        self,
        mock_repo: pathlib.Path,
        monkeypatch: pytest.MonkeyPatch,
    ) -> None:
        _make_env(mock_repo, 1)
        monkeypatch.chdir(mock_repo)
        cli_main(["bootstrap"])
        token_path = mock_repo / ".loom" / "tokens" / "user1.token"
        token_path.write_text("tampered", encoding="utf-8")
        rc = cli_main(["bootstrap", "--force"])
        assert rc == 0
        assert token_path.read_text() == "sk-ant-oat01-key1"

    def test_missing_env_returns_1(
        self,
        mock_repo: pathlib.Path,
        monkeypatch: pytest.MonkeyPatch,
    ) -> None:
        monkeypatch.chdir(mock_repo)
        rc = cli_main(["bootstrap"])
        assert rc == 1


# ---------------------------------------------------------------------------
# Result struct
# ---------------------------------------------------------------------------


def test_bootstrap_result_to_dict_roundtrip() -> None:
    r = BootstrapResult()
    r.written = ["a.token"]
    r.unchanged = ["b.token"]
    r.drifted = []
    r.skipped = []
    r.dry_run = True
    d = r.to_dict()
    # Ensure JSON-serializable.
    assert json.dumps(d)
    assert d["written"] == ["a.token"]
    assert d["dry_run"] is True
