"""Tests for loom_tools.tokens.select."""

from __future__ import annotations

import json
import os
import random
import subprocess
import sys
import time
from pathlib import Path

import pytest

from loom_tools.tokens.select import (
    EX_CONFIG,
    EmptyTokenPoolError,
    SelectedToken,
    select_token,
)


# ---------- helpers ----------


def _make_workspace(tmp_path: Path, accounts: dict[str, str]) -> Path:
    """Materialize a fake .loom/tokens/ with the given {name: key} accounts.

    Returns the workspace root.
    """
    tokens_dir = tmp_path / ".loom" / "tokens"
    tokens_dir.mkdir(parents=True)
    tokens_dir.chmod(0o700)
    for name, key in accounts.items():
        f = tokens_dir / f"{name}.token"
        f.write_text(key, encoding="utf-8")
        f.chmod(0o600)
    return tmp_path


def _write_ranking(workspace: Path, lines: list[str]) -> None:
    rfile = workspace / ".loom" / "tokens" / ".ranking"
    rfile.write_text("\n".join(lines) + "\n", encoding="utf-8")
    # Ensure mtime is fresh
    now = time.time()
    os.utime(rfile, (now, now))


def _write_allowlist(workspace: Path, names: list[str]) -> None:
    f = workspace / ".loom" / "tokens" / ".allowlist"
    f.write_text("\n".join(names) + "\n", encoding="utf-8")


# ---------- import-safety ----------


def test_module_import_has_no_side_effects(tmp_path):
    """Re-importing the module in a subprocess with no .loom/tokens must succeed."""
    code = (
        "import loom_tools.tokens.select as s; "
        "import loom_tools.tokens.bad_tokens as b; "
        "print('OK')"
    )
    # Locate the package source for this checkout (handles worktrees where
    # the system-installed loom_tools points at a different path that may
    # not yet have the tokens submodule).
    pkg_root = Path(__file__).resolve().parents[2] / "src"
    env = os.environ.copy()
    existing_pp = env.get("PYTHONPATH", "")
    env["PYTHONPATH"] = (
        f"{pkg_root}{os.pathsep}{existing_pp}" if existing_pp else str(pkg_root)
    )
    result = subprocess.run(
        [sys.executable, "-c", code],
        capture_output=True,
        text=True,
        cwd=str(tmp_path),
        check=False,
        env=env,
    )
    assert result.returncode == 0, result.stderr
    assert "OK" in result.stdout


# ---------- empty pool errors ----------


def test_missing_tokens_dir_raises(tmp_path):
    with pytest.raises(EmptyTokenPoolError, match="does not exist"):
        select_token(tmp_path)


def test_empty_tokens_dir_raises(tmp_path):
    (tmp_path / ".loom" / "tokens").mkdir(parents=True)
    with pytest.raises(EmptyTokenPoolError, match="No .token files"):
        select_token(tmp_path)


def test_all_tokens_bad_raises(tmp_path):
    workspace = _make_workspace(tmp_path, {"a": "key-a", "b": "key-b"})
    bad_file = workspace / ".loom" / "tokens" / ".bad_tokens"
    bad_file.write_text(
        "2026-01-01T00:00:00Z a expired\n2026-01-01T00:00:00Z b expired\n",
        encoding="utf-8",
    )
    with pytest.raises(EmptyTokenPoolError, match="marked bad"):
        select_token(workspace)


# ---------- random tier ----------


def test_random_pick_returns_valid_token(tmp_path):
    workspace = _make_workspace(tmp_path, {"alpha": "key-alpha", "beta": "key-beta"})
    sel = select_token(workspace, rng=random.Random(42))
    assert isinstance(sel, SelectedToken)
    assert sel.name in ("alpha", "beta")
    assert sel.key in ("key-alpha", "key-beta")
    assert sel.mode == "random"
    assert sel.file.is_file()


def test_random_skips_bad_token(tmp_path):
    workspace = _make_workspace(
        tmp_path, {"good": "key-good", "rotten": "key-rotten"},
    )
    bad_file = workspace / ".loom" / "tokens" / ".bad_tokens"
    bad_file.write_text("2026-01-01T00:00:00Z rotten expired\n", encoding="utf-8")
    sel = select_token(workspace, rng=random.Random(0))
    assert sel.name == "good"
    assert sel.key == "key-good"


def test_random_strips_whitespace(tmp_path):
    workspace = _make_workspace(tmp_path, {"trim": "  key-trimmed\n\n"})
    sel = select_token(workspace)
    assert sel.key == "key-trimmed"


# ---------- allowlist tier ----------


def test_allowlist_only_picks_allowed(tmp_path):
    workspace = _make_workspace(
        tmp_path, {"alpha": "key-a", "beta": "key-b", "gamma": "key-c"},
    )
    _write_allowlist(workspace, ["beta"])
    for _ in range(20):
        sel = select_token(workspace)
        assert sel.name == "beta"
        assert sel.mode == "allowlist"


