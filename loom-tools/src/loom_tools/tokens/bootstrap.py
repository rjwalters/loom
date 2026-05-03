"""Bootstrap the multi-account OAuth token pool from ``.env``.

Reads numbered ``ACCOUNT_EMAIL_N`` / ``ACCOUNT_KEY_N`` / ``ACCOUNT_TOKEN_FILE_N``
triples and materializes them as per-account ``.token`` files inside
``.loom/tokens/``. Writes an ``index.json`` manifest with sha256
fingerprints (truncated to 8 chars) so drift between ``.env`` and on-disk
state can be detected without storing secret material.

Lean-genius reference: ``scripts/agents/claude-wrapper.sh:46-66``. The
bootstrap behaviour mirrors that shell snippet (strip surrounding quotes
and embedded newlines, mode ``0600`` per file) but uses
``ACCOUNT_TOKEN_FILE_N`` for filenames instead of ``agent-N.token`` for
readability.
"""

from __future__ import annotations

import hashlib
import json
import os
import re
from datetime import datetime, timezone
from pathlib import Path

from loom_tools.common.logging import (
    log_error,
    log_info,
    log_success,
    log_warning,
)
from loom_tools.common.paths import LoomPaths

# ``ACCOUNT_<FIELD>_<N>=<value>`` — N is 1+ digits, FIELD is one of the
# triple keys we recognize. Anchored to start of line.
_ENV_LINE_RE = re.compile(
    r"^ACCOUNT_(EMAIL|KEY|TOKEN_FILE)_(\d+)\s*=\s*(.*)$"
)

# Filename safety: lowercase letters, digits, dot, dash, underscore. Must
# end with .token (case-insensitive). Matches lean-genius conventions.
_TOKEN_FILE_RE = re.compile(r"^[A-Za-z0-9._-]+\.token$")

INDEX_VERSION = 1
DIR_MODE = 0o700
FILE_MODE = 0o600


# ---------------------------------------------------------------------------
# .env parsing
# ---------------------------------------------------------------------------


def _strip_value(raw: str) -> str:
    """Strip surrounding quotes and embedded newlines from a value.

    Mirrors the shell snippet ``printf '%s' "$val" | tr -d "'\"\n"`` from
    lean-genius's ``claude-wrapper.sh:61``. Trailing comment after the
    value (e.g. ``KEY=value # comment``) is *not* stripped: we treat the
    whole RHS as opaque, only normalising whitespace and quotes.
    """
    s = raw.strip()
    # Drop a single matching pair of surrounding quotes.
    if len(s) >= 2 and s[0] == s[-1] and s[0] in ("'", '"'):
        s = s[1:-1]
    # Remove embedded newlines and stray quotes (matches `tr -d "'\"\n"`).
    s = s.replace("\n", "").replace("\r", "")
    s = s.replace("'", "").replace('"', "")
    return s


def parse_env_accounts(env_path: Path) -> dict[int, dict[str, str]]:
    """Parse ``.env`` and return ``{N: {"email": ..., "key": ..., "file": ...}}``.

    Lines that don't match the ``ACCOUNT_*_N=`` pattern are ignored
    (this is not a general-purpose ``.env`` parser). Partial triples are
    returned as-is so the caller can warn and skip them.

    Args:
        env_path: Path to the ``.env`` file (must exist).

    Returns:
        Mapping from account index ``N`` to a dict with keys ``email``,
        ``key``, and ``file`` (any subset may be present).
    """
    accounts: dict[int, dict[str, str]] = {}
    text = env_path.read_text(encoding="utf-8", errors="replace")
    field_to_key = {"EMAIL": "email", "KEY": "key", "TOKEN_FILE": "file"}

    for line in text.splitlines():
        stripped = line.lstrip()
        if not stripped or stripped.startswith("#"):
            continue
        m = _ENV_LINE_RE.match(stripped)
        if not m:
            continue
        field, n_str, raw_value = m.group(1), m.group(2), m.group(3)
        try:
            n = int(n_str)
        except ValueError:
            continue
        value = _strip_value(raw_value)
        accounts.setdefault(n, {})[field_to_key[field]] = value

    return accounts


