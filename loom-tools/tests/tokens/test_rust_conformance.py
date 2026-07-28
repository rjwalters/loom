"""Conformance suite: Python `loom_tools.tokens` vs. the native
`loom-daemon tokens` Rust port (issue #4082, Phase 1 of epic #4081).

Drives fixture pools through both implementations and diffs the resulting
state files / CLI output byte-for-byte on scenarios where behavior is fully
deterministic (single eligible candidate, or CLI subcommands that never
consult randomness). This intentionally does **not** try to reproduce the
same shuffle order as Python's `random.Random` for a given seed — see
`loom-daemon/src/tokens_pool/rng.rs` module docs for why that isn't a goal.

Every test is skipped (not failed) when the compiled `loom-daemon` binary
cannot be located, so this file is a no-op on a machine that has only run
`pip install -e loom-tools` without ever building the Rust workspace. Point
`LOOM_DAEMON_BIN` at an explicit binary to override discovery; otherwise this
searches `target/release/loom-daemon` then `target/debug/loom-daemon`
relative to the repo root inferred from this file's own location (so it
works from a plain checkout, not just a Loom-managed worktree).

Wiring this into an actual CI job (which needs a Rust build step ahead of
the Python test job) is deferred to a follow-up issue — see the PR
description for issue #4082.
"""

from __future__ import annotations

import json
import os
import subprocess
from pathlib import Path

import pytest

from loom_tools.tokens import allowlist as py_allowlist
from loom_tools.tokens.bad_tokens import mark_bad as py_mark_bad
from loom_tools.tokens.select import select_token as py_select_token


def _find_repo_root() -> Path | None:
    """Walk up from this file looking for the Cargo workspace root."""
    current = Path(__file__).resolve()
    for parent in current.parents:
        if (parent / "Cargo.toml").is_file() and (parent / "loom-daemon").is_dir():
            return parent
    return None


def _find_daemon_binary() -> Path | None:
    env = os.environ.get("LOOM_DAEMON_BIN")
    if env:
        p = Path(env)
        return p if p.is_file() else None
    root = _find_repo_root()
    if root is None:
        return None
    for profile in ("release", "debug"):
        candidate = root / "target" / profile / "loom-daemon"
        if candidate.is_file():
            return candidate
    return None


DAEMON_BIN = _find_daemon_binary()

pytestmark = pytest.mark.skipif(
    DAEMON_BIN is None,
    reason=(
        "loom-daemon binary not found — build it first "
        "(`cd loom-daemon && cargo build`) or set $LOOM_DAEMON_BIN"
    ),
)


def _run_rust(*args: str) -> subprocess.CompletedProcess[str]:
    assert DAEMON_BIN is not None
    return subprocess.run(
        [str(DAEMON_BIN), "tokens", *args],
        capture_output=True,
        text=True,
        check=False,
    )


def _write_pool(workspace: Path, names: list[str]) -> Path:
    """Seed `<workspace>/.loom/tokens/*.token` and return `workspace` itself
    (not the tokens dir) — every caller treats the return value as the
    workspace root passed to `--workspace` / `select_token()`."""
    d = workspace / ".loom" / "tokens"
    d.mkdir(parents=True, exist_ok=True)
    for n in names:
        (d / f"{n}.token").write_text(f"sk-ant-oat01-fake-{n}", encoding="utf-8")
    return workspace


# ---------------------------------------------------------------------------
# select: single-candidate scenarios are the only ones where a random tier's
# *outcome* is directly comparable across two independent RNGs.
# ---------------------------------------------------------------------------


def test_select_single_token_random_tier_agrees(tmp_path):
    workspace = _write_pool(tmp_path, ["only"])

    py_sel = py_select_token(workspace)
    result = _run_rust("select", "--workspace", str(workspace), "--no-key")
    assert result.returncode == 0, result.stderr
    rust_sel = json.loads(result.stdout)

    assert rust_sel["name"] == py_sel.name == "only"
    assert rust_sel["mode"] == py_sel.mode == "random"


def test_select_ranked_tier_single_healthy_candidate_agrees(tmp_path):
    workspace = _write_pool(tmp_path, ["a", "b"])
    (workspace / ".loom" / "tokens" / ".ranking").write_text(
        "a|available\nb|exhausted\n", encoding="utf-8"
    )

    py_sel = py_select_token(workspace)
    result = _run_rust("select", "--workspace", str(workspace), "--no-key")
    assert result.returncode == 0, result.stderr
    rust_sel = json.loads(result.stdout)

    assert rust_sel["name"] == py_sel.name == "a"
    assert rust_sel["mode"] == py_sel.mode == "ranked"