def test_allowlist_skips_bad(tmp_path):
    workspace = _make_workspace(tmp_path, {"a": "ka", "b": "kb"})
    _write_allowlist(workspace, ["a", "b"])
    bad_file = workspace / ".loom" / "tokens" / ".bad_tokens"
    bad_file.write_text("2026-01-01T00:00:00Z a expired\n", encoding="utf-8")
    sel = select_token(workspace)
    assert sel.name == "b"
    assert sel.mode == "allowlist"


def test_allowlist_with_comments(tmp_path):
    workspace = _make_workspace(tmp_path, {"a": "ka", "b": "kb"})
    _write_allowlist(workspace, ["# header", "a", ""])
    sel = select_token(workspace)
    assert sel.name == "a"


def test_allowlist_with_no_existing_tokens_falls_through_to_random(tmp_path):
    workspace = _make_workspace(tmp_path, {"present": "kp"})
    _write_allowlist(workspace, ["missing-account"])
    sel = select_token(workspace)
    # Should fall through to random tier since allowlist has no eligible files
    assert sel.name == "present"
    assert sel.mode == "random"


# ---------- ranking tier ----------


def test_ranking_picks_among_top_n_eligible(tmp_path):
    # Default N=3 spreads across eligible ranked entries (a is exhausted, so
    # b and c are the top-2 eligible). See issue #3736.
    workspace = _make_workspace(tmp_path, {"a": "ka", "b": "kb", "c": "kc"})
    _write_ranking(workspace, ["a|exhausted", "b|", "c|"])
    sel = select_token(workspace, rng=random.Random(0))
    assert sel.name in ("b", "c")
    assert sel.name != "a"  # never the exhausted account
    assert sel.mode == "ranked"


def test_ranking_skips_blocked_status(tmp_path):
    workspace = _make_workspace(tmp_path, {"a": "ka", "b": "kb"})
    _write_ranking(workspace, ["a|blocked", "b|"])
    sel = select_token(workspace)
    assert sel.name == "b"


def test_ranking_skips_bad_token(tmp_path):
    workspace = _make_workspace(tmp_path, {"a": "ka", "b": "kb"})
    _write_ranking(workspace, ["a|", "b|"])
    bad_file = workspace / ".loom" / "tokens" / ".bad_tokens"
    bad_file.write_text("2026-01-01T00:00:00Z a expired\n", encoding="utf-8")
    sel = select_token(workspace)
    assert sel.name == "b"


def test_stale_ranking_falls_through_to_random(tmp_path):
    workspace = _make_workspace(tmp_path, {"a": "ka", "b": "kb"})
    rfile = workspace / ".loom" / "tokens" / ".ranking"
    rfile.write_text("a|\nb|\n", encoding="utf-8")
    # Backdate by 11 minutes — past the 10-min freshness window
    old = time.time() - (11 * 60)
    os.utime(rfile, (old, old))
    sel = select_token(workspace)
    # Tier 1 declined; tier 3 selected something
    assert sel.mode == "random"


def _write_stale_ranking(workspace: Path, lines: list[str], age_secs: float) -> None:
    """Write .ranking and backdate its mtime by *age_secs* seconds."""
    rfile = workspace / ".loom" / "tokens" / ".ranking"
    rfile.write_text("\n".join(lines) + "\n", encoding="utf-8")
    old = time.time() - age_secs
    os.utime(rfile, (old, old))


def test_stale_ranking_excludes_exhausted_from_random(tmp_path):
    """A stale .ranking still excludes exhausted accounts from the random tier.

    Regression for issue #3894: without this, an absent/stale ranking degraded
    to fully-random selection and repeatedly handed out the exhausted account,
    wedging sweeps at startup.
    """
    workspace = _make_workspace(tmp_path, {"tired": "kt", "fresh": "kf"})
    # 11 minutes old — past the freshness window — with `tired` exhausted.
    _write_stale_ranking(workspace, ["tired|exhausted", "fresh|"], 11 * 60)
    for seed in range(30):
        sel = select_token(workspace, rng=random.Random(seed))
        assert sel.name == "fresh"  # never the exhausted account
        assert sel.mode == "random"  # tier-1 declined (stale)


def test_stale_ranking_excludes_blocked_from_random(tmp_path):
    workspace = _make_workspace(tmp_path, {"blk": "kb", "ok": "ko"})
    _write_stale_ranking(workspace, ["blk|blocked", "ok|"], 20 * 60)
    for seed in range(20):
        sel = select_token(workspace, rng=random.Random(seed))
        assert sel.name == "ok"
        assert sel.mode == "random"


