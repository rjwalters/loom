"""Import **live** account OAuth tokens from claude-monitor's store (#4006).

Loom already treats claude-monitor as its primary account source, but through
``~/.claude-monitor/accounts.env`` — a **static snapshot** an operator wrote by
hand at some point in the past. claude-monitor itself keeps the *live*
credentials in its SQLite store (``usage.db`` → ``oauth_credentials``), and
refreshes them as accounts are re-authenticated.

Those two surfaces drift, and the drift is silent and total. The incident that
motivated this module: every account in the pool began returning

    401 {"type":"authentication_error",
         "message":"OAuth access token has been revoked."}

so the dynamic concurrency cap collapsed to ``min(healthy 0 × …) = 0`` and the
daemon stopped dispatching entirely. The operator *had* re-authenticated every
account — but in claude-monitor, which wrote the fresh tokens to ``usage.db``
and never touched ``accounts.env``. Loom kept reading the stale file, and
``loom-tokens bootstrap --force`` faithfully rewrote the same revoked tokens,
because the snapshot itself was what had gone stale. There was no Loom-side
command to pull from the live store; the standing workaround was hand-copying a
pool between hosts.

This module closes that gap: it reads the live store directly and materializes
the pool through the *same* writer ``bootstrap`` uses
(:func:`bootstrap.materialize_accounts`), so atomic write-then-rename, ``0600``
file / ``0700`` directory modes, fingerprint drift detection, and the
``index.json`` manifest are identical by construction rather than by
convention.

**Read-only, and a soft dependency.** The database is opened
``file:…?mode=ro`` (never written, never migrated — it belongs to another
tool) and every failure mode is a clean typed error rather than a crash:
claude-monitor absent, ``usage.db`` absent, or a schema without
``oauth_credentials`` (an older claude-monitor). ``LOOM_CLAUDE_MONITOR_DIR``
relocates the directory, so tests never touch a real ``~/.claude-monitor``.

**Secrets.** ``oauth_credentials.access_token`` is raw secret material. It is
carried in memory only, written solely to ``0600`` token files, and never
logged — logs and the manifest carry email, filename, and the 8-char
fingerprint only.
"""

from __future__ import annotations

import logging
import os
import re
import sqlite3
import urllib.parse
from dataclasses import dataclass
from pathlib import Path

from loom_tools.common.logging import log_info, log_warning
from loom_tools.tokens import monitor
from loom_tools.tokens.bootstrap import (
    Account,
    derive_token_filename,
    materialize_accounts,
    write_index,
)

logger = logging.getLogger(__name__)

# claude-monitor's SQLite store, alongside ranking.json / accounts.env in the
# directory resolved by ``monitor.claude_monitor_dir()``.
MONITOR_DB_NAME = "usage.db"

# Provenance tag recorded in index.json for accounts sourced from the live DB.
# Deliberately distinct from "monitor" (the accounts.env snapshot) so an
# operator reading the manifest can tell which surface a token came from.
SOURCE_MONITOR_DB = "monitor-db"

# claude-monitor labels an account either by bare email or as
# "<email>'s Organization". The bare-email case needs no parsing; this pattern
# recovers the email from the organization form when the ``accounts`` join
# yields nothing.
_ORG_LABEL_RE = re.compile(r"^(?P<email>.+?)'s Organization$")

# Opening the DB should be instant; a bounded timeout means a lock held by
# claude-monitor surfaces as an error instead of hanging a sweep dispatch.
_SQLITE_TIMEOUT_SECONDS = 5.0


class MonitorDbUnavailable(RuntimeError):
    """The claude-monitor credential store could not be read.

    Raised when the directory or ``usage.db`` is absent, or the schema has no
    ``oauth_credentials`` table. Callers surface the message to the operator —
    it names the path that was tried.
    """


@dataclass(frozen=True)
class MonitorCredential:
    """One active credential row resolved to an email."""

    email: str
    token: str
    label: str