def test_select_ranked_tier_load_gate_agrees(tmp_path):
    """Both selectors exclude a 5h-loaded account in the preferred pass (#4195).

    A mixed-load 3-field ranking: `a` is healthy but 90% 5h-loaded (>= the 0.70
    default gate), `b` is healthy and lightly loaded. The preferred pass leaves
    exactly one eligible candidate (`b`), so the outcome is deterministic and
    directly comparable across the two independent RNGs.
    """
    workspace = _write_pool(tmp_path, ["a", "b"])
    (workspace / ".loom" / "tokens" / ".ranking").write_text(
        "a|available|0.90\nb|available|0.10\n", encoding="utf-8"
    )

    py_sel = py_select_token(workspace)
    result = _run_rust("select", "--workspace", str(workspace), "--no-key")
    assert result.returncode == 0, result.stderr
    rust_sel = json.loads(result.stdout)

    assert rust_sel["name"] == py_sel.name == "b"
    assert rust_sel["mode"] == py_sel.mode == "ranked"


def test_select_ranked_tier_load_gate_fallback_agrees(tmp_path):
    """When every account is over the gate, both readmit in the fallback (#4195).

    Both accounts are 5h-loaded, so the fallback pass (gate dropped) is what
    selects — the pool never hard-fails on load alone. Cap the rotation window
    to N=1 (via the shared env both processes read) so both pick the first
    eligible fallback candidate (`a`) deterministically.
    """
    workspace = _write_pool(tmp_path, ["a", "b"])
    (workspace / ".loom" / "tokens" / ".ranking").write_text(
        "a|available|0.95\nb|available|0.90\n", encoding="utf-8"
    )
    prev = os.environ.get("LOOM_TOKEN_SPREAD_TOP_N")
    os.environ["LOOM_TOKEN_SPREAD_TOP_N"] = "1"
    try:
        py_sel = py_select_token(workspace)
        result = _run_rust("select", "--workspace", str(workspace), "--no-key")
    finally:
        if prev is None:
            os.environ.pop("LOOM_TOKEN_SPREAD_TOP_N", None)
        else:
            os.environ["LOOM_TOKEN_SPREAD_TOP_N"] = prev
    assert result.returncode == 0, result.stderr
    rust_sel = json.loads(result.stdout)

    assert rust_sel["name"] == py_sel.name == "a"
    assert rust_sel["mode"] == py_sel.mode == "ranked"


def test_select_ranked_tier_unknown_util_not_gated_agrees(tmp_path):
    """A legacy 2-field (unknown-util) line stays eligible in both (#4195).

    `a` has no measured 5h utilization (2-field legacy line) -> unknown ->
    never gated; `b` is 5h-loaded and excluded. The preferred pass leaves `a`
    as the sole candidate, deterministic across both RNGs.
    """
    workspace = _write_pool(tmp_path, ["a", "b"])
    (workspace / ".loom" / "tokens" / ".ranking").write_text(
        "a|available\nb|available|0.99\n", encoding="utf-8"
    )

    py_sel = py_select_token(workspace)
    result = _run_rust("select", "--workspace", str(workspace), "--no-key")
    assert result.returncode == 0, result.stderr
    rust_sel = json.loads(result.stdout)

    assert rust_sel["name"] == py_sel.name == "a"
    assert rust_sel["mode"] == py_sel.mode == "ranked"


def test_select_bad_token_skipped_by_both(tmp_path):
    workspace = _write_pool(tmp_path, ["a", "b"])
    py_mark_bad(workspace, "a", "exhausted: 429")

    py_sel = py_select_token(workspace)
    result = _run_rust("select", "--workspace", str(workspace), "--no-key")
    assert result.returncode == 0, result.stderr
    rust_sel = json.loads(result.stdout)

    # Both must skip the bad-marked "a" and land on the only remaining "b".
    assert py_sel.name == "b"
    assert rust_sel["name"] == "b"


def test_select_empty_pool_both_exit_ex_config(tmp_path):
    workspace = tmp_path / "no-pool"
    workspace.mkdir()

    with pytest.raises(Exception):
        py_select_token(workspace)

    env = dict(os.environ)
    env["LOOM_SHARED_TOKENS_DIR"] = ""  # disable the shared-pool fallback
    result = subprocess.run(
        [str(DAEMON_BIN), "tokens", "select", "--workspace", str(workspace), "--no-key"],
        capture_output=True,
        text=True,
        check=False,
        env=env,
    )
    assert result.returncode == 78  # EX_CONFIG, both implementations agree


# ---------------------------------------------------------------------------
# allowlist (`.allowlist`): fully deterministic, byte-for-byte comparable.
# ---------------------------------------------------------------------------