def test_stale_ranking_excludes_exhausted_from_allowlist(tmp_path):
    """Stale-ranking exclusions also apply to the allowlist tier."""
    workspace = _make_workspace(tmp_path, {"tired": "kt", "fresh": "kf"})
    _write_allowlist(workspace, ["tired", "fresh"])
    _write_stale_ranking(workspace, ["tired|exhausted", "fresh|"], 11 * 60)
    for seed in range(20):
        sel = select_token(workspace, rng=random.Random(seed))
        assert sel.name == "fresh"
        assert sel.mode == "allowlist"


def test_stale_ranking_all_exhausted_falls_back_rather_than_hard_fail(tmp_path):
    """A stale 'everything exhausted' ranking must not hard-fail a live pool.

    The advisory exclusions empty the pool, so selection retries ignoring them
    — returning a (possibly tired) token rather than raising EmptyTokenPoolError.
    """
    workspace = _make_workspace(tmp_path, {"a": "ka", "b": "kb"})
    _write_stale_ranking(workspace, ["a|exhausted", "b|exhausted"], 30 * 60)
    sel = select_token(workspace, rng=random.Random(0))
    assert sel.name in ("a", "b")
    assert sel.mode == "random"


def test_stale_ranking_without_status_still_random_over_all(tmp_path):
    """A stale ranking with no exhausted/blocked entries excludes nothing."""
    workspace = _make_workspace(tmp_path, {"a": "ka", "b": "kb"})
    _write_stale_ranking(workspace, ["a|", "b|"], 11 * 60)
    chosen = {
        select_token(workspace, rng=random.Random(seed)).name for seed in range(30)
    }
    # No exclusions => both accounts reachable via the random tier.
    assert chosen == {"a", "b"}


def test_ranking_with_comments_and_blank_lines(tmp_path):
    workspace = _make_workspace(tmp_path, {"a": "ka", "b": "kb"})
    _write_ranking(
        workspace,
        ["# header comment", "", "a|exhausted", "b|"],
    )
    sel = select_token(workspace)
    assert sel.name == "b"


def test_ranking_falls_through_when_all_exhausted(tmp_path):
    workspace = _make_workspace(tmp_path, {"a": "ka", "b": "kb"})
    _write_ranking(workspace, ["a|exhausted", "b|exhausted"])
    sel = select_token(workspace)
    # Falls through past tier 1 (all exhausted), past tier 2 (no allowlist),
    # to tier 3 (random).
    assert sel.mode == "random"


# ---------- ranking spread (issue #3736) ----------


def test_ranking_spreads_across_top_n_with_varying_seeds(tmp_path, monkeypatch):
    """Varying rng seeds must select different accounts among the top-N.

    This is the anti-collision property: concurrent spawners (each with an
    independent rng) should NOT deterministically land on .ranking[0].
    """
    monkeypatch.delenv("LOOM_TOKEN_SPREAD_TOP_N", raising=False)
    workspace = _make_workspace(
        tmp_path, {"a": "ka", "b": "kb", "c": "kc", "d": "kd", "e": "ke"},
    )
    # All available; ranking order a,b,c,d,e. Default N=3 => window {a,b,c}.
    _write_ranking(workspace, ["a|", "b|", "c|", "d|", "e|"])
    top_n = {"a", "b", "c"}
    chosen: set[str] = set()
    for seed in range(50):
        sel = select_token(workspace, rng=random.Random(seed))
        assert sel.mode == "ranked"
        assert sel.name in top_n  # never spills past the top-N window
        chosen.add(sel.name)
    # More than one distinct account is selected across seeds (spread), and
    # entries below the window (d, e) are never chosen.
    assert len(chosen) > 1
    assert chosen <= top_n


def test_ranking_n1_restores_greedy_first_eligible(tmp_path, monkeypatch):
    """N=1 (env override) exactly restores the historical greedy behavior."""
    monkeypatch.setenv("LOOM_TOKEN_SPREAD_TOP_N", "1")
    workspace = _make_workspace(tmp_path, {"a": "ka", "b": "kb", "c": "kc"})
    _write_ranking(workspace, ["a|exhausted", "b|", "c|"])
    # First eligible entry is b — deterministic regardless of seed.
    for seed in range(25):
        sel = select_token(workspace, rng=random.Random(seed))
        assert sel.name == "b"
        assert sel.mode == "ranked"


