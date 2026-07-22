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
    Account,
    BootstrapResult,
    _TOKEN_FILE_RE,
    _strip_value,
    bootstrap_tokens,
    default_claude_monitor_accounts_env,
    default_home_accounts_env,
    derive_token_filename,
    fingerprint,
    merge_accounts,
    parse_env_accounts,
    resolve_repo_env,
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
        # A triple missing a non-derivable field (the KEY) is still skipped.
        # (Missing TOKEN_FILE is now auto-derived from the email, #3697 — see
        # TestDeriveTokenFilename / test_token_file_auto_derived_from_email.)
        env_body = (
            "ACCOUNT_EMAIL_1=a@b.com\n"
            "ACCOUNT_KEY_1=sk-1\n"
            "ACCOUNT_TOKEN_FILE_1=a.token\n"
            # _2 is missing KEY (cannot be derived) -> skipped
            "ACCOUNT_EMAIL_2=c@d.com\n"
            "ACCOUNT_TOKEN_FILE_2=c.token\n"
        )
        _write_env(mock_repo, env_body)
        result = bootstrap_tokens(mock_repo)
        assert result.written == ["a.token"]
        # _2 was silently dropped (with a warning).
        tokens_dir = mock_repo / ".loom" / "tokens"
        assert not (tokens_dir / "c.token").exists()

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
    # #3695 fields present and JSON-safe.
    assert d["effective"] == []
    assert d["home_env"] is None
    assert d["repo_env"] is None


# ---------------------------------------------------------------------------
# #3695: home-dir master + per-repo override
# ---------------------------------------------------------------------------


def _acct(email: str, key: str, file: str, source: str, index: int = 1) -> Account:
    return Account(email=email, key=key, file=file, source=source, index=index)


def _triple(i: int, email: str, key: str, file: str) -> str:
    return (
        f"ACCOUNT_EMAIL_{i}={email}\n"
        f"ACCOUNT_KEY_{i}={key}\n"
        f"ACCOUNT_TOKEN_FILE_{i}={file}\n"
    )


class TestHomeMasterResolution:
    def test_default_is_home_loom(self, monkeypatch: pytest.MonkeyPatch) -> None:
        monkeypatch.delenv("LOOM_ACCOUNTS_ENV", raising=False)
        p = default_home_accounts_env()
        assert p is not None
        assert p.name == "accounts.env"
        assert p.parent.name == ".loom"

    def test_env_override(self, monkeypatch: pytest.MonkeyPatch) -> None:
        monkeypatch.setenv("LOOM_ACCOUNTS_ENV", "/custom/accts.env")
        assert default_home_accounts_env() == pathlib.Path("/custom/accts.env")

    def test_empty_env_disables(self, monkeypatch: pytest.MonkeyPatch) -> None:
        monkeypatch.setenv("LOOM_ACCOUNTS_ENV", "")
        assert default_home_accounts_env() is None


class TestRepoEnvResolution:
    def test_explicit_env_wins(self, mock_repo: pathlib.Path) -> None:
        explicit = mock_repo / "custom.env"
        assert resolve_repo_env(mock_repo, explicit) == explicit

    def test_dedicated_preferred_over_legacy(self, mock_repo: pathlib.Path) -> None:
        (mock_repo / ".env").write_text("x\n")
        dedicated = mock_repo / ".loom" / "accounts.env"
        dedicated.write_text("y\n")
        assert resolve_repo_env(mock_repo, None) == dedicated

    def test_legacy_fallback(self, mock_repo: pathlib.Path) -> None:
        # No dedicated file -> legacy .env path, even if it does not exist.
        assert resolve_repo_env(mock_repo, None) == mock_repo / ".env"