def test_allowlist_set_file_byte_identical(tmp_path):
    py_ws = tmp_path / "py"
    rust_ws = tmp_path / "rust"
    _write_pool(py_ws, ["a", "b", "c"])
    _write_pool(rust_ws, ["a", "b", "c"])

    py_allowlist.write_allowlist(py_ws, ["b", "a"])
    result = _run_rust("pin", "set", "b", "a", "--workspace", str(rust_ws))
    assert result.returncode == 0, result.stderr

    py_content = (py_ws / ".loom" / "tokens" / ".allowlist").read_text(encoding="utf-8")
    rust_content = (rust_ws / ".loom" / "tokens" / ".allowlist").read_text(encoding="utf-8")
    assert py_content == rust_content == "b\na\n"


def test_allowlist_status_json_agrees(tmp_path):
    py_ws = tmp_path / "py"
    rust_ws = tmp_path / "rust"
    _write_pool(py_ws, ["a", "b"])
    _write_pool(rust_ws, ["a", "b"])
    py_allowlist.write_allowlist(py_ws, ["a"])
    _run_rust("pin", "set", "a", "--workspace", str(rust_ws))

    py_status = {
        "allowlist_active": bool(py_allowlist.read_allowlist(py_ws)),
        "allowlist": py_allowlist.read_allowlist(py_ws),
        "accounts": py_allowlist.list_accounts(py_ws),
    }
    result = _run_rust("pin", "status", "--workspace", str(rust_ws), "--json")
    assert result.returncode == 0, result.stderr
    rust_status = json.loads(result.stdout)

    assert rust_status == py_status


def test_unpin_clears_identically(tmp_path):
    py_ws = tmp_path / "py"
    rust_ws = tmp_path / "rust"
    _write_pool(py_ws, ["a"])
    _write_pool(rust_ws, ["a"])
    py_allowlist.write_allowlist(py_ws, ["a"])
    _run_rust("pin", "set", "a", "--workspace", str(rust_ws))

    py_cleared = py_allowlist.clear_allowlist(py_ws)
    result = _run_rust("unpin", "--workspace", str(rust_ws), "--json")
    assert result.returncode == 0, result.stderr
    rust_cleared = json.loads(result.stdout)["cleared"]

    assert py_cleared == rust_cleared is True
    assert not (py_ws / ".loom" / "tokens" / ".allowlist").exists()
    assert not (rust_ws / ".loom" / "tokens" / ".allowlist").exists()


# ---------------------------------------------------------------------------
# unblock: auth-reason regex + word-boundary bad_tokens rewrite.
# ---------------------------------------------------------------------------


def test_unblock_auth_reason_removed_identically(tmp_path):
    py_ws = tmp_path / "py"
    rust_ws = tmp_path / "rust"
    _write_pool(py_ws, ["a", "b"])
    _write_pool(rust_ws, ["a", "b"])
    py_mark_bad(py_ws, "a", "401 unauthorized")
    py_mark_bad(py_ws, "b", "exhausted: 429")
    py_mark_bad(rust_ws, "a", "401 unauthorized")
    py_mark_bad(rust_ws, "b", "exhausted: 429")

    # Python's unblock only exists behind the CLI (cli.py::_cmd_unblock), not
    # a standalone importable function — invoke it the same way an operator
    # would, via `python -m loom_tools.tokens.cli`.
    py_result = subprocess.run(
        [
            "python3",
            "-m",
            "loom_tools.tokens.cli",
            "unblock",
            "a",
            "--json",
        ],
        capture_output=True,
        text=True,
        check=False,
        cwd=py_ws,
        env={**os.environ, "LOOM_WORKSPACE": str(py_ws), "PYTHONPATH": _loom_tools_src()},
    )
    assert py_result.returncode == 0, py_result.stderr
    py_json = json.loads(py_result.stdout)

    rust_result = _run_rust("unblock", "a", "--workspace", str(rust_ws), "--json")
    assert rust_result.returncode == 0, rust_result.stderr
    rust_json = json.loads(rust_result.stdout)

    # "a" (auth) is removed; "b" (exhausted) is not a target here, so nothing
    # is excluded and both exit 0 (#4212 adds the `excluded` field).
    assert py_json == rust_json == {"removed": 1, "kept": 1, "excluded": []}

    py_bad = (py_ws / ".loom" / "tokens" / ".bad_tokens").read_text(encoding="utf-8")
    rust_bad = (rust_ws / ".loom" / "tokens" / ".bad_tokens").read_text(encoding="utf-8")
    # Both keep exactly the "b" entry; timestamps differ per-run but the
    # account name + reason text must match.
    assert "a" not in py_bad.split()
    assert "a" not in rust_bad.split()
    assert "b" in py_bad
    assert "b" in rust_bad


