"""Bootstrap the multi-account OAuth token pool from account sources.

Reads numbered ``ACCOUNT_EMAIL_N`` / ``ACCOUNT_KEY_N`` / ``ACCOUNT_TOKEN_FILE_N``
triples and materializes them as per-account ``.token`` files inside
``.loom/tokens/``. Writes an ``index.json`` manifest with sha256
fingerprints (truncated to 8 chars) so drift between the source and on-disk
state can be detected without storing secret material.

**Home-dir master + per-repo override (#3695).** Accounts are read from two
sources and merged so a set of Claude accounts can be declared **once** and
shared across every workspace instead of duplicating ``ACCOUNT_*_N`` triples
into every repo's ``.env``:

1. **Home master** — ``~/.loom/accounts.env`` (override with the
   ``LOOM_ACCOUNTS_ENV`` env var; set it to ``""`` to disable the master).
2. **Repo-local** — ``<repo>/.loom/accounts.env`` if present, else the legacy
   ``<repo>/.env`` (override with ``--env`` / ``env_path``).

The two sets are merged **by account email** (``ACCOUNT_EMAIL``): a repo-local
entry whose email also appears in the master **overrides** it (e.g. to rotate
a key), and a repo-local entry with a new email **adds** to the pool. Accounts
present only in the master are inherited. To *exclude* a master account from a
repo, use the ``.allowlist`` pin (``loom-tokens pin``) — the merge only ever
adds/overrides, it never subtracts. A repo with only a legacy ``.env`` and no
master behaves exactly as before this change.

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
from dataclasses import dataclass
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

INDEX_VERSION = 2
DIR_MODE = 0o700
FILE_MODE = 0o600

# Home-dir master accounts file (#3695). Shared across every workspace so the
# account set is declared once. Override with LOOM_ACCOUNTS_ENV; set that to
# the empty string to disable the master entirely.
DEFAULT_HOME_ACCOUNTS_ENV = "~/.loom/accounts.env"
HOME_ACCOUNTS_ENV_VAR = "LOOM_ACCOUNTS_ENV"


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
# Source resolution + merge (home master over repo-local, #3695)
# ---------------------------------------------------------------------------


@dataclass(frozen=True)
class Account:
    """A single validated account triple with its provenance.

    ``source`` is one of ``"home"`` (only in the master), ``"repo"`` (only in
    the repo-local source), or ``"repo-override"`` (email present in both; the
    repo-local entry won).
    """

    email: str
    key: str
    file: str
    source: str
    index: int  # source index N, for reporting/ordering


def default_home_accounts_env() -> Path | None:
    """Resolve the home-dir master accounts file (#3695).

    Precedence:
        1. ``LOOM_ACCOUNTS_ENV`` env var — an explicit path (``~`` expanded).
           The empty string (or all-whitespace) **disables** the master.
        2. ``~/.loom/accounts.env`` (the default location).

    Returns the resolved :class:`Path` (which may not exist on disk), or
    ``None`` when the master has been explicitly disabled.
    """
    override = os.environ.get(HOME_ACCOUNTS_ENV_VAR)
    if override is not None:
        if not override.strip():
            return None
        return Path(override).expanduser()
    return Path(DEFAULT_HOME_ACCOUNTS_ENV).expanduser()


def resolve_repo_env(repo_root: Path, env_path: Path | None) -> Path:
    """Resolve the repo-local accounts source path (#3695).

    Precedence:
        1. Explicit ``env_path`` (the ``--env`` flag) — used verbatim.
        2. ``<repo>/.loom/accounts.env`` if it exists (the dedicated file).
        3. ``<repo>/.env`` (legacy fallback — preserves pre-#3695 behaviour).

    The returned path may not exist on disk (the legacy default is returned
    even when absent so the caller can report it).
    """
    if env_path is not None:
        return env_path
    dedicated = repo_root / ".loom" / "accounts.env"
    if dedicated.is_file():
        return dedicated
    return repo_root / ".env"


def _assemble_valid_accounts(
    accounts: dict[int, dict[str, str]],
    source: str,
) -> list[Account]:
    """Validate parsed triples and return complete, safe :class:`Account`s.

    Incomplete triples (missing email/key/file) and unsafe token filenames are
    warned about and skipped — mirroring the pre-#3695 inline validation, now
    factored out so both the home master and the repo-local source run it.
    """
    out: list[Account] = []
    for n in sorted(accounts):
        triple = accounts[n]
        missing = [k for k in ("email", "key", "file") if not triple.get(k)]
        if missing:
            log_warning(
                f"[{source}] ACCOUNT_*_{n}: incomplete triple "
                f"(missing: {', '.join(sorted(missing))}); skipping."
            )
            continue
        filename = triple["file"]
        if not _TOKEN_FILE_RE.match(filename) or "/" in filename or "\\" in filename:
            log_warning(
                f"[{source}] ACCOUNT_TOKEN_FILE_{n}={filename!r}: "
                f"unsafe filename; skipping."
            )
            continue
        out.append(
            Account(
                email=triple["email"],
                key=triple["key"],
                file=filename,
                source=source,
                index=n,
            )
        )
    return out


def merge_accounts(
    home: list[Account],
    repo: list[Account],
) -> list[Account]:
    """Merge repo-local accounts **over** the home master, keyed by email.

    Rules (#3695):
        * An account whose email appears only in the master is inherited
          (``source="home"``).
        * An account whose email appears only in the repo-local source is
          added (``source="repo"``).
        * An email present in both: the repo-local entry wins and is tagged
          ``source="repo-override"`` — it keeps the master's position in the
          ordering but takes the repo's key/file/index.

    Ordering is deterministic: master accounts first (in master index order),
    then repo-only additions (in repo index order). Email comparison is
    case-insensitive so ``User@x.com`` and ``user@x.com`` are the same account.
    """

    def key(acct: Account) -> str:
        return acct.email.strip().lower()

    merged: dict[str, Account] = {}
    order: list[str] = []
    for acct in home:
        k = key(acct)
        if k not in merged:
            order.append(k)
        merged[k] = acct  # last home entry for a dup email wins (stable)

    for acct in repo:
        k = key(acct)
        if k in merged:
            # Override in place, preserving position but recording provenance.
            merged[k] = Account(
                email=acct.email,
                key=acct.key,
                file=acct.file,
                source="repo-override",
                index=acct.index,
            )
        else:
            order.append(k)
            merged[k] = acct

    return [merged[k] for k in order]


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
        # #3695: where accounts were read from and the effective merged set.
        self.home_env: Path | None = None
        self.repo_env: Path | None = None
        # Each entry: {"email", "name", "file", "source"}.
        self.effective: list[dict[str, str]] = []

    def to_dict(self) -> dict[str, object]:
        return {
            "written": list(self.written),
            "unchanged": list(self.unchanged),
            "drifted": list(self.drifted),
            "skipped": list(self.skipped),
            "dry_run": self.dry_run,
            "tokens_dir": str(self.tokens_dir) if self.tokens_dir else None,
            "index_path": str(self.index_path) if self.index_path else None,
            "home_env": str(self.home_env) if self.home_env else None,
            "repo_env": str(self.repo_env) if self.repo_env else None,
            "effective": [dict(a) for a in self.effective],
        }


_HOME_UNSET = object()


def bootstrap_tokens(
    repo_root: Path,
    *,
    env_path: Path | None = None,
    home_env_path: Path | None | object = _HOME_UNSET,
    force: bool = False,
    dry_run: bool = False,
) -> BootstrapResult:
    """Bootstrap ``.loom/tokens/`` from the merged account sources (#3695).

    Reads the home-dir master (``~/.loom/accounts.env`` by default) and the
    repo-local source (``<repo>/.loom/accounts.env`` if present, else the
    legacy ``<repo>/.env``), merges them **by account email** with the
    repo-local source overriding the master, and materializes the effective
    set into ``.loom/tokens/``.

    Args:
        repo_root: Repository root (must contain ``.loom/``).
        env_path: Optional override for the repo-local source file. When
            ``None`` (default) it is resolved by :func:`resolve_repo_env`.
        home_env_path: Optional override for the home master. Omit to use the
            default resolution (:func:`default_home_accounts_env`, honoring
            ``LOOM_ACCOUNTS_ENV``); pass ``None`` to disable the master
            entirely for this call; pass a :class:`Path` to point elsewhere.
        force: When ``True``, overwrite existing token files even if their
            contents match the source. When ``False`` (default), files whose
            fingerprint matches are left alone.
        dry_run: When ``True``, no files are written; the result lists what
            *would* change (and the effective merged set with provenance).

    Returns:
        :class:`BootstrapResult` summarising the operation. ``.effective``
        lists the merged account set and where each account came from.

    Raises:
        FileNotFoundError: If **neither** the home master nor the repo-local
            source exists on disk.
        ValueError: If two effective accounts resolve to the same token
            filename (they would clobber each other on disk).
    """
    paths = LoomPaths(repo_root)
    tokens_dir = paths.loom_dir / "tokens"
    index_path = tokens_dir / "index.json"

    # Resolve the two sources. `home_env_path` uses a sentinel so an explicit
    # None (disable) is distinguishable from an omitted argument (use default).
    if home_env_path is _HOME_UNSET:
        home_file = default_home_accounts_env()
    else:
        home_file = home_env_path  # type: ignore[assignment]
    repo_file = resolve_repo_env(repo_root, env_path)

    result = BootstrapResult()
    result.dry_run = dry_run
    result.tokens_dir = tokens_dir
    result.index_path = index_path
    result.home_env = home_file if (home_file and home_file.is_file()) else None
    result.repo_env = repo_file if repo_file.is_file() else None

    home_present = bool(home_file and home_file.is_file())
    repo_present = repo_file.is_file()
    if not home_present and not repo_present:
        raise FileNotFoundError(
            "No account source found. Looked for a repo-local source at "
            f"{repo_file} and a home master at "
            f"{home_file if home_file else '(disabled)'}. "
            "Declare accounts in one of them (see `loom-tokens bootstrap --help`)."
        )

    home_accounts: list[Account] = []
    if home_present:
        home_accounts = _assemble_valid_accounts(
            parse_env_accounts(home_file), "home"  # type: ignore[arg-type]
        )
    repo_accounts: list[Account] = []
    if repo_present:
        repo_accounts = _assemble_valid_accounts(
            parse_env_accounts(repo_file), "repo"
        )

    valid = merge_accounts(home_accounts, repo_accounts)

    # Record the effective merged set (with provenance) for reporting even
    # when nothing is written — bootstrap --dry-run consumes this.
    result.effective = [
        {
            "email": a.email,
            "name": _name_from_file(a.file),
            "file": a.file,
            "source": a.source,
        }
        for a in valid
    ]

    if not valid:
        srcs = ", ".join(
            str(p) for p in (result.home_env, result.repo_env) if p
        )
        log_warning(
            f"No complete ACCOUNT_*_N triples in {srcs or 'the account source(s)'}; "
            f"nothing to bootstrap."
        )
        return result

    # Detect duplicate filenames (would otherwise clobber each other). This
    # runs on the *merged* set, so a repo account reusing a master account's
    # token filename under a different email is caught here.
    seen_files: dict[str, Account] = {}
    for acct in valid:
        prior = seen_files.get(acct.file)
        if prior is not None:
            log_error(
                f"Duplicate ACCOUNT_TOKEN_FILE: {acct.file!r} maps to both "
                f"{prior.email!r} ({prior.source}) and {acct.email!r} "
                f"({acct.source}); aborting."
            )
            raise ValueError(f"duplicate token filename: {acct.file}")
        seen_files[acct.file] = acct

    if not dry_run:
        tokens_dir.mkdir(parents=True, exist_ok=True)
        # Tighten directory mode (best-effort; ignore on FS without chmod).
        try:
            os.chmod(tokens_dir, DIR_MODE)
        except OSError:
            pass

    manifest_accounts: list[dict[str, object]] = []
    for acct in valid:
        n, email, key, filename = acct.index, acct.email, acct.key, acct.file
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
                f"DRIFT: {filename} on disk does not match the account source "
                f"(disk fp={existing_fp}, source fp={new_fp}); "
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
                    "source": acct.source,
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
                "source": acct.source,
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
            f"{len(result.drifted)} token(s) drifted from the account source; "
            f"re-run with --force to overwrite on-disk values."
        )
    elif result.written and not dry_run:
        log_success(
            f"Bootstrapped {len(result.written)} token(s) into {tokens_dir}"
        )

    return result