def monitor_db_path(monitor_dir: Path | None = None) -> Path:
    """Return the path to claude-monitor's ``usage.db``.

    Honors ``LOOM_CLAUDE_MONITOR_DIR`` via :func:`monitor.claude_monitor_dir`.
    """
    base = monitor_dir or monitor.claude_monitor_dir()
    return base / MONITOR_DB_NAME


def _read_only_uri(path: Path) -> str:
    """Build the ``mode=ro`` SQLite URI for *path*.

    The path is percent-encoded before interpolation. Without this, a ``?`` or
    ``#`` anywhere in the path terminates the URI early and the ``mode=ro``
    query parameter is silently dropped — the connection then opens
    **read-write** against claude-monitor's live database (#4029). ``%`` fares
    even worse: the unencoded form fails to open a database that is present and
    readable.

    ``urllib.parse.quote`` (default ``safe='/'``) encodes ``?``, ``#``, ``%``,
    and spaces while leaving the path separators intact. Unlike
    :meth:`pathlib.Path.as_uri` it has no absolute-path precondition, so a
    relative ``LOOM_CLAUDE_MONITOR_DIR`` still yields a usable URI (resolved
    against the cwd, as the unencoded form was) rather than raising
    ``ValueError``.
    """
    return f"file:{urllib.parse.quote(str(path))}?mode=ro"


def _email_from_label(label: str) -> str | None:
    """Recover an email from a credential label, or None if it isn't one.

    claude-monitor uses either the bare email or ``"<email>'s Organization"``.
    Anything without an ``@`` is a display name we cannot join on.
    """
    text = (label or "").strip()
    if not text:
        return None
    m = _ORG_LABEL_RE.match(text)
    if m:
        text = m.group("email").strip()
    return text if "@" in text else None


def read_monitor_credentials(
    db_path: Path | None = None,
    *,
    monitor_dir: Path | None = None,
) -> list[MonitorCredential]:
    """Read active credentials from claude-monitor's store.

    Only ``is_active = 1`` rows with a non-empty ``access_token`` are returned:
    claude-monitor deactivates rows for accounts an operator has removed, and
    importing those would repopulate the pool with accounts they retired.

    ``expires_at`` is deliberately **not** used as a filter. Observed rows carry
    timestamps months in the past while the token still authenticates
    (claude-monitor refreshes the value in place without always updating the
    column), so filtering on it would discard working credentials. Health is
    established by probing (``loom-tokens check``), which is authoritative.

    When one email has several active rows, the highest ``id`` wins — rows are
    append-ordered, so that is the most recently stored credential.

    Returns:
        Credentials ordered by row id, de-duplicated by email.

    Raises:
        MonitorDbUnavailable: The directory, database file, or
            ``oauth_credentials`` table is missing.
    """
    path = db_path or monitor_db_path(monitor_dir)
    if not path.is_file():
        raise MonitorDbUnavailable(
            f"claude-monitor database not found at {path}. "
            "Is claude-monitor installed on this host? "
            "(Set LOOM_CLAUDE_MONITOR_DIR to point elsewhere.)"
        )

    # Read-only URI: this database belongs to claude-monitor. We never write,
    # migrate, or take a write lock on it. The path is percent-encoded so a
    # '?' or '#' in it cannot silently void the mode=ro guarantee (#4029).
    uri = _read_only_uri(path)
    try:
        conn = sqlite3.connect(uri, uri=True, timeout=_SQLITE_TIMEOUT_SECONDS)
    except sqlite3.Error as exc:
        raise MonitorDbUnavailable(f"Could not open {path}: {exc}") from exc

    try:
        # LEFT JOIN so a credential whose account row is missing still comes
        # back — its email is then recovered from the label.
        rows = conn.execute(
            """
            SELECT c.id, c.label, c.access_token, a.email
              FROM oauth_credentials c
              LEFT JOIN accounts a ON a.id = c.account_id
             WHERE c.is_active = 1
             ORDER BY c.id
            """
        ).fetchall()
    except sqlite3.Error as exc:
        # A missing oauth_credentials table (older claude-monitor) lands here as
        # an OperationalError whose message names the missing table — only then
        # is the "predates the credential store" hint correct. Any other
        # sqlite3.Error (e.g. a hot WAL missing its -shm sidecar, a corrupt
        # file) reaches the same branch, and attributing it to a missing table
        # would send the reader down the wrong path.
        hint = ""
        if isinstance(exc, sqlite3.OperationalError) and "no such table" in str(exc):
            hint = " This claude-monitor may predate the credential store."
        raise MonitorDbUnavailable(
            f"Could not read oauth_credentials from {path}: {exc}.{hint}"
        ) from exc
    finally:
        conn.close()

    by_email: dict[str, MonitorCredential] = {}
    for _row_id, label, access_token, joined_email in rows:
        token = (access_token or "").strip()
        if not token:
            logger.debug("monitor-db: row %r has no access_token; skipping", label)
            continue
        email = (joined_email or "").strip() or _email_from_label(label or "")
        if not email:
            log_warning(
                f"claude-monitor credential {label!r} has no resolvable email; "
                "skipping (cannot derive a stable token filename)."
            )
            continue
        # Later rows (higher id) supersede earlier ones for the same email.
        by_email[email.lower()] = MonitorCredential(
            email=email, token=token, label=label or email
        )

    return list(by_email.values())