def test_unblock_default_scope_excluded_exit3_identically(tmp_path):
    """#4212: a named account whose only entry is non-auth ("exhausted") is
    left in place under the default scope — both implementations name it in
    `excluded` and exit 3 (not a silent success)."""
    py_ws = tmp_path / "py"
    rust_ws = tmp_path / "rust"
    _write_pool(py_ws, ["a", "b"])
    _write_pool(rust_ws, ["a", "b"])
    py_mark_bad(py_ws, "a", "401 unauthorized")
    py_mark_bad(py_ws, "b", "exhausted: weekly-limit")
    py_mark_bad(rust_ws, "a", "401 unauthorized")
    py_mark_bad(rust_ws, "b", "exhausted: weekly-limit")

    py_result = subprocess.run(
        [
            "python3",
            "-m",
            "loom_tools.tokens.cli",
            "unblock",
            "a",
            "b",
            "--json",
        ],
        capture_output=True,
        text=True,
        check=False,
        cwd=py_ws,
        env={**os.environ, "LOOM_WORKSPACE": str(py_ws), "PYTHONPATH": _loom_tools_src()},
    )
    rust_result = _run_rust("unblock", "a", "b", "--workspace", str(rust_ws), "--json")

    # Both exit 3 (default scope left a named account still blocked).
    assert py_result.returncode == 3, py_result.stderr
    assert rust_result.returncode == 3, rust_result.stderr

    py_json = json.loads(py_result.stdout)
    rust_json = json.loads(rust_result.stdout)
    # "a"'s auth entry is dropped; "b"'s exhausted entry stays and is named.
    assert py_json == rust_json == {"removed": 1, "kept": 1, "excluded": ["b"]}


def _loom_tools_src() -> str:
    root = _find_repo_root()
    assert root is not None
    return str(root / "loom-tools" / "src")


# ---------------------------------------------------------------------------
# check --source monitor: the claude-monitor ranking consumer (#4094).
#
# The live-probe path is not cross-checked here because making the hardcoded
# Anthropic endpoint injectable would require modifying the (read-only this
# phase) Python `check.py`. Instead we exercise the fully deterministic,
# network-free `--source monitor` path, which drives the SAME `.ranking`
# writer format the probe path uses — so byte-identity of the monitor-sourced
# `.ranking` transitively validates the shared writer. The live-probe status
# branches are covered by Rust unit tests in `tokens_pool::check` (stub
# transport) mirroring `loom-tools/tests/tokens/test_check.py`.
# ---------------------------------------------------------------------------

import datetime  # noqa: E402


def _fresh_generated_at() -> str:
    return datetime.datetime.now(datetime.timezone.utc).strftime(
        "%Y-%m-%dT%H:%M:%SZ"
    )


def _seed_monitor_pool(workspace: Path, index_accounts: list[dict]) -> Path:
    """Seed `<workspace>/.loom/tokens/` with `*.token` files and an index.json
    manifest (name+email per account). Returns the workspace."""
    d = workspace / ".loom" / "tokens"
    d.mkdir(parents=True, exist_ok=True)
    for acct in index_accounts:
        (d / f"{acct['name']}.token").write_text(
            f"sk-ant-oat01-fake-{acct['name']}", encoding="utf-8"
        )
    (d / "index.json").write_text(
        json.dumps({"version": 2, "accounts": index_accounts}), encoding="utf-8"
    )
    return workspace


def _write_ranking_json(monitor_dir: Path, generated_at: str, accounts: list[dict]):
    monitor_dir.mkdir(parents=True, exist_ok=True)
    (monitor_dir / "ranking.json").write_text(
        json.dumps(
            {"schema": 1, "generated_at": generated_at, "accounts": accounts}
        ),
        encoding="utf-8",
    )


def _run_python_check(tokens_dir: Path, monitor_dir: Path, *extra: str):
    return subprocess.run(
        ["python3", "-m", "loom_tools.tokens.cli", "check", *extra],
        capture_output=True,
        text=True,
        check=False,
        env={
            **os.environ,
            "PYTHONPATH": _loom_tools_src(),
            "LOOM_TOKENS_DIR": str(tokens_dir),
            "LOOM_CLAUDE_MONITOR_DIR": str(monitor_dir),
            "LOOM_SHARED_TOKENS_DIR": "",
        },
    )


def _run_rust_check(workspace: Path, monitor_dir: Path, *extra: str):
    assert DAEMON_BIN is not None
    return subprocess.run(
        [str(DAEMON_BIN), "tokens", "check", "--workspace", str(workspace), *extra],
        capture_output=True,
        text=True,
        check=False,
        env={
            **os.environ,
            "LOOM_CLAUDE_MONITOR_DIR": str(monitor_dir),
            "LOOM_SHARED_TOKENS_DIR": "",
        },
    )


