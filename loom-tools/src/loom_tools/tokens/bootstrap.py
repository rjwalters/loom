"""Bootstrap the multi-account OAuth token pool from account sources.

Reads numbered ``ACCOUNT_EMAIL_N`` / ``ACCOUNT_KEY_N`` / ``ACCOUNT_TOKEN_FILE_N``
triples and materializes them as per-account ``.token`` files inside
``.loom/tokens/``. Writes an ``index.json`` manifest with sha256
fingerprints (truncated to 8 chars) so drift between the source and on-disk
state can be detected without storing secret material.

**Additive multi-source merge (#3695, #3698).** Accounts are read from up to
three sources and merged so a set of Claude accounts can be declared **once**
and shared across every workspace instead of duplicating ``ACCOUNT_*_N``
triples into every repo's ``.env``. In **precedence order, highest first**:

1. **claude-monitor master (#3698)** — ``~/.claude-monitor/accounts.env`` when
   present (directory resolved via ``monitor.claude_monitor_dir()``, overridable
   with ``LOOM_CLAUDE_MONITOR_DIR``). The companion **claude-monitor** tool
   writes EMAIL+KEY-only entries here; the token filename is auto-derived from
   the email (see :func:`derive_token_filename`). This is a **soft dependency**:
   pure file detection, no import of any claude-monitor package. This is now the
   *primary* home source.
2. **Home master (#3695)** — **opt-in only** (#3704). Consulted only when the
   ``LOOM_ACCOUNTS_ENV`` env var points at a file (conventionally
   ``~/.loom/accounts.env``); set it to ``""`` to disable, leave it unset and
   the home master is **not auto-read** (no default location). Retired as a
   default source by #3704 — the capability survives via the explicit override.
3. **Repo-local** — ``<repo>/.loom/accounts.env`` if present, else the legacy
   ``<repo>/.env`` (override with ``--env`` / ``env_path``).

The sets are merged **by account email** (``ACCOUNT_EMAIL``, case-insensitive)
with precedence **claude-monitor > repo > home**: an entry whose email appears
in a higher-precedence source **overrides** the lower one (e.g. to rotate a
key), and an entry with a new email **adds** to the pool. Accounts present only
in a lower-precedence source are inherited. To *exclude* an inherited account
from a repo, use the ``.allowlist`` pin (``loom-tokens pin``) — the merge only
ever adds/overrides, it never subtracts. **Absent the claude-monitor file,
resolution is byte-for-byte identical to the #3695 home+repo merge (repo
overrides home)**, and a repo with only a legacy ``.env`` and no master behaves
exactly as it did before #3695.

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
from loom_tools.tokens import monitor

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

# Home-dir master accounts file (#3695). The *conventional* opt-in location an
# operator can point LOOM_ACCOUNTS_ENV at; it is **no longer a default source**
# (#3704 retired the auto-read). The home master is consulted only when
# LOOM_ACCOUNTS_ENV is set to a non-empty path (see default_home_accounts_env).
DEFAULT_HOME_ACCOUNTS_ENV = "~/.loom/accounts.env"
HOME_ACCOUNTS_ENV_VAR = "LOOM_ACCOUNTS_ENV"

# claude-monitor master accounts file (#3698). The companion claude-monitor tool
# writes EMAIL+KEY-only entries here; this is the highest-precedence account
# source. The directory is resolved via ``monitor.claude_monitor_dir()`` so
# ``LOOM_CLAUDE_MONITOR_DIR`` overrides it and tests never touch a real
# ``~/.claude-monitor``. Soft dependency: pure file detection, no package import.
MONITOR_ACCOUNTS_ENV_NAME = "accounts.env"

# Characters stripped from an email local-part when deriving a token filename
# (#3697): dots and hyphens, so ``a.b.jones`` -> ``abjones`` and
# ``agent-1`` -> ``agent1``, matching the established naming convention.
_LOCAL_PART_STRIP_RE = re.compile(r"[.-]")
# Any character outside the token-filename safe set is dropped as a final
# guard so the derived name always passes ``_TOKEN_FILE_RE``.
_UNSAFE_FILENAME_CHAR_RE = re.compile(r"[^A-Za-z0-9._-]")


def derive_token_filename(email: str) -> str:
    """Derive a safe ``<name>.token`` filename from an account email (#3697).

    Convention (stable so account identities don't churn):

    * strip dots and hyphens from the local-part;
    * append ``-<first-domain-label>``;
    * lowercase, then drop any remaining unsafe character.

    Examples::

        alice@example.com        -> alice-example.token
        a.b.jones@example.org    -> abjones-example.token
        agent-1@example.com      -> agent1-example.token

    The result always matches :data:`_TOKEN_FILE_RE`. Two *distinct* emails
    can still derive the same stem (e.g. ``ajones@example.com`` and
    ``a.jones@example.com``); that true collision is caught by the existing
    duplicate-filename guard in :func:`bootstrap_tokens`, not silently merged.
    """
    local, sep, domain = email.strip().partition("@")
    local_clean = _LOCAL_PART_STRIP_RE.sub("", local)
    domain_label = domain.split(".")[0] if sep else ""
    stem = f"{local_clean}-{domain_label}" if domain_label else local_clean
    stem = _UNSAFE_FILENAME_CHAR_RE.sub("", stem.lower())
    if not stem:
        stem = "account"
    return f"{stem}.token"


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

    ``source`` is one of:

    * ``"home"`` — only in the ``~/.loom`` home master;
    * ``"repo"`` — only in the repo-local source;
    * ``"repo-override"`` — email present in both home and repo; the repo entry
      won over the home master;
    * ``"monitor"`` (#3698) — only in the claude-monitor master;
    * ``"monitor-override"`` (#3698) — email present in claude-monitor and at
      least one lower-precedence source; the claude-monitor entry won.
    """

    email: str
    key: str
    file: str
    source: str
    index: int  # source index N, for reporting/ordering


def default_home_accounts_env() -> Path | None:
    """Resolve the home-dir master accounts file (#3695, retired as default #3704).

    Precedence:
        1. ``LOOM_ACCOUNTS_ENV`` env var — an explicit path (``~`` expanded).
           The empty string (or all-whitespace) **disables** the master.
        2. Unset — returns ``None``. The home master is **opt-in only**: it is
           no longer auto-read from a default location (#3704). Point
           ``LOOM_ACCOUNTS_ENV`` at a file (conventionally
           ``~/.loom/accounts.env``, :data:`DEFAULT_HOME_ACCOUNTS_ENV`) to
           enable it.

    Returns the resolved :class:`Path` (which may not exist on disk), or
    ``None`` when the master is disabled or has not been explicitly opted in.
    """
    override = os.environ.get(HOME_ACCOUNTS_ENV_VAR)
    if override is not None:
        if not override.strip():
            return None
        return Path(override).expanduser()
    return None


def default_claude_monitor_accounts_env() -> Path:
    """Resolve claude-monitor's master accounts file (#3698).

    Returns ``<claude-monitor-dir>/accounts.env`` where the directory comes from
    :func:`monitor.claude_monitor_dir` (honoring ``LOOM_CLAUDE_MONITOR_DIR`` so
    tests never touch a real ``~/.claude-monitor``). The returned path may not
    exist on disk — the caller checks for presence and degrades silently to the
    home+repo sources when it is absent. This is a **soft dependency**: only the
    directory resolver is imported, never any claude-monitor package.
    """
    return monitor.claude_monitor_dir() / MONITOR_ACCOUNTS_ENV_NAME


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
        # Auto-derive the token filename from the email when omitted (#3697),
        # so a claude-monitor-style EMAIL+KEY-only entry bootstraps directly.
        if not triple.get("file") and triple.get("email"):
            triple = {**triple, "file": derive_token_filename(triple["email"])}
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
    base: list[Account],
    over: list[Account],
    *,
    override_label: str = "repo-override",
) -> list[Account]:
    """Merge the *over* accounts **on top of** the *base* accounts, by email.

    Rules (#3695, generalized in #3698):
        * An account whose email appears only in *base* is inherited (keeps its
          own ``source``).
        * An account whose email appears only in *over* is added (keeps its own
          ``source``).
        * An email present in both: the *over* entry wins and is retagged
          ``source=override_label`` — it keeps *base*'s position in the ordering
          but takes *over*'s key/file/index.

    ``override_label`` defaults to ``"repo-override"`` so the #3695 home+repo
    merge is byte-for-byte unchanged; the #3698 three-source merge passes
    ``"monitor-override"`` when layering the claude-monitor source on top.

    Ordering is deterministic: *base* accounts first (in *base* order), then
    *over*-only additions (in *over* order). Email comparison is case-insensitive
    so ``User@x.com`` and ``user@x.com`` are the same account.
    """

    def key(acct: Account) -> str:
        return acct.email.strip().lower()

    merged: dict[str, Account] = {}
    order: list[str] = []
    for acct in base:
        k = key(acct)
        if k not in merged:
            order.append(k)
        merged[k] = acct  # last base entry for a dup email wins (stable)

    for acct in over:
        k = key(acct)
        if k in merged:
            # Override in place, preserving position but recording provenance.
            merged[k] = Account(
                email=acct.email,
                key=acct.key,
                file=acct.file,
                source=override_label,
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
        # #3695/#3698: where accounts were read from and the effective set.
        self.monitor_env: Path | None = None
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
            "monitor_env": str(self.monitor_env) if self.monitor_env else None,
            "home_env": str(self.home_env) if self.home_env else None,
            "repo_env": str(self.repo_env) if self.repo_env else None,
            "effective": [dict(a) for a in self.effective],
        }


@dataclass
class MaterializeOutcome:
    """Per-file disposition from :func:`materialize_accounts`."""

    written: list[str]
    unchanged: list[str]
    drifted: list[str]
    # Manifest rows for ``index.json`` (never contains secret material).
    manifest_accounts: list[dict[str, object]]


def materialize_accounts(
    accounts: list[Account],
    tokens_dir: Path,
    *,
    force: bool = False,
    dry_run: bool = False,
) -> MaterializeOutcome:
    """Write each account's key to ``<tokens_dir>/<file>`` and build manifest rows.

    Extracted from :func:`bootstrap_tokens` so the claude-monitor DB importer
    (#4006) materializes pools through the *same* writer: identical atomic
    write-then-rename, identical ``0600``/``0700`` modes, identical
    fingerprint-based drift detection, identical ``index.json`` rows. A second
    hand-rolled writer would be free to drift from this one on exactly the
    security-relevant details.

    Drift (an on-disk token whose fingerprint differs from the source) is
    reported and **skipped** unless ``force`` — the operator is told to re-run
    with ``--force`` rather than having a hand-edited token silently clobbered.

    Args:
        accounts: Effective account set; ``file`` values must already be unique
            (the caller checks, since it owns the error message).
        tokens_dir: Destination pool directory (created when not ``dry_run``).
        force: Overwrite tokens whose fingerprint differs from the source.
        dry_run: Compute dispositions without writing anything.

    Returns:
        :class:`MaterializeOutcome` — filenames bucketed by disposition plus
        the manifest rows to hand to :func:`write_index`.
    """
    if not dry_run:
        tokens_dir.mkdir(parents=True, exist_ok=True)
        # Tighten directory mode (best-effort; ignore on FS without chmod).
        try:
            os.chmod(tokens_dir, DIR_MODE)
        except OSError:
            pass

    written: list[str] = []
    unchanged: list[str] = []
    drifted: list[str] = []
    manifest_accounts: list[dict[str, object]] = []

    for acct in accounts:
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
            drifted.append(filename)
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
            unchanged.append(filename)
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
            written.append(filename)

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

    return MaterializeOutcome(
        written=written,
        unchanged=unchanged,
        drifted=drifted,
        manifest_accounts=manifest_accounts,
    )


def write_index(
    manifest_accounts: list[dict[str, object]],
    index_path: Path,
    *,
    dry_run: bool = False,
) -> None:
    """Atomically write the ``index.json`` manifest beside the pool.

    The manifest carries only email / filename / source / 8-char fingerprint —
    :func:`fingerprint` guarantees no secret material lands here.
    """
    manifest = {
        "version": INDEX_VERSION,
        "generated_at": _now_iso(),
        "accounts": manifest_accounts,
    }
    if dry_run:
        return
    # Confirm dir exists (e.g. dry_run was False but no files written).
    index_path.parent.mkdir(parents=True, exist_ok=True)
    index_tmp = index_path.with_suffix(index_path.suffix + ".tmp")
    index_tmp.write_text(
        json.dumps(manifest, indent=2, sort_keys=False) + "\n",
        encoding="utf-8",
    )
    os.replace(index_tmp, index_path)


_HOME_UNSET = object()


def _check_monitor_divergence(valid: list[Account]) -> None:
    """Warn when the merged bootstrap set disagrees with claude-monitor's live store.

    The incident this guards against (#4030): claude-monitor re-authenticated
    every account, writing fresh tokens to ``usage.db`` but never touching
    ``accounts.env`` — the static snapshot ``bootstrap`` reads. ``bootstrap
    --force`` then faithfully rewrote the *revoked* tokens and reported
    ``written=N`` while the pool was dead; the only diagnosis was hand-comparing
    sha256 fingerprints between two files. This surfaces that condition
    automatically and names the remedy.

    Compares what bootstrap is about to write (``valid``) against the live store,
    joining on lowercased email (matching :func:`merge_accounts` and
    ``read_monitor_credentials``), and warns on the **intersection** whose
    fingerprints differ — never auto-switching sources.

    Read-only and non-fatal by contract:

    * **Absent store** (no claude-monitor / no ``usage.db``) — silent. A host
      without claude-monitor is a fully-supported configuration.
    * **Unreadable / locked / older-schema store** — silent. The reader
      normalizes every such failure into :class:`MonitorDbUnavailable`.
    * **Membership-only difference** (an email present on only one side) —
      silent. A repo-local account absent from claude-monitor, or a monitor
      account the pool legitimately pins out, is normal; warning on it would fire
      on healthy hosts and train operators to ignore the message — the exact
      failure this check exists to fix.

    No secret material is printed — emails and 8-char fingerprints only,
    consistent with ``index.json``.
    """
    # Deferred import: ``monitor_db`` imports from this module (``Account``,
    # ``materialize_accounts``, ...), so a top-level import here would create an
    # import cycle and fail at load time. Keep this import function-local — do
    # NOT hoist it to the module top (#4030).
    try:
        from loom_tools.tokens.monitor_db import (
            MonitorDbUnavailable,
            read_monitor_credentials,
        )
    except ImportError:
        return

    try:
        creds = read_monitor_credentials()
    except MonitorDbUnavailable:
        # No store, or an unreadable / locked / older-schema one — degrade to
        # silence. Detection must never fail an otherwise-successful bootstrap.
        return

    live_fp = {c.email.strip().lower(): fingerprint(c.token) for c in creds}
    diverged: list[tuple[str, str, str]] = []
    for acct in valid:
        live = live_fp.get(acct.email.strip().lower())
        if live is None:
            continue  # membership difference — not a key-divergence signal
        env_fp = fingerprint(acct.key)
        if live != env_fp:
            diverged.append((acct.email, env_fp, live))

    if not diverged:
        return

    detail = ", ".join(
        f"{email} (merged fp={merged_fp}, live fp={monitor_fp})"
        for email, merged_fp, monitor_fp in diverged
    )
    log_warning(
        f"{len(diverged)} account(s) in the merged bootstrap sources disagree "
        f"with claude-monitor's live store (usage.db): {detail}. The snapshot in "
        f"accounts.env is stale, not the on-disk tokens — `--force` will NOT fix "
        f"this, it rewrites the same stale values. Run "
        f"`loom-tokens import-from-monitor --force` to adopt the live credentials."
    )


def bootstrap_tokens(
    repo_root: Path,
    *,
    env_path: Path | None = None,
    home_env_path: Path | None | object = _HOME_UNSET,
    force: bool = False,
    dry_run: bool = False,
    tokens_dir: Path | None = None,
) -> BootstrapResult:
    """Bootstrap ``.loom/tokens/`` from the merged account sources (#3695, #3698).

    Reads up to three account sources and merges them **by account email** with
    precedence **claude-monitor > repo > home**, then materializes the effective
    set into ``.loom/tokens/``:

    * the claude-monitor master (``~/.claude-monitor/accounts.env`` when present,
      #3698) — highest precedence, EMAIL+KEY-only entries auto-derive their
      token filename;
    * the home-dir master (#3695) — **opt-in only** (#3704), read only when
      ``LOOM_ACCOUNTS_ENV`` points at a file (conventionally
      ``~/.loom/accounts.env``); not auto-read from a default location;
    * the repo-local source (``<repo>/.loom/accounts.env`` if present, else the
      legacy ``<repo>/.env``).

    Absent the claude-monitor file the behavior is byte-for-byte identical to
    the #3695 home+repo merge (repo overrides home).

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
        tokens_dir: Optional override for the *destination* pool directory. When
            ``None`` (default) the pool is written to ``<repo>/.loom/tokens``
            (unchanged behavior). Pass a directory (e.g. the shared
            machine-level pool ``~/.loom/tokens``) to materialize a pool shared
            across every repo the daemon dispatches into (issue #3938). Only the
            write target changes; account *sources* are resolved exactly as
            before.

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
    # Destination pool dir: the repo-local pool by default, or an explicit
    # override (e.g. the shared machine-level pool for `--shared`, issue #3938).
    if tokens_dir is None:
        tokens_dir = paths.loom_dir / "tokens"
    index_path = tokens_dir / "index.json"

    # Resolve the sources. `home_env_path` uses a sentinel so an explicit
    # None (disable) is distinguishable from an omitted argument (use default).
    if home_env_path is _HOME_UNSET:
        home_file = default_home_accounts_env()
    else:
        home_file = home_env_path  # type: ignore[assignment]
    repo_file = resolve_repo_env(repo_root, env_path)
    # claude-monitor master (#3698): highest precedence, soft dependency. The
    # resolver honors LOOM_CLAUDE_MONITOR_DIR; absent on disk it degrades
    # silently to the home+repo path (no import, no crash).
    monitor_file = default_claude_monitor_accounts_env()

    result = BootstrapResult()
    result.dry_run = dry_run
    result.tokens_dir = tokens_dir
    result.index_path = index_path
    result.monitor_env = monitor_file if monitor_file.is_file() else None
    result.home_env = home_file if (home_file and home_file.is_file()) else None
    result.repo_env = repo_file if repo_file.is_file() else None

    monitor_present = monitor_file.is_file()
    home_present = bool(home_file and home_file.is_file())
    repo_present = repo_file.is_file()
    if not monitor_present and not home_present and not repo_present:
        raise FileNotFoundError(
            "No account source found. Looked for a repo-local source at "
            f"{repo_file}, a home master at "
            f"{home_file if home_file else '(disabled)'}, and a claude-monitor "
            f"master at {monitor_file}. "
            "Declare accounts in one of them (see `loom-tokens bootstrap --help`)."
        )

    monitor_accounts: list[Account] = []
    if monitor_present:
        monitor_accounts = _assemble_valid_accounts(
            parse_env_accounts(monitor_file), "monitor"
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

    # Three-source merge, precedence claude-monitor > repo > home. First layer
    # repo over home (unchanged #3695 behavior — repo-override), then layer
    # claude-monitor on top (monitor-override). Absent the monitor source the
    # second merge is a no-op, so behavior is byte-for-byte identical to #3695.
    home_repo = merge_accounts(home_accounts, repo_accounts)
    valid = merge_accounts(
        home_repo, monitor_accounts, override_label="monitor-override"
    )

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
            str(p)
            for p in (result.monitor_env, result.home_env, result.repo_env)
            if p
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

    outcome = materialize_accounts(
        valid, tokens_dir, force=force, dry_run=dry_run
    )
    result.written.extend(outcome.written)
    result.unchanged.extend(outcome.unchanged)
    result.drifted.extend(outcome.drifted)

    write_index(outcome.manifest_accounts, index_path, dry_run=dry_run)

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

    # #4030: detection for the stale-snapshot incident. Runs last so the warning
    # is the final line the operator sees, and runs under --dry-run too (it is
    # read-only detection — suppressing it there would hide the divergence from
    # the exact command used to inspect state). Silent when no claude-monitor
    # store is present; never fails the bootstrap.
    _check_monitor_divergence(valid)

    return result