# ---------------------------------------------------------------------------
# Fingerprinting + manifest
# ---------------------------------------------------------------------------


def fingerprint(secret: str) -> str:
    """Return the first 8 hex chars of sha256(secret).

    Used in ``index.json`` for drift detection. **Never** stores the
    secret itself — only enough entropy to detect when ``.env`` and the
    on-disk token diverge.
    """
    return hashlib.sha256(secret.encode("utf-8")).hexdigest()[:8]


def _now_iso() -> str:
    return datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")


def _name_from_file(filename: str) -> str:
    """Strip the trailing ``.token`` suffix to derive a logical account name."""
    if filename.lower().endswith(".token"):
        return filename[: -len(".token")]
    return filename


# ---------------------------------------------------------------------------
# Public entry point
# ---------------------------------------------------------------------------


class BootstrapResult:
    """Outcome of a single ``bootstrap_tokens()`` call."""

    def __init__(self) -> None:
        self.written: list[str] = []
        self.unchanged: list[str] = []
        self.drifted: list[str] = []
        self.skipped: list[str] = []
        self.dry_run: bool = False
        self.tokens_dir: Path | None = None
        self.index_path: Path | None = None

    def to_dict(self) -> dict[str, object]:
        return {
            "written": list(self.written),
            "unchanged": list(self.unchanged),
            "drifted": list(self.drifted),
            "skipped": list(self.skipped),
            "dry_run": self.dry_run,
            "tokens_dir": str(self.tokens_dir) if self.tokens_dir else None,
            "index_path": str(self.index_path) if self.index_path else None,
        }