def test_check_monitor_source_ranking_byte_identical(tmp_path):
    index_accounts = [
        {"name": "acct-a", "email": "a@example.com"},
        {"name": "acct-b", "email": "b@example.com"},
        {"name": "acct-c", "email": "c@example.com"},
    ]
    py_ws = _seed_monitor_pool(tmp_path / "py", index_accounts)
    rust_ws = _seed_monitor_pool(tmp_path / "rust", index_accounts)
    monitor_dir = tmp_path / "monitor"
    _write_ranking_json(
        monitor_dir,
        _fresh_generated_at(),
        [
            {"email": "b@example.com", "status": "available",
             "utilization": {"7d": 0.80, "5h": 0.10}},
            {"email": "a@example.com", "status": "available",
             "utilization": {"7d": 0.20, "5h": 0.10}},
            {"email": "c@example.com", "status": "rate_limited",
             "utilization": {"7d": 0.10, "5h": 0.90}},
        ],
    )

    py = _run_python_check(
        py_ws / ".loom" / "tokens", monitor_dir, "--source", "monitor", "--ranking"
    )
    assert py.returncode == 0, py.stderr
    rust = _run_rust_check(rust_ws, monitor_dir, "--source", "monitor", "--ranking")
    assert rust.returncode == 0, rust.stderr

    py_ranking = (py_ws / ".loom" / "tokens" / ".ranking").read_text(encoding="utf-8")
    rust_ranking = (rust_ws / ".loom" / "tokens" / ".ranking").read_text(
        encoding="utf-8"
    )
    # Ordered by (status_rank, util_7d, util_5h): a (0.20) < b (0.80) < c (rl).
    # The 5h utilization is emitted as the optional third field (issue #4195),
    # byte-identical across both writers.
    assert (
        py_ranking
        == rust_ranking
        == "acct-a|available|0.10\nacct-b|available|0.10\nacct-c|rate_limited|0.90\n"
    )

    # The Python selector consumes the Rust-written .ranking: it picks a
    # healthy (available) account, never the rate_limited one, via the ranked
    # tier. (Which of the two available accounts wins is a random/spread tier
    # decision, so assert membership, not a single name.)
    sel = py_select_token(rust_ws)
    assert sel.name in {"acct-a", "acct-b"}
    assert sel.mode == "ranked"


def test_check_monitor_source_manifest_only_account_has_empty_status(tmp_path):
    index_accounts = [
        {"name": "acct-a", "email": "a@example.com"},
        {"name": "acct-z", "email": "z@example.com"},
    ]
    py_ws = _seed_monitor_pool(tmp_path / "py", index_accounts)
    rust_ws = _seed_monitor_pool(tmp_path / "rust", index_accounts)
    monitor_dir = tmp_path / "monitor"
    # Only acct-a is mentioned by the monitor; acct-z is manifest-only.
    _write_ranking_json(
        monitor_dir,
        _fresh_generated_at(),
        [{"email": "a@example.com", "status": "available",
          "utilization": {"7d": 0.50}}],
    )

    py = _run_python_check(
        py_ws / ".loom" / "tokens", monitor_dir, "--source", "monitor", "--ranking"
    )
    assert py.returncode == 0, py.stderr
    rust = _run_rust_check(rust_ws, monitor_dir, "--source", "monitor", "--ranking")
    assert rust.returncode == 0, rust.stderr

    py_ranking = (py_ws / ".loom" / "tokens" / ".ranking").read_text(encoding="utf-8")
    rust_ranking = (rust_ws / ".loom" / "tokens" / ".ranking").read_text(
        encoding="utf-8"
    )
    # acct-z keeps its RAW empty status in the file (sorts last, rank 99).
    assert py_ranking == rust_ranking == "acct-a|available\nacct-z|\n"


# ---------------------------------------------------------------------------
# bootstrap: multi-source `.env` merge + `.token`/`index.json` provisioning
# (#4105). The Python CLI resolves its repo root via `find_repo_root()` (needs
# a `.git` + `.loom/` marker and is run with `cwd=workspace`); the Rust CLI
# takes an explicit `--workspace`. Both are isolated from the real host
# environment (no home master, no claude-monitor, no shared pool) so only the
# repo-local `.env` feeds the merge.
# ---------------------------------------------------------------------------