def credentials_to_accounts(creds: list[MonitorCredential]) -> list[Account]:
    """Convert credentials to :class:`Account`s with derived token filenames.

    Filenames come from :func:`bootstrap.derive_token_filename`, the same
    derivation ``bootstrap`` uses, so an account keeps one stable identity
    across both import paths (``robb@2amlogic.com`` → ``robb-2amlogic.token``)
    and re-importing overwrites in place instead of creating a parallel file.
    """
    return [
        Account(
            email=c.email,
            key=c.token,
            file=derive_token_filename(c.email),
            source=SOURCE_MONITOR_DB,
            index=n,
        )
        for n, c in enumerate(creds, start=1)
    ]


class MonitorImportResult:
    """Outcome of a single :func:`import_from_monitor` call."""

    def __init__(self) -> None:
        self.written: list[str] = []
        self.unchanged: list[str] = []
        self.drifted: list[str] = []
        self.pruned: list[str] = []
        self.dry_run: bool = False
        self.tokens_dir: Path | None = None
        self.index_path: Path | None = None
        self.db_path: Path | None = None
        # Each entry: {"email", "name", "file", "source"}.
        self.effective: list[dict[str, str]] = []

    def to_dict(self) -> dict[str, object]:
        return {
            "written": list(self.written),
            "unchanged": list(self.unchanged),
            "drifted": list(self.drifted),
            "pruned": list(self.pruned),
            "dry_run": self.dry_run,
            "tokens_dir": str(self.tokens_dir) if self.tokens_dir else None,
            "index_path": str(self.index_path) if self.index_path else None,
            "db_path": str(self.db_path) if self.db_path else None,
            "effective": [dict(a) for a in self.effective],
        }