def bootstrap_tokens(
    repo_root: Path,
    *,
    env_path: Path | None = None,
    force: bool = False,
    dry_run: bool = False,
) -> BootstrapResult:
    """Bootstrap ``.loom/tokens/`` from ``.env`` triples.

    Args:
        repo_root: Repository root (must contain ``.loom/``).
        env_path: Optional override for the ``.env`` file. Defaults to
            ``repo_root / ".env"``.
        force: When ``True``, overwrite existing token files even if
            their contents match ``.env`` (rewrites mode and timestamp).
            When ``False`` (default), files whose fingerprint matches
            ``.env`` are left alone.
        dry_run: When ``True``, no files are written; the result lists
            what *would* change.

    Returns:
        :class:`BootstrapResult` summarising the operation.

    Raises:
        FileNotFoundError: If ``.env`` does not exist at ``env_path``.
    """
    paths = LoomPaths(repo_root)
    tokens_dir = paths.loom_dir / "tokens"
    index_path = tokens_dir / "index.json"
    env_file = env_path if env_path is not None else (repo_root / ".env")

    result = BootstrapResult()
    result.dry_run = dry_run
    result.tokens_dir = tokens_dir
    result.index_path = index_path

    if not env_file.is_file():
        raise FileNotFoundError(f".env not found at {env_file}")

    accounts = parse_env_accounts(env_file)
    if not accounts:
        log_warning(
            f"No ACCOUNT_*_N entries found in {env_file}; nothing to bootstrap."
        )
        return result

    # Validate triples and build the work list, in stable order by N.
    valid: list[tuple[int, str, str, str]] = []  # (n, email, key, filename)
    for n in sorted(accounts):
        triple = accounts[n]
        missing = [k for k in ("email", "key", "file") if not triple.get(k)]
        if missing:
            log_warning(
                f"ACCOUNT_*_{n}: incomplete triple "
                f"(missing: {', '.join(sorted(missing))}); skipping."
            )
            continue
        filename = triple["file"]
        if not _TOKEN_FILE_RE.match(filename) or "/" in filename or "\\" in filename:
            log_warning(
                f"ACCOUNT_TOKEN_FILE_{n}={filename!r}: unsafe filename; skipping."
            )
            continue
        valid.append((n, triple["email"], triple["key"], filename))

    if not valid:
        log_warning(
            f"No complete ACCOUNT_*_N triples in {env_file}; nothing to bootstrap."
        )
        return result

    # Detect duplicate filenames (would otherwise clobber each other).
    seen_files: dict[str, int] = {}
    for n, _email, _key, filename in valid:
        if filename in seen_files:
            log_error(
                f"Duplicate ACCOUNT_TOKEN_FILE: {filename!r} appears for "
                f"both _{seen_files[filename]} and _{n}; aborting."
            )
            raise ValueError(f"duplicate token filename: {filename}")
        seen_files[filename] = n

    if not dry_run:
        tokens_dir.mkdir(parents=True, exist_ok=True)
        # Tighten directory mode (best-effort; ignore on FS without chmod).
        try:
            os.chmod(tokens_dir, DIR_MODE)
        except OSError:
            pass

    manifest_accounts: list[dict[str, object]] = []
    for n, email, key, filename in valid:
        token_path = tokens_dir / filename
        new_fp = fingerprint(key)

        existing_fp: str | None = None
        if token_path.exists():
            try:
                existing = token_path.read_text(encoding="utf-8")
            except OSError as exc:
                log_warning(f"Could not read {token_path}: {exc}; will rewrite.")
                existing = None  # type: ignore[assignment]
            if existing is not None:
                existing_fp = fingerprint(existing.rstrip("\n"))

        action: str
        if existing_fp is None:
            action = "written"
        elif existing_fp == new_fp:
            action = "unchanged" if not force else "written"
        else:
            action = "drifted"

        if action == "drifted" and not force:
            log_warning(
                f"DRIFT: {filename} on disk does not match .env "
                f"(disk fp={existing_fp}, env fp={new_fp}); "
                f"re-run with --force to overwrite."
            )
            result.drifted.append(filename)
            # Still record current on-disk fingerprint in manifest so
            # operators can see it.
            manifest_accounts.append(
                {
                    "env_index": n,
                    "name": _name_from_file(filename),
                    "email": email,
                    "file": filename,
                    "key_fingerprint": existing_fp,
                    "drift": True,
                    "env_fingerprint": new_fp,
                }
            )
            continue

        if action == "unchanged":
            result.unchanged.append(filename)
        else:
            # written (new file or force-overwrite)
            if not dry_run:
                # Write atomically: temp file + rename.
                tmp = token_path.with_suffix(token_path.suffix + ".tmp")
                tmp.write_text(key, encoding="utf-8")
                try:
                    os.chmod(tmp, FILE_MODE)
                except OSError:
                    pass
                os.replace(tmp, token_path)
                # Re-apply mode in case `replace` reset it on some FS.
                try:
                    os.chmod(token_path, FILE_MODE)
                except OSError:
                    pass
            result.written.append(filename)

        manifest_accounts.append(
            {
                "env_index": n,
                "name": _name_from_file(filename),
                "email": email,
                "file": filename,
                "key_fingerprint": new_fp,
            }
        )

    # Write the manifest.
    manifest = {
        "version": INDEX_VERSION,
        "generated_at": _now_iso(),
        "accounts": manifest_accounts,
    }
    if not dry_run:
        # Confirm dir exists (e.g. dry_run was False but no files written).
        tokens_dir.mkdir(parents=True, exist_ok=True)
        index_tmp = index_path.with_suffix(index_path.suffix + ".tmp")
        index_tmp.write_text(
            json.dumps(manifest, indent=2, sort_keys=False) + "\n",
            encoding="utf-8",
        )
        os.replace(index_tmp, index_path)

    log_info(
        f"bootstrap: written={len(result.written)} "
        f"unchanged={len(result.unchanged)} "
        f"drifted={len(result.drifted)} "
        f"skipped={len(result.skipped)} "
        f"(dry_run={dry_run})"
    )
    if result.drifted and not force:
        log_warning(
            f"{len(result.drifted)} token(s) drifted from .env; "
            f"re-run with --force to overwrite on-disk values."
        )
    elif result.written and not dry_run:
        log_success(
            f"Bootstrapped {len(result.written)} token(s) into {tokens_dir}"
        )

    return result