def _seed_bootstrap_repo(workspace: Path, env_body: str) -> Path:
    """Create a `.git` + `.loom` repo skeleton with a repo-local `.env`."""
    (workspace / ".loom").mkdir(parents=True, exist_ok=True)
    (workspace / ".git").mkdir(parents=True, exist_ok=True)
    (workspace / ".env").write_text(env_body, encoding="utf-8")
    return workspace


def _bootstrap_env(monitor_dir: Path) -> dict:
    return {
        **os.environ,
        "PYTHONPATH": _loom_tools_src(),
        "LOOM_ACCOUNTS_ENV": "",  # disable the home master
        "LOOM_CLAUDE_MONITOR_DIR": str(monitor_dir),  # empty -> no monitor source
        "LOOM_SHARED_TOKENS_DIR": "",  # disable the shared pool fallback
    }


def _run_python_bootstrap(workspace: Path, monitor_dir: Path, *extra: str):
    return subprocess.run(
        ["python3", "-m", "loom_tools.tokens.cli", "bootstrap", *extra],
        capture_output=True,
        text=True,
        check=False,
        cwd=workspace,
        env=_bootstrap_env(monitor_dir),
    )


def _run_rust_bootstrap(workspace: Path, monitor_dir: Path, *extra: str):
    assert DAEMON_BIN is not None
    return subprocess.run(
        [
            str(DAEMON_BIN),
            "tokens",
            "bootstrap",
            "--workspace",
            str(workspace),
            *extra,
        ],
        capture_output=True,
        text=True,
        check=False,
        env=_bootstrap_env(monitor_dir),
    )


_BOOTSTRAP_ENV_BODY = (
    "ACCOUNT_EMAIL_1=alice@example.com\n"
    "ACCOUNT_KEY_1=sk-ant-oat01-alice\n"
    "ACCOUNT_EMAIL_2=bob.jones@example.org\n"
    "ACCOUNT_KEY_2=sk-ant-oat01-bob\n"
    # A partial triple: email with no key. Both must warn and skip, not abort.
    "ACCOUNT_EMAIL_3=orphan@example.com\n"
)


def test_bootstrap_dry_run_effective_set_agrees(tmp_path):
    monitor_dir = tmp_path / "monitor-empty"  # never created -> no monitor src
    py_ws = _seed_bootstrap_repo(tmp_path / "py", _BOOTSTRAP_ENV_BODY)
    rust_ws = _seed_bootstrap_repo(tmp_path / "rust", _BOOTSTRAP_ENV_BODY)

    py = _run_python_bootstrap(py_ws, monitor_dir, "--dry-run", "--json")
    assert py.returncode == 0, py.stderr
    rust = _run_rust_bootstrap(rust_ws, monitor_dir, "--dry-run", "--json")
    assert rust.returncode == 0, rust.stderr

    py_effective = json.loads(py.stdout)["effective"]
    rust_effective = json.loads(rust.stdout)["effective"]

    # The auto-derived filenames + merge provenance must match byte-for-byte.
    assert rust_effective == py_effective
    # The orphan (partial) triple is dropped by both; only the two full ones
    # survive with filenames derived from the email local-part + domain label.
    files = {a["file"] for a in rust_effective}
    assert files == {"alice-example.token", "bobjones-example.token"}
    # Dry-run writes nothing.
    assert not (py_ws / ".loom" / "tokens" / "index.json").exists()
    assert not (rust_ws / ".loom" / "tokens" / "index.json").exists()


def _index_accounts(pool: Path) -> list:
    """Load `index.json`'s accounts array (drops the per-run `generated_at`)."""
    manifest = json.loads((pool / "index.json").read_text(encoding="utf-8"))
    assert manifest["version"] == 2
    return manifest["accounts"]


def test_bootstrap_index_json_and_tokens_byte_compatible(tmp_path):
    monitor_dir = tmp_path / "monitor-empty"
    py_ws = _seed_bootstrap_repo(tmp_path / "py", _BOOTSTRAP_ENV_BODY)
    rust_ws = _seed_bootstrap_repo(tmp_path / "rust", _BOOTSTRAP_ENV_BODY)

    py = _run_python_bootstrap(py_ws, monitor_dir)
    assert py.returncode == 0, py.stderr
    rust = _run_rust_bootstrap(rust_ws, monitor_dir)
    assert rust.returncode == 0, rust.stderr

    py_pool = py_ws / ".loom" / "tokens"
    rust_pool = rust_ws / ".loom" / "tokens"

    # index.json manifest rows are byte-compatible (only generated_at differs).
    assert _index_accounts(rust_pool) == _index_accounts(py_pool)
    # And the manifest carries only 8-char fingerprints + source, no secrets.
    for row in _index_accounts(rust_pool):
        assert len(row["key_fingerprint"]) == 8
        assert "source" in row
        assert "key" not in row
    raw = (rust_pool / "index.json").read_text(encoding="utf-8")
    assert "sk-ant-oat01" not in raw

    # The materialized token files are identical across both writers.
    for name in ("alice-example.token", "bobjones-example.token"):
        assert (rust_pool / name).read_text(encoding="utf-8") == (
            py_pool / name
        ).read_text(encoding="utf-8")