def import_from_monitor(
    tokens_dir: Path,
    *,
    db_path: Path | None = None,
    monitor_dir: Path | None = None,
    force: bool = False,
    dry_run: bool = False,
    prune: bool = False,
) -> MonitorImportResult:
    """Materialize ``tokens_dir`` from claude-monitor's live credential store.

    Idempotent: a token whose on-disk fingerprint already matches the store is
    left untouched and reported ``unchanged``. A token that differs is reported
    ``drifted`` and skipped unless ``force`` — so a hand-pinned token is never
    silently replaced. **After an account roll, ``force=True`` is the flag that
    actually updates the pool**, since every rolled token legitimately differs
    from what is on disk.

    Args:
        tokens_dir: Destination pool (per-repo ``<repo>/.loom/tokens`` or the
            shared machine-level pool — the caller resolves which).
        db_path: Override the database location (tests).
        monitor_dir: Override the claude-monitor directory (tests).
        force: Overwrite tokens that differ from the store.
        dry_run: Report what would change; write nothing.
        prune: Delete ``*.token`` files for accounts the store no longer
            reports active, treating claude-monitor as authoritative for pool
            membership. Off by default so an import never removes a token an
            operator provisioned by another route.

    Returns:
        :class:`MonitorImportResult` summarising the operation.

    Raises:
        MonitorDbUnavailable: The store could not be read.
        ValueError: Two accounts derive the same token filename (they would
            clobber each other on disk).
    """
    resolved_db = db_path or monitor_db_path(monitor_dir)
    creds = read_monitor_credentials(resolved_db)

    result = MonitorImportResult()
    result.dry_run = dry_run
    result.tokens_dir = tokens_dir
    result.index_path = tokens_dir / "index.json"
    result.db_path = resolved_db

    accounts = credentials_to_accounts(creds)
    result.effective = [
        {
            "email": a.email,
            "name": a.file[: -len(".token")],
            "file": a.file,
            "source": a.source,
        }
        for a in accounts
    ]

    if not accounts:
        log_warning(
            f"No active credentials in {resolved_db}; nothing to import. "
            "Check that claude-monitor has accounts configured."
        )
        return result

    # Two distinct emails can derive the same stem (e.g. a.jones@x.com and
    # ajones@x.com). Fail loudly rather than let one clobber the other.
    seen: dict[str, Account] = {}
    for acct in accounts:
        prior = seen.get(acct.file)
        if prior is not None:
            raise ValueError(
                f"duplicate token filename: {acct.file} maps to both "
                f"{prior.email!r} and {acct.email!r}"
            )
        seen[acct.file] = acct

    outcome = materialize_accounts(
        accounts, tokens_dir, force=force, dry_run=dry_run
    )
    result.written = outcome.written
    result.unchanged = outcome.unchanged
    result.drifted = outcome.drifted

    if prune:
        result.pruned = _prune_stale_tokens(
            tokens_dir, keep={a.file for a in accounts}, dry_run=dry_run
        )

    write_index(outcome.manifest_accounts, result.index_path, dry_run=dry_run)

    log_info(
        f"import-from-monitor: written={len(result.written)} "
        f"unchanged={len(result.unchanged)} "
        f"drifted={len(result.drifted)} "
        f"pruned={len(result.pruned)} "
        f"(dry_run={dry_run})"
    )
    if result.drifted and not force:
        log_warning(
            f"{len(result.drifted)} token(s) differ from claude-monitor's store "
            "and were left as-is; re-run with --force to overwrite. After "
            "rolling accounts this is expected — --force is what applies the "
            "new tokens."
        )
    return result


def _prune_stale_tokens(
    tokens_dir: Path, *, keep: set[str], dry_run: bool
) -> list[str]:
    """Delete ``*.token`` files not in *keep*; return the filenames removed.

    Globbing ``*.token`` deliberately excludes the pool's state files
    (``.ranking``, ``.bad_tokens``, ``.failure_counts``, ``.allowlist``) and
    ``index.json``, none of which carry that suffix — pruning never disturbs
    rotation state.
    """
    if not tokens_dir.is_dir():
        return []
    pruned: list[str] = []
    for token_file in sorted(tokens_dir.glob("*.token")):
        if token_file.name in keep:
            continue
        pruned.append(token_file.name)
        if not dry_run:
            try:
                os.remove(token_file)
            except OSError as exc:
                log_warning(f"Could not prune {token_file}: {exc}")
                pruned.pop()
    if pruned:
        log_info(
            f"pruned {len(pruned)} token(s) no longer active in claude-monitor: "
            + ", ".join(pruned)
        )
    return pruned