def test_ranking_spread_config_key(tmp_path, monkeypatch):
    """.loom/config.json -> tokens.spreadTopN is honored when env is unset."""
    monkeypatch.delenv("LOOM_TOKEN_SPREAD_TOP_N", raising=False)
    workspace = _make_workspace(tmp_path, {"a": "ka", "b": "kb", "c": "kc"})
    _write_ranking(workspace, ["a|", "b|", "c|"])
    config_path = workspace / ".loom" / "config.json"
    config_path.write_text(json.dumps({"tokens": {"spreadTopN": 1}}), encoding="utf-8")
    # spreadTopN=1 => greedy first eligible (a), deterministic across seeds.
    for seed in range(10):
        sel = select_token(workspace, rng=random.Random(seed))
        assert sel.name == "a"

    # env var overrides the config key.
    monkeypatch.setenv("LOOM_TOKEN_SPREAD_TOP_N", "3")
    chosen = {
        select_token(workspace, rng=random.Random(seed)).name for seed in range(50)
    }
    assert len(chosen) > 1


def test_ranking_spread_skips_interleaved_exhausted_blocked(tmp_path, monkeypatch):
    """Exhausted/blocked entries interleaved in the ranking are never chosen.

    The top-N window is filled from the *eligible* entries only, preserving
    ranking order and skipping exhausted/blocked/bad tokens.
    """
    monkeypatch.delenv("LOOM_TOKEN_SPREAD_TOP_N", raising=False)
    workspace = _make_workspace(
        tmp_path,
        {"a": "ka", "b": "kb", "c": "kc", "d": "kd", "e": "ke"},
    )
    # Interleaved: a exhausted, b ok, c blocked, d ok, e ok.
    # Eligible order: b, d, e. Default N=3 => window {b, d, e}.
    _write_ranking(workspace, ["a|exhausted", "b|", "c|blocked", "d|", "e|"])
    window = {"b", "d", "e"}
    chosen: set[str] = set()
    for seed in range(50):
        sel = select_token(workspace, rng=random.Random(seed))
        assert sel.mode == "ranked"
        assert sel.name in window
        assert sel.name not in ("a", "c")  # never exhausted/blocked
        chosen.add(sel.name)
    assert len(chosen) > 1


def test_ranking_spread_skips_bad_in_window(tmp_path, monkeypatch):
    """A bad token is excluded from the top-N window entirely."""
    monkeypatch.delenv("LOOM_TOKEN_SPREAD_TOP_N", raising=False)
    workspace = _make_workspace(tmp_path, {"a": "ka", "b": "kb", "c": "kc"})
    _write_ranking(workspace, ["a|", "b|", "c|"])
    bad_file = workspace / ".loom" / "tokens" / ".bad_tokens"
    bad_file.write_text("2026-01-01T00:00:00Z a expired\n", encoding="utf-8")
    for seed in range(25):
        sel = select_token(workspace, rng=random.Random(seed))
        assert sel.name in ("b", "c")
        assert sel.name != "a"


# ---------- CLI ----------


def test_cli_json_output(tmp_path):
    workspace = _make_workspace(tmp_path, {"only": "key-only"})
    pkg_root = Path(__file__).resolve().parents[2] / "src"
    env = os.environ.copy()
    existing_pp = env.get("PYTHONPATH", "")
    env["PYTHONPATH"] = (
        f"{pkg_root}{os.pathsep}{existing_pp}" if existing_pp else str(pkg_root)
    )
    result = subprocess.run(
        [
            sys.executable,
            "-m",
            "loom_tools.tokens.select",
            "--workspace",
            str(workspace),
            "--json",
        ],
        capture_output=True,
        text=True,
        check=True,
        env=env,
    )
    payload = json.loads(result.stdout)
    assert payload["name"] == "only"
    assert payload["key"] == "key-only"
    assert payload["mode"] == "random"
    assert payload["file"].endswith("only.token")


def _cli_env() -> dict[str, str]:
    pkg_root = Path(__file__).resolve().parents[2] / "src"
    env = os.environ.copy()
    existing_pp = env.get("PYTHONPATH", "")
    env["PYTHONPATH"] = (
        f"{pkg_root}{os.pathsep}{existing_pp}" if existing_pp else str(pkg_root)
    )
    return env


def test_cli_no_key_omits_secret(tmp_path):
    workspace = _make_workspace(tmp_path, {"only": "key-only"})
    result = subprocess.run(
        [
            sys.executable,
            "-m",
            "loom_tools.tokens.select",
            "--workspace",
            str(workspace),
            "--json",
            "--no-key",
        ],
        capture_output=True,
        text=True,
        check=True,
        env=_cli_env(),
    )
    payload = json.loads(result.stdout)
    assert "key" not in payload
    assert payload["name"] == "only"


def test_cli_empty_pool_exits_78(tmp_path):
    result = subprocess.run(
        [
            sys.executable,
            "-m",
            "loom_tools.tokens.select",
            "--workspace",
            str(tmp_path),
            "--json",
        ],
        capture_output=True,
        text=True,
        check=False,
        env=_cli_env(),
    )
    assert result.returncode == EX_CONFIG
    assert "loom-tokens bootstrap" in result.stderr