def test_bootstrap_no_source_both_fail(tmp_path):
    monitor_dir = tmp_path / "monitor-empty"
    # A repo skeleton with NO account source at all.
    py_ws = tmp_path / "py"
    rust_ws = tmp_path / "rust"
    for ws in (py_ws, rust_ws):
        (ws / ".loom").mkdir(parents=True)
        (ws / ".git").mkdir(parents=True)

    py = _run_python_bootstrap(py_ws, monitor_dir, "--json")
    rust = _run_rust_bootstrap(rust_ws, monitor_dir, "--json")
    # Both exit non-zero when no account source exists.
    assert py.returncode != 0
    assert rust.returncode != 0


# ---------------------------------------------------------------------------
# import-from-monitor: claude-monitor live SQLite (usage.db) import (#4106).
# A fixture usage.db is driven through both CLIs (via --db) and the resulting
# materialized pool + index.json must match byte-for-byte. Both are isolated
# from the real host (no shared pool) and target their own repo-local pool.
# ---------------------------------------------------------------------------

import sqlite3  # noqa: E402


def _seed_usage_db(db_path: Path, creds: list[dict]) -> Path:
    """Create a fixture claude-monitor usage.db with the accounts +
    oauth_credentials schema the importer queries. Each cred dict carries
    `label`, `access_token`, `email` (None -> no accounts row) and `is_active`."""
    db_path.parent.mkdir(parents=True, exist_ok=True)
    conn = sqlite3.connect(db_path)
    try:
        conn.executescript(
            """
            CREATE TABLE accounts (id INTEGER PRIMARY KEY, email TEXT);
            CREATE TABLE oauth_credentials (
                id INTEGER PRIMARY KEY,
                label TEXT,
                access_token TEXT,
                account_id INTEGER,
                is_active INTEGER
            );
            """
        )
        next_account_id = 1
        for c in creds:
            account_id = None
            if c.get("email") is not None:
                account_id = next_account_id
                next_account_id += 1
                conn.execute(
                    "INSERT INTO accounts (id, email) VALUES (?, ?)",
                    (account_id, c["email"]),
                )
            conn.execute(
                "INSERT INTO oauth_credentials "
                "(label, access_token, account_id, is_active) VALUES (?, ?, ?, ?)",
                (c["label"], c["access_token"], account_id, c["is_active"]),
            )
        conn.commit()
    finally:
        conn.close()
    return db_path


def _seed_import_repo(workspace: Path) -> Path:
    """A `.git` + `.loom` repo skeleton so the Python CLI's find_repo_root()
    resolves the repo-local pool at `<workspace>/.loom/tokens`."""
    (workspace / ".loom").mkdir(parents=True, exist_ok=True)
    (workspace / ".git").mkdir(parents=True, exist_ok=True)
    return workspace


def _import_env() -> dict:
    return {
        **os.environ,
        "PYTHONPATH": _loom_tools_src(),
        "LOOM_SHARED_TOKENS_DIR": "",  # disable the shared pool fallback
    }


def _run_python_import(workspace: Path, db: Path, *extra: str):
    return subprocess.run(
        [
            "python3",
            "-m",
            "loom_tools.tokens.cli",
            "import-from-monitor",
            "--db",
            str(db),
            *extra,
        ],
        capture_output=True,
        text=True,
        check=False,
        cwd=workspace,
        env=_import_env(),
    )


def _run_rust_import(workspace: Path, db: Path, *extra: str):
    assert DAEMON_BIN is not None
    return subprocess.run(
        [
            str(DAEMON_BIN),
            "tokens",
            "import-from-monitor",
            "--workspace",
            str(workspace),
            "--db",
            str(db),
            *extra,
        ],
        capture_output=True,
        text=True,
        check=False,
        env=_import_env(),
    )


_IMPORT_CREDS = [
    {"label": "alice@example.com", "access_token": "sk-live-alice",
     "email": "alice@example.com", "is_active": 1},
    # Org-label form, joined account row absent -> email recovered from label.
    {"label": "bob@example.com's Organization", "access_token": "sk-live-bob",
     "email": None, "is_active": 1},
    # Inactive -> filtered by both.
    {"label": "carol@example.com", "access_token": "sk-live-carol",
     "email": "carol@example.com", "is_active": 0},
]