class TestMergeAccounts:
    def test_home_only(self) -> None:
        merged = merge_accounts([_acct("a@x", "k", "a.token", "home")], [])
        assert [m.source for m in merged] == ["home"]

    def test_repo_adds_new_email(self) -> None:
        home = [_acct("a@x", "k1", "a.token", "home")]
        repo = [_acct("b@x", "k2", "b.token", "repo")]
        merged = merge_accounts(home, repo)
        assert [(m.email, m.source) for m in merged] == [
            ("a@x", "home"),
            ("b@x", "repo"),
        ]

    def test_repo_overrides_same_email(self) -> None:
        home = [_acct("a@x", "home-key", "a.token", "home")]
        repo = [_acct("a@x", "repo-key", "a.token", "repo")]
        merged = merge_accounts(home, repo)
        assert len(merged) == 1
        assert merged[0].source == "repo-override"
        assert merged[0].key == "repo-key"

    def test_override_is_case_insensitive_on_email(self) -> None:
        home = [_acct("User@X.com", "hk", "a.token", "home")]
        repo = [_acct("user@x.com", "rk", "a.token", "repo")]
        merged = merge_accounts(home, repo)
        assert len(merged) == 1
        assert merged[0].key == "rk"

    def test_ordering_home_first_then_repo(self) -> None:
        home = [
            _acct("a@x", "k", "a.token", "home", 1),
            _acct("b@x", "k", "b.token", "home", 2),
        ]
        repo = [_acct("c@x", "k", "c.token", "repo", 1)]
        merged = merge_accounts(home, repo)
        assert [m.email for m in merged] == ["a@x", "b@x", "c@x"]