def test_import_from_monitor_dry_run_effective_agrees(tmp_path):
    db = _seed_usage_db(tmp_path / "monitor" / "usage.db", _IMPORT_CREDS)
    py_ws = _seed_import_repo(tmp_path / "py")
    rust_ws = _seed_import_repo(tmp_path / "rust")

    py = _run_python_import(py_ws, db, "--dry-run", "--json")
    assert py.returncode == 0, py.stderr
    rust = _run_rust_import(rust_ws, db, "--dry-run", "--json")
    assert rust.returncode == 0, rust.stderr

    py_effective = json.loads(py.stdout)["effective"]
    rust_effective = json.loads(rust.stdout)["effective"]

    assert rust_effective == py_effective
    # The two active accounts survive; the inactive carol is filtered out.
    files = {a["file"] for a in rust_effective}
    assert files == {"alice-example.token", "bob-example.token"}
    assert all(a["source"] == "monitor-db" for a in rust_effective)
    # Dry-run writes nothing.
    assert not (py_ws / ".loom" / "tokens" / "index.json").exists()
    assert not (rust_ws / ".loom" / "tokens" / "index.json").exists()


def test_import_from_monitor_pool_and_index_byte_compatible(tmp_path):
    db = _seed_usage_db(tmp_path / "monitor" / "usage.db", _IMPORT_CREDS)
    py_ws = _seed_import_repo(tmp_path / "py")
    rust_ws = _seed_import_repo(tmp_path / "rust")

    py = _run_python_import(py_ws, db)
    assert py.returncode == 0, py.stderr
    rust = _run_rust_import(rust_ws, db)
    assert rust.returncode == 0, rust.stderr

    py_pool = py_ws / ".loom" / "tokens"
    rust_pool = rust_ws / ".loom" / "tokens"

    # index.json manifest rows are byte-compatible (only generated_at differs).
    assert _index_accounts(rust_pool) == _index_accounts(py_pool)
    for row in _index_accounts(rust_pool):
        assert len(row["key_fingerprint"]) == 8
        assert row["source"] == "monitor-db"
        assert "key" not in row
    raw = (rust_pool / "index.json").read_text(encoding="utf-8")
    assert "sk-live" not in raw

    # The materialized token files are identical across both writers.
    for name in ("alice-example.token", "bob-example.token"):
        assert (rust_pool / name).read_text(encoding="utf-8") == (
            py_pool / name
        ).read_text(encoding="utf-8")


def test_import_from_monitor_db_absent_both_fail(tmp_path):
    missing_db = tmp_path / "monitor" / "usage.db"  # never created
    py_ws = _seed_import_repo(tmp_path / "py")
    rust_ws = _seed_import_repo(tmp_path / "rust")

    py = _run_python_import(py_ws, missing_db, "--json")
    rust = _run_rust_import(rust_ws, missing_db, "--json")
    # Both exit non-zero when the store is unavailable.
    assert py.returncode != 0
    assert rust.returncode != 0


def test_check_monitor_source_no_fresh_data_leaves_ranking_untouched(tmp_path):
    index_accounts = [{"name": "acct-a", "email": "a@example.com"}]
    py_ws = _seed_monitor_pool(tmp_path / "py", index_accounts)
    rust_ws = _seed_monitor_pool(tmp_path / "rust", index_accounts)
    monitor_dir = tmp_path / "monitor"
    # STALE ranking.json (>600s old) -> no fresh data.
    stale = (
        datetime.datetime.now(datetime.timezone.utc) - datetime.timedelta(hours=2)
    ).strftime("%Y-%m-%dT%H:%M:%SZ")
    _write_ranking_json(
        monitor_dir, stale,
        [{"email": "a@example.com", "status": "available"}],
    )

    # Pre-seed a distinctive existing .ranking in both pools.
    sentinel = "preexisting|available\n"
    (py_ws / ".loom" / "tokens" / ".ranking").write_text(sentinel, encoding="utf-8")
    (rust_ws / ".loom" / "tokens" / ".ranking").write_text(sentinel, encoding="utf-8")

    py = _run_python_check(
        py_ws / ".loom" / "tokens", monitor_dir, "--source", "monitor", "--ranking"
    )
    assert py.returncode == 0, py.stderr
    rust = _run_rust_check(rust_ws, monitor_dir, "--source", "monitor", "--ranking")
    assert rust.returncode == 0, rust.stderr

    # Both leave the existing .ranking byte-unchanged (empty report, no write).
    assert (py_ws / ".loom" / "tokens" / ".ranking").read_text(
        encoding="utf-8"
    ) == sentinel
    assert (rust_ws / ".loom" / "tokens" / ".ranking").read_text(
        encoding="utf-8"
    ) == sentinel