class TestBootstrapMerge:
    def _write_home(self, tmp_path: pathlib.Path, body: str) -> pathlib.Path:
        home = tmp_path / "home-accounts.env"
        home.write_text(body, encoding="utf-8")
        return home

    def test_home_master_materialized(self, mock_repo: pathlib.Path) -> None:
        home = self._write_home(
            mock_repo, _triple(1, "a@x", "sk-home-a", "alice.token")
        )
        # Repo has no source of its own — relies entirely on the master.
        result = bootstrap_tokens(mock_repo, home_env_path=home)
        tokens_dir = mock_repo / ".loom" / "tokens"
        assert (tokens_dir / "alice.token").read_text() == "sk-home-a"
        assert result.written == ["alice.token"]
        assert result.effective == [
            {
                "email": "a@x",
                "name": "alice",
                "file": "alice.token",
                "source": "home",
            }
        ]

    def test_repo_overrides_master_key(self, mock_repo: pathlib.Path) -> None:
        home = self._write_home(
            mock_repo, _triple(1, "a@x", "sk-home", "alice.token")
        )
        _write_env(mock_repo, _triple(1, "a@x", "sk-repo", "alice.token"))
        bootstrap_tokens(mock_repo, home_env_path=home)
        tokens_dir = mock_repo / ".loom" / "tokens"
        # Repo key wins for the shared email.
        assert (tokens_dir / "alice.token").read_text() == "sk-repo"

    def test_repo_adds_account_to_master(self, mock_repo: pathlib.Path) -> None:
        home = self._write_home(
            mock_repo, _triple(1, "a@x", "sk-home", "alice.token")
        )
        _write_env(mock_repo, _triple(1, "b@x", "sk-repo", "bob.token"))
        result = bootstrap_tokens(mock_repo, home_env_path=home)
        tokens_dir = mock_repo / ".loom" / "tokens"
        assert (tokens_dir / "alice.token").read_text() == "sk-home"
        assert (tokens_dir / "bob.token").read_text() == "sk-repo"
        sources = {e["email"]: e["source"] for e in result.effective}
        assert sources == {"a@x": "home", "b@x": "repo"}

    def test_no_home_flag_ignores_master(self, mock_repo: pathlib.Path) -> None:
        home = self._write_home(
            mock_repo, _triple(1, "a@x", "sk-home", "alice.token")
        )
        _write_env(mock_repo, _triple(1, "b@x", "sk-repo", "bob.token"))
        # Passing home_env_path=None disables the master for this call.
        result = bootstrap_tokens(mock_repo, home_env_path=None)
        assert result.written == ["bob.token"]
        assert home  # keep ref; unused on this path

    def test_neither_source_raises(self, mock_repo: pathlib.Path) -> None:
        with pytest.raises(FileNotFoundError):
            bootstrap_tokens(mock_repo, home_env_path=None)

    def test_dedicated_repo_file_used(self, mock_repo: pathlib.Path) -> None:
        (mock_repo / ".loom" / "accounts.env").write_text(
            _triple(1, "a@x", "sk-dedicated", "alice.token"), encoding="utf-8"
        )
        # Legacy .env has a different key that must be ignored in favour of the
        # dedicated file.
        _write_env(mock_repo, _triple(1, "a@x", "sk-legacy", "alice.token"))
        bootstrap_tokens(mock_repo, home_env_path=None)
        assert (
            mock_repo / ".loom" / "tokens" / "alice.token"
        ).read_text() == "sk-dedicated"

    def test_cross_source_duplicate_filename_aborts(
        self, mock_repo: pathlib.Path
    ) -> None:
        home = self._write_home(
            mock_repo, _triple(1, "a@x", "sk-home", "shared.token")
        )
        # Different email, same token filename -> collision on disk.
        _write_env(mock_repo, _triple(1, "b@x", "sk-repo", "shared.token"))
        with pytest.raises(ValueError, match="duplicate"):
            bootstrap_tokens(mock_repo, home_env_path=home)

    def test_env_var_master_resolution(
        self, mock_repo: pathlib.Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        home = self._write_home(
            mock_repo, _triple(1, "a@x", "sk-env", "alice.token")
        )
        monkeypatch.setenv("LOOM_ACCOUNTS_ENV", str(home))
        # No explicit home_env_path -> default resolution reads LOOM_ACCOUNTS_ENV.
        result = bootstrap_tokens(mock_repo)
        assert result.written == ["alice.token"]

    def test_manifest_records_source(self, mock_repo: pathlib.Path) -> None:
        home = self._write_home(
            mock_repo, _triple(1, "a@x", "sk-home", "alice.token")
        )
        _write_env(mock_repo, _triple(1, "b@x", "sk-repo", "bob.token"))
        bootstrap_tokens(mock_repo, home_env_path=home)
        idx = json.loads(
            (mock_repo / ".loom" / "tokens" / "index.json").read_text()
        )
        by_email = {a["email"]: a["source"] for a in idx["accounts"]}
        assert by_email == {"a@x": "home", "b@x": "repo"}


class TestCliMerge:
    def test_no_home_flag_via_cli(
        self,
        mock_repo: pathlib.Path,
        monkeypatch: pytest.MonkeyPatch,
        capsys: pytest.CaptureFixture[str],
    ) -> None:
        (mock_repo / "home.env").write_text(
            _triple(1, "a@x", "sk-home", "alice.token"), encoding="utf-8"
        )
        _write_env(mock_repo, _triple(1, "b@x", "sk-repo", "bob.token"))
        monkeypatch.setenv("LOOM_ACCOUNTS_ENV", str(mock_repo / "home.env"))
        monkeypatch.chdir(mock_repo)
        rc = cli_main(["bootstrap", "--no-home", "--json"])
        assert rc == 0
        payload = json.loads(capsys.readouterr().out)
        assert payload["written"] == ["bob.token"]

    def test_effective_report_printed(
        self,
        mock_repo: pathlib.Path,
        monkeypatch: pytest.MonkeyPatch,
        capsys: pytest.CaptureFixture[str],
    ) -> None:
        (mock_repo / "home.env").write_text(
            _triple(1, "a@x", "sk-home", "alice.token"), encoding="utf-8"
        )
        monkeypatch.setenv("LOOM_ACCOUNTS_ENV", str(mock_repo / "home.env"))
        monkeypatch.chdir(mock_repo)
        rc = cli_main(["bootstrap", "--dry-run"])
        assert rc == 0
        out = capsys.readouterr().out
        assert "Effective accounts" in out
        assert "alice" in out
        assert "home" in out


class TestDeriveTokenFilename:
    """Auto-derive ACCOUNT_TOKEN_FILE_N from email when omitted (#3697)."""

    def test_convention_examples(self) -> None:
        # Established naming convention (generic example domains).
        assert derive_token_filename("alice@example.com") == "alice-example.token"
        assert (
            derive_token_filename("a.b.jones@example.org") == "abjones-example.token"
        )
        assert derive_token_filename("agent-1@example.com") == "agent1-example.token"

    def test_result_always_passes_safety_regex(self) -> None:
        for email in (
            "user@example.com",
            "a.b.c@sub.example.co.uk",
            "weird+tag@example.com",
            "UPPER@Example.COM",
            "n@d",
        ):
            derived = derive_token_filename(email)
            assert _TOKEN_FILE_RE.match(derived), derived
            assert "/" not in derived and "\\" not in derived

    def test_no_at_sign_still_safe(self) -> None:
        derived = derive_token_filename("localonly")
        assert _TOKEN_FILE_RE.match(derived)

    def test_two_emails_can_collide_to_same_stem(self) -> None:
        # This is intentional: the duplicate-filename guard catches it.
        assert derive_token_filename("ajones@example.com") == derive_token_filename(
            "a.jones@example.com"
        )


class TestBootstrapAutoDerive:
    def test_token_file_auto_derived_from_email(
        self, mock_repo: pathlib.Path
    ) -> None:
        # EMAIL + KEY only (claude-monitor-style) -> file derived.
        _write_env(
            mock_repo,
            "ACCOUNT_EMAIL_1=alice@example.com\nACCOUNT_KEY_1=sk-1\n",
        )
        result = bootstrap_tokens(mock_repo)
        assert result.written == ["alice-example.token"]
        tokens_dir = mock_repo / ".loom" / "tokens"
        assert (tokens_dir / "alice-example.token").read_text() == "sk-1"

    def test_explicit_file_still_wins(self, mock_repo: pathlib.Path) -> None:
        _write_env(
            mock_repo,
            "ACCOUNT_EMAIL_1=alice@example.com\n"
            "ACCOUNT_KEY_1=sk-1\n"
            "ACCOUNT_TOKEN_FILE_1=explicit.token\n",
        )
        result = bootstrap_tokens(mock_repo)
        assert result.written == ["explicit.token"]

    def test_derived_collision_aborts(self, mock_repo: pathlib.Path) -> None:
        # Two distinct emails that sanitize to the same stem must be caught by
        # the existing duplicate-filename guard, not silently merged.
        _write_env(
            mock_repo,
            "ACCOUNT_EMAIL_1=ajones@example.com\nACCOUNT_KEY_1=sk-1\n"
            "ACCOUNT_EMAIL_2=a.jones@example.com\nACCOUNT_KEY_2=sk-2\n",
        )
        with pytest.raises(ValueError, match="duplicate token filename"):
            bootstrap_tokens(mock_repo)


class TestBackwardCompat3697:
    """Absent claude-monitor, bootstrap behaves exactly as on #3695."""

    def test_loom_accounts_env_still_honored(
        self, mock_repo: pathlib.Path, tmp_path: pathlib.Path, monkeypatch
    ) -> None:
        master = tmp_path / "master-accounts.env"
        master.write_text(
            "ACCOUNT_EMAIL_1=home@example.com\n"
            "ACCOUNT_KEY_1=hk\n"
            "ACCOUNT_TOKEN_FILE_1=home.token\n",
            encoding="utf-8",
        )
        monkeypatch.setenv("LOOM_ACCOUNTS_ENV", str(master))
        result = bootstrap_tokens(mock_repo)
        assert "home.token" in result.written
        assert result.home_env == master

    def test_empty_loom_accounts_env_disables_master(
        self, mock_repo: pathlib.Path, monkeypatch
    ) -> None:
        monkeypatch.setenv("LOOM_ACCOUNTS_ENV", "")
        _write_env(
            mock_repo,
            "ACCOUNT_EMAIL_1=a@example.com\n"
            "ACCOUNT_KEY_1=sk-1\n"
            "ACCOUNT_TOKEN_FILE_1=a.token\n",
        )
        result = bootstrap_tokens(mock_repo)
        assert result.home_env is None
        assert result.written == ["a.token"]

    def test_existing_full_triple_setup_unchanged(
        self, mock_repo: pathlib.Path
    ) -> None:
        # A config that bootstrapped fine pre-#3697 (full triples) is untouched.
        _write_env(
            mock_repo,
            "ACCOUNT_EMAIL_1=a@example.com\n"
            "ACCOUNT_KEY_1=sk-1\n"
            "ACCOUNT_TOKEN_FILE_1=custom-a.token\n",
        )
        result = bootstrap_tokens(mock_repo)
        assert result.written == ["custom-a.token"]


# ---------------------------------------------------------------------------
# #3698: claude-monitor-first account sourcing (additive three-source merge)
# ---------------------------------------------------------------------------


def _pair(i: int, email: str, key: str) -> str:
    """A claude-monitor-style EMAIL+KEY-only entry (no TOKEN_FILE)."""
    return f"ACCOUNT_EMAIL_{i}={email}\nACCOUNT_KEY_{i}={key}\n"


def _write_monitor(
    tmp_path: pathlib.Path,
    monkeypatch: pytest.MonkeyPatch,
    body: str,
) -> pathlib.Path:
    """Materialize a fixture ``~/.claude-monitor/accounts.env`` and point the
    ``LOOM_CLAUDE_MONITOR_DIR`` override at it (the autouse conftest fixture
    otherwise points it at a non-existent path so a real dir never leaks in)."""
    mon_dir = tmp_path / "claude-monitor-home"
    mon_dir.mkdir(exist_ok=True)
    accounts = mon_dir / "accounts.env"
    accounts.write_text(body, encoding="utf-8")
    monkeypatch.setenv("LOOM_CLAUDE_MONITOR_DIR", str(mon_dir))
    return accounts


class TestClaudeMonitorResolver:
    def test_default_dir_honors_env_override(
        self, tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        monkeypatch.setenv("LOOM_CLAUDE_MONITOR_DIR", str(tmp_path / "mon"))
        p = default_claude_monitor_accounts_env()
        assert p == tmp_path / "mon" / "accounts.env"

    def test_default_dir_is_claude_monitor(
        self, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        monkeypatch.delenv("LOOM_CLAUDE_MONITOR_DIR", raising=False)
        p = default_claude_monitor_accounts_env()
        assert p.name == "accounts.env"
        assert p.parent.name == ".claude-monitor"


class TestBackwardCompat3698:
    """The safety arbiter: nothing that resolves today may break."""

    def test_home_only_still_bootstraps(
        self, mock_repo: pathlib.Path, tmp_path: pathlib.Path, monkeypatch
    ) -> None:
        # ARBITER TEST: only ~/.loom/accounts.env, NO claude-monitor file.
        # A pure default-swap to claude-monitor would break this; the additive
        # design must keep it working byte-for-byte.
        master = tmp_path / "loom-accounts.env"
        master.write_text(
            _triple(1, "a@example.com", "hk-a", "alice.token")
            + _triple(2, "b@example.com", "hk-b", "bob.token"),
            encoding="utf-8",
        )
        monkeypatch.setenv("LOOM_ACCOUNTS_ENV", str(master))
        result = bootstrap_tokens(mock_repo)
        assert sorted(result.written) == ["alice.token", "bob.token"]
        assert result.monitor_env is None
        assert result.home_env == master
        tokens_dir = mock_repo / ".loom" / "tokens"
        assert (tokens_dir / "alice.token").read_text() == "hk-a"
        assert (tokens_dir / "bob.token").read_text() == "hk-b"
        assert {e["source"] for e in result.effective} == {"home"}

    def test_home_repo_merge_byte_for_byte_without_monitor(
        self, mock_repo: pathlib.Path, tmp_path: pathlib.Path, monkeypatch
    ) -> None:
        # Absent the claude-monitor file, the home+repo merge is unchanged:
        # repo still overrides home, repo-only adds, home-only inherits.
        master = tmp_path / "loom-accounts.env"
        master.write_text(
            _triple(1, "shared@example.com", "home-key", "shared.token")
            + _triple(2, "homeonly@example.com", "hk", "homeonly.token"),
            encoding="utf-8",
        )
        monkeypatch.setenv("LOOM_ACCOUNTS_ENV", str(master))
        _write_env(
            mock_repo,
            _triple(1, "shared@example.com", "repo-key", "shared.token")
            + _triple(2, "repoonly@example.com", "rk", "repoonly.token"),
        )
        result = bootstrap_tokens(mock_repo)
        assert result.monitor_env is None
        tokens_dir = mock_repo / ".loom" / "tokens"
        # Repo key wins for the shared email (repo-override).
        assert (tokens_dir / "shared.token").read_text() == "repo-key"
        assert (tokens_dir / "homeonly.token").read_text() == "hk"
        assert (tokens_dir / "repoonly.token").read_text() == "rk"
        sources = {e["email"]: e["source"] for e in result.effective}
        assert sources == {
            "shared@example.com": "repo-override",
            "homeonly@example.com": "home",
            "repoonly@example.com": "repo",
        }

    def test_loom_accounts_env_empty_still_disables_master(
        self, mock_repo: pathlib.Path, monkeypatch
    ) -> None:
        # LOOM_ACCOUNTS_ENV="" still disables the home master; the repo source
        # alone bootstraps (no claude-monitor file present).
        monkeypatch.setenv("LOOM_ACCOUNTS_ENV", "")
        _write_env(mock_repo, _triple(1, "a@example.com", "sk-1", "a.token"))
        result = bootstrap_tokens(mock_repo)
        assert result.home_env is None
        assert result.monitor_env is None
        assert result.written == ["a.token"]


class TestClaudeMonitorSourcing:
    def test_monitor_only_no_dead_end(
        self, mock_repo: pathlib.Path, tmp_path: pathlib.Path, monkeypatch
    ) -> None:
        # Only ~/.claude-monitor/accounts.env populated (EMAIL+KEY only), no
        # home master, no repo triples -> every account materialized via
        # auto-derive; no account-resolution dead-end.
        monkeypatch.setenv("LOOM_ACCOUNTS_ENV", "")  # disable home master
        mon = _write_monitor(
            tmp_path,
            monkeypatch,
            _pair(1, "alice@example.com", "mk-a")
            + _pair(2, "a.b.jones@example.org", "mk-j"),
        )
        result = bootstrap_tokens(mock_repo)
        assert result.monitor_env == mon
        assert result.home_env is None
        tokens_dir = mock_repo / ".loom" / "tokens"
        assert (tokens_dir / "alice-example.token").read_text() == "mk-a"
        assert (tokens_dir / "abjones-example.token").read_text() == "mk-j"
        assert {e["source"] for e in result.effective} == {"monitor"}

    @pytest.mark.skipif(sys.platform == "win32", reason="POSIX modes only")
    def test_monitor_token_files_mode_0600(
        self, mock_repo: pathlib.Path, tmp_path: pathlib.Path, monkeypatch
    ) -> None:
        monkeypatch.setenv("LOOM_ACCOUNTS_ENV", "")
        _write_monitor(tmp_path, monkeypatch, _pair(1, "alice@example.com", "mk"))
        bootstrap_tokens(mock_repo)
        token_path = mock_repo / ".loom" / "tokens" / "alice-example.token"
        assert os.stat(token_path).st_mode & 0o777 == 0o600

    def test_monitor_overrides_repo(
        self, mock_repo: pathlib.Path, tmp_path: pathlib.Path, monkeypatch
    ) -> None:
        # Same email in monitor and repo -> monitor key/file wins, provenance
        # monitor-override; monitor-only adds; repo-only retained.
        monkeypatch.setenv("LOOM_ACCOUNTS_ENV", "")
        _write_monitor(
            tmp_path,
            monkeypatch,
            _pair(1, "alice@example.com", "monitor-key")
            + _pair(2, "monly@example.com", "mk-only"),
        )
        _write_env(
            mock_repo,
            _triple(1, "alice@example.com", "repo-key", "alice-example.token")
            + _triple(2, "reponly@example.com", "rk", "reponly.token"),
        )
        result = bootstrap_tokens(mock_repo)
        tokens_dir = mock_repo / ".loom" / "tokens"
        # Monitor key wins on the shared email.
        assert (tokens_dir / "alice-example.token").read_text() == "monitor-key"
        assert (tokens_dir / "monly-example.token").read_text() == "mk-only"
        assert (tokens_dir / "reponly.token").read_text() == "rk"
        sources = {e["email"]: e["source"] for e in result.effective}
        assert sources == {
            "alice@example.com": "monitor-override",
            "monly@example.com": "monitor",
            "reponly@example.com": "repo",
        }

    def test_monitor_overrides_home(
        self, mock_repo: pathlib.Path, tmp_path: pathlib.Path, monkeypatch
    ) -> None:
        # Precedence is claude-monitor > repo > home. An email in monitor and
        # home resolves to the monitor key.
        master = tmp_path / "loom-accounts.env"
        master.write_text(
            _triple(1, "alice@example.com", "home-key", "alice-example.token"),
            encoding="utf-8",
        )
        monkeypatch.setenv("LOOM_ACCOUNTS_ENV", str(master))
        _write_monitor(
            tmp_path, monkeypatch, _pair(1, "alice@example.com", "monitor-key")
        )
        result = bootstrap_tokens(mock_repo)
        tokens_dir = mock_repo / ".loom" / "tokens"
        assert (tokens_dir / "alice-example.token").read_text() == "monitor-key"
        assert result.effective[0]["source"] == "monitor-override"

    def test_monitor_no_home_flag_keeps_monitor(
        self, mock_repo: pathlib.Path, tmp_path: pathlib.Path, monkeypatch
    ) -> None:
        # --no-home (home_env_path=None) disables the home master but the
        # claude-monitor source is independent and still consulted.
        master = tmp_path / "loom-accounts.env"
        master.write_text(
            _triple(1, "homeonly@example.com", "hk", "homeonly.token"),
            encoding="utf-8",
        )
        monkeypatch.setenv("LOOM_ACCOUNTS_ENV", str(master))
        _write_monitor(
            tmp_path, monkeypatch, _pair(1, "alice@example.com", "mk")
        )
        result = bootstrap_tokens(mock_repo, home_env_path=None)
        assert result.home_env is None
        assert result.monitor_env is not None
        assert result.written == ["alice-example.token"]

    def test_missing_monitor_dir_degrades_silently(
        self, mock_repo: pathlib.Path, monkeypatch
    ) -> None:
        # LOOM_CLAUDE_MONITOR_DIR points at a non-existent dir (conftest
        # default): no crash, no import — falls through to home+repo.
        _write_env(mock_repo, _triple(1, "a@example.com", "sk-1", "a.token"))
        result = bootstrap_tokens(mock_repo)
        assert result.monitor_env is None
        assert result.written == ["a.token"]

    def test_monitor_index_records_provenance_no_secret(
        self, mock_repo: pathlib.Path, tmp_path: pathlib.Path, monkeypatch
    ) -> None:
        monkeypatch.setenv("LOOM_ACCOUNTS_ENV", "")
        _write_monitor(
            tmp_path, monkeypatch, _pair(1, "alice@example.com", "mk-secret")
        )
        bootstrap_tokens(mock_repo)
        idx = json.loads(
            (mock_repo / ".loom" / "tokens" / "index.json").read_text()
        )
        entry = idx["accounts"][0]
        assert entry["source"] == "monitor"
        assert entry["email"] == "alice@example.com"
        # Secret hygiene: only a fingerprint, never the key material.
        assert "mk-secret" not in json.dumps(idx)
        assert "key" not in entry
        assert len(entry["key_fingerprint"]) == 8


class TestMonitorIdentityContinuity:
    """Monitor-derived stems must match existing index.json stems so the
    .ranking / .bad_tokens history keyed on the account name stays attached."""

    def test_derive_matches_established_stems(self) -> None:
        # The names the selector keys on are the derived stems; a monitor
        # account for the same email derives the same name.
        assert derive_token_filename("alice@example.com") == "alice-example.token"
        assert (
            derive_token_filename("a.b.jones@example.org") == "abjones-example.token"
        )
        assert derive_token_filename("agent-1@example.com") == "agent1-example.token"

    def test_monitor_stem_matches_prior_index_stem(
        self, mock_repo: pathlib.Path, tmp_path: pathlib.Path, monkeypatch
    ) -> None:
        # Bootstrap once from the repo (auto-derive), record the index stem,
        # then bootstrap again with the same email sourced from claude-monitor:
        # the stem (account name) is identical, so history stays attached.
        monkeypatch.setenv("LOOM_ACCOUNTS_ENV", "")
        _write_env(mock_repo, _pair(1, "alice@example.com", "repo-key"))
        bootstrap_tokens(mock_repo)
        idx_before = json.loads(
            (mock_repo / ".loom" / "tokens" / "index.json").read_text()
        )
        name_before = idx_before["accounts"][0]["name"]

        _write_monitor(
            tmp_path, monkeypatch, _pair(1, "alice@example.com", "monitor-key")
        )
        bootstrap_tokens(mock_repo)
        idx_after = json.loads(
            (mock_repo / ".loom" / "tokens" / "index.json").read_text()
        )
        name_after = idx_after["accounts"][0]["name"]
        assert name_after == name_before == "alice-example"

    def test_monitor_repo_overlap_collapses_not_duplicate_abort(
        self, mock_repo: pathlib.Path, tmp_path: pathlib.Path, monkeypatch
    ) -> None:
        # Same email in monitor and repo derives the same filename; the merge
        # collapses them to one account (override) — it must NOT trip the
        # duplicate-filename guard.
        monkeypatch.setenv("LOOM_ACCOUNTS_ENV", "")
        _write_monitor(
            tmp_path, monkeypatch, _pair(1, "alice@example.com", "monitor-key")
        )
        _write_env(mock_repo, _pair(1, "alice@example.com", "repo-key"))
        result = bootstrap_tokens(mock_repo)  # must not raise ValueError
        assert result.written == ["alice-example.token"]
        tokens_dir = mock_repo / ".loom" / "tokens"
        assert (tokens_dir / "alice-example.token").read_text() == "monitor-key"

    def test_true_cross_source_collision_still_aborts(
        self, mock_repo: pathlib.Path, tmp_path: pathlib.Path, monkeypatch
    ) -> None:
        # Distinct emails whose derived filenames collide is still a real
        # clobber and must abort (guard runs on the merged set).
        monkeypatch.setenv("LOOM_ACCOUNTS_ENV", "")
        _write_monitor(
            tmp_path, monkeypatch, _pair(1, "ajones@example.com", "mk")
        )
        _write_env(mock_repo, _pair(1, "a.jones@example.com", "rk"))
        with pytest.raises(ValueError, match="duplicate token filename"):
            bootstrap_tokens(mock_repo)


class TestCliMonitorReporting:
    def test_effective_report_shows_monitor_provenance(
        self,
        mock_repo: pathlib.Path,
        tmp_path: pathlib.Path,
        monkeypatch: pytest.MonkeyPatch,
        capsys: pytest.CaptureFixture[str],
    ) -> None:
        monkeypatch.setenv("LOOM_ACCOUNTS_ENV", "")
        _write_monitor(
            tmp_path, monkeypatch, _pair(1, "alice@example.com", "mk")
        )
        monkeypatch.chdir(mock_repo)
        rc = cli_main(["bootstrap", "--dry-run"])
        assert rc == 0
        out = capsys.readouterr().out
        assert "claude-monitor" in out
        assert "alice" in out
