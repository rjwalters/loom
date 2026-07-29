"""Local-only, opt-in semantic search over sweep summaries + PR history (#4339).

Codecast-evaluation borrow-list item 2 (``docs/research/codecast-evaluation.md``,
Question 4), constrained by that evaluation's Q3 verdict: **opt-in,
off-by-default, local-only storage** — this module must never reproduce
codecast's plaintext off-host sync inside Loom.

**What is indexed (v1)**:

1. Sweep final summaries — the tail (~8 KB) of each
   ``.loom/logs/sweep-issue-<N>.log``, keyed by the issue number parsed from
   the filename.
2. Merged PR titles/bodies for the current repo, via
   ``gh pr list --state merged --json number,title,body,mergedAt,url --limit
   500`` (bounded; reading the forge is normal coordination-layer traffic,
   not off-host sync of local data).

Issue-closing-comment ingest and raw-transcript indexing (the #3726 archive)
are explicitly out of scope for v1 — see the follow-up issue filed alongside
this PR for the Tier B (vector embeddings) work.

**Storage**: a SQLite database at ``.loom/search-index/index.db``
(repo-local, gitignored). Ranking is SQLite FTS5 + BM25 — stdlib ``sqlite3``
only, zero new dependencies. Tier B (pluggable embeddings) is designed in via
the :class:`Embedder` protocol and the ``embeddings`` table, but not
implemented in v1.

**Enablement**: ``search.enabled`` in ``.loom/config.json`` (read via the
canonical :mod:`loom_tools.common.config_resolver`), with env override
``LOOM_SEARCH_ENABLED`` (via :func:`loom_tools.common.config.env_bool`).
Default OFF. Precedence is **env > config > default**, matching the
``autonomous.*`` / ``transcriptArchive`` convention elsewhere in this repo.
When disabled, ``loom-search QUERY`` degrades to a plain grep over
``.loom/logs/sweep-issue-*.log`` and indexing is a no-op.

See ``defaults/docs/semantic-search.md`` for the full usage + threat-model
doc.
"""

from __future__ import annotations

import argparse
import json
import logging
import os
import re
import sqlite3
import subprocess
import sys
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Protocol

from loom_tools.common.config import env_bool
from loom_tools.common.config_resolver import get_path, resolve_effective_config

logger = logging.getLogger(__name__)

#: Env var overriding ``search.enabled`` in ``.loom/config.json``.
SEARCH_ENABLED_ENV = "LOOM_SEARCH_ENABLED"

#: Repo-relative directory the SQLite index lives under. Must stay gitignored
#: (see ``.gitignore``) — a misconfigured/un-ignored destination refuses to
#: write, mirroring ``defaults/scripts/archive-transcripts.sh``'s
#: gitignore-or-refuse guard.
INDEX_DIR_REL = ".loom/search-index"
INDEX_DB_NAME = "index.db"

#: Repo-relative glob of sweep log files to index / grep-fallback over.
SWEEP_LOG_GLOB = ".loom/logs/sweep-issue-*.log"

#: Bytes of log tail to index per sweep (bounds cost; full transcripts are
#: explicitly out of scope for v1).
LOG_TAIL_BYTES = 8192

#: Bound on merged PRs fetched per index run.
PR_FETCH_LIMIT = 500

_SWEEP_LOG_RE = re.compile(r"sweep-issue-(\d+)\.log$")

_SCHEMA = """
CREATE TABLE IF NOT EXISTS items (
    source_type TEXT NOT NULL,
    source_id TEXT NOT NULL,
    watermark TEXT NOT NULL,
    fts_rowid INTEGER,
    PRIMARY KEY (source_type, source_id)
);

CREATE VIRTUAL TABLE IF NOT EXISTS documents USING fts5(
    source_type UNINDEXED,
    source_id UNINDEXED,
    title,
    body,
    link UNINDEXED
);

-- Tier B placeholder (#4339 follow-up): not populated in v1. Kept here so a
-- later PR can add local/remote embeddings without a schema migration dance.
CREATE TABLE IF NOT EXISTS embeddings (
    source_type TEXT NOT NULL,
    source_id TEXT NOT NULL,
    model TEXT NOT NULL,
    vector BLOB NOT NULL,
    PRIMARY KEY (source_type, source_id, model)
);
"""


class Embedder(Protocol):
    """Tier B stub (#4339 follow-up) — not implemented in v1.

    A later, separately opt-in tier can implement this to populate the
    ``embeddings`` table (local ONNX model or a remote API gated by its own
    ``search.embeddings.provider`` config, default none).
    """

    def embed(self, text: str) -> list[float]:
        """Return a vector embedding for ``text``."""
        ...


@dataclass
class SearchResult:
    """One ranked search hit."""

    source_type: str  # "sweep" or "pr"
    source_id: str
    title: str
    summary: str
    link: str
    rank: float


# --------------------------------------------------------------------------
# Enablement
# --------------------------------------------------------------------------


def is_search_enabled(repo_root: Path) -> bool:
    """Resolve whether the semantic index is enabled for ``repo_root``.

    Precedence: env (:data:`SEARCH_ENABLED_ENV`) > ``search.enabled`` in the
    resolved ``.loom/config.json`` tree > default (``False``). The env var
    wins in *both* directions when present — even ``LOOM_SEARCH_ENABLED=0``
    overrides a ``true`` config value, matching the
    ``transcriptArchive``/``autonomous.*`` convention.
    """
    if SEARCH_ENABLED_ENV in os.environ:
        return env_bool(SEARCH_ENABLED_ENV, default=False)
    effective = resolve_effective_config(repo_root)
    return bool(get_path(effective, "search.enabled", False))


# --------------------------------------------------------------------------
# gitignore-or-refuse guard (mirrors archive-transcripts.sh)
# --------------------------------------------------------------------------


class IndexDestinationNotIgnoredError(RuntimeError):
    """Raised when the index destination is tracked (or trackable) by git."""


def _git_toplevel(path: Path) -> str | None:
    probe = path
    while not probe.exists() and probe != probe.parent:
        probe = probe.parent
    try:
        result = subprocess.run(
            ["git", "-C", str(probe), "rev-parse", "--show-toplevel"],
            check=False,
            capture_output=True,
            text=True,
            timeout=10,
        )
    except (OSError, subprocess.TimeoutExpired):
        return None
    if result.returncode != 0:
        return None
    return result.stdout.strip() or None


def _is_gitignored(top: str, dest: Path) -> bool:
    try:
        result = subprocess.run(
            ["git", "-C", top, "check-ignore", "-q", str(dest) + "/"],
            check=False,
            capture_output=True,
            text=True,
            timeout=10,
        )
    except (OSError, subprocess.TimeoutExpired):
        return False
    return result.returncode == 0


def guard_index_dest_not_tracked(dest: Path) -> None:
    """Refuse to proceed if ``dest`` is inside a git repo but not gitignored.

    Mirrors ``archive-transcripts.sh``'s ``guard_dest_not_tracked``: a
    misconfigured index destination must never become a tracked artifact.
    Not inside any repo at all is fine (e.g. a destination outside the repo).
    """
    top = _git_toplevel(dest)
    if not top:
        return
    if _is_gitignored(top, dest):
        return
    raise IndexDestinationNotIgnoredError(
        f"Index destination '{dest}' is inside git repo '{top}' but is NOT "
        "gitignored. Refusing to write the search index into a tracked "
        "tree. Add the path to .gitignore, or point search.indexDir "
        "elsewhere."
    )


def index_dir(repo_root: Path) -> Path:
    """Resolve the index directory for ``repo_root`` (no side effects)."""
    return repo_root / INDEX_DIR_REL


def index_db_path(repo_root: Path) -> Path:
    return index_dir(repo_root) / INDEX_DB_NAME


# --------------------------------------------------------------------------
# Storage
# --------------------------------------------------------------------------


def _connect(db_path: Path) -> sqlite3.Connection:
    db_path.parent.mkdir(parents=True, exist_ok=True)
    conn = sqlite3.connect(str(db_path))
    conn.executescript(_SCHEMA)
    conn.commit()
    return conn


def _upsert_document(
    conn: sqlite3.Connection,
    *,
    source_type: str,
    source_id: str,
    watermark: str,
    title: str,
    body: str,
    link: str,
) -> bool:
    """Insert/refresh one document. Returns True if newly indexed/changed."""
    row = conn.execute(
        "SELECT watermark, fts_rowid FROM items WHERE source_type = ? AND source_id = ?",
        (source_type, source_id),
    ).fetchone()
    if row is not None and row[0] == watermark:
        return False  # unchanged -> skip (incremental no-op)

    if row is not None and row[1] is not None:
        conn.execute("DELETE FROM documents WHERE rowid = ?", (row[1],))

    cur = conn.execute(
        "INSERT INTO documents (source_type, source_id, title, body, link) VALUES (?, ?, ?, ?, ?)",
        (source_type, source_id, title, body, link),
    )
    fts_rowid = cur.lastrowid
    conn.execute(
        """
        INSERT INTO items (source_type, source_id, watermark, fts_rowid)
        VALUES (?, ?, ?, ?)
        ON CONFLICT(source_type, source_id)
        DO UPDATE SET watermark = excluded.watermark, fts_rowid = excluded.fts_rowid
        """,
        (source_type, source_id, watermark, fts_rowid),
    )
    return True


# --------------------------------------------------------------------------
# Sweep log ingest
# --------------------------------------------------------------------------


def _read_log_tail(path: Path, max_bytes: int = LOG_TAIL_BYTES) -> str:
    try:
        size = path.stat().st_size
    except OSError:
        return ""
    with path.open("rb") as fh:
        if size > max_bytes:
            fh.seek(size - max_bytes)
        data = fh.read()
    return data.decode("utf-8", errors="replace")


def _sweep_log_paths(repo_root: Path) -> list[Path]:
    logs_dir = repo_root / ".loom" / "logs"
    if not logs_dir.is_dir():
        return []
    return sorted(logs_dir.glob("sweep-issue-*.log"))


def index_sweep_logs(conn: sqlite3.Connection, repo_root: Path) -> int:
    """Ingest sweep-log tails. Returns the count of newly indexed/changed rows."""
    new_count = 0
    for log_path in _sweep_log_paths(repo_root):
        match = _SWEEP_LOG_RE.search(log_path.name)
        if not match:
            continue
        issue = match.group(1)
        try:
            mtime = log_path.stat().st_mtime
        except OSError:
            continue
        watermark = repr(mtime)
        tail = _read_log_tail(log_path)
        if not tail.strip():
            continue
        changed = _upsert_document(
            conn,
            source_type="sweep",
            source_id=issue,
            watermark=watermark,
            title=f"Sweep issue #{issue}",
            body=tail,
            link=str(log_path),
        )
        if changed:
            new_count += 1
    return new_count


# --------------------------------------------------------------------------
# PR ingest
# --------------------------------------------------------------------------


def _run_gh_pr_list(repo_root: Path, limit: int = PR_FETCH_LIMIT) -> list[dict[str, Any]]:
    """Fetch merged PRs for ``repo_root`` via ``gh pr list``.

    A thin, directly-monkeypatchable seam for tests (no real network calls
    in the test suite). Returns ``[]`` on any failure (missing ``gh``,
    offline, non-repo) rather than raising — PR ingest is best-effort.
    """
    try:
        result = subprocess.run(
            [
                "gh",
                "pr",
                "list",
                "--state",
                "merged",
                "--json",
                "number,title,body,mergedAt,url",
                "--limit",
                str(limit),
            ],
            cwd=str(repo_root),
            check=False,
            capture_output=True,
            text=True,
            timeout=60,
        )
    except (OSError, subprocess.TimeoutExpired) as exc:
        logger.warning("gh pr list failed (skipping PR ingest): %s", exc)
        return []
    if result.returncode != 0:
        logger.warning("gh pr list exited %s (skipping PR ingest): %s", result.returncode, result.stderr.strip())
        return []
    try:
        data = json.loads(result.stdout or "[]")
    except json.JSONDecodeError:
        return []
    return data if isinstance(data, list) else []


def index_prs(conn: sqlite3.Connection, repo_root: Path) -> int:
    """Ingest merged PR titles/bodies. Returns count of newly indexed/changed rows."""
    prs = _run_gh_pr_list(repo_root)
    new_count = 0
    for pr in prs:
        number = pr.get("number")
        if number is None:
            continue
        merged_at = pr.get("mergedAt") or ""
        title = pr.get("title") or ""
        body = pr.get("body") or ""
        link = pr.get("url") or ""
        changed = _upsert_document(
            conn,
            source_type="pr",
            source_id=str(number),
            watermark=str(merged_at),
            title=title,
            body=body,
            link=link,
        )
        if changed:
            new_count += 1
    return new_count


# --------------------------------------------------------------------------
# Indexing entrypoint
# --------------------------------------------------------------------------


def build_index(repo_root: Path) -> dict[str, int]:
    """Build/refresh the index for ``repo_root``.

    A no-op (returns zero counts, creates nothing) when search is disabled.
    Otherwise creates ``.loom/search-index/index.db`` if absent, and ingests
    sweep-log tails plus merged PRs, skipping unchanged items.
    """
    if not is_search_enabled(repo_root):
        return {"sweeps": 0, "prs": 0}

    dest = index_dir(repo_root)
    guard_index_dest_not_tracked(dest)

    conn = _connect(index_db_path(repo_root))
    try:
        sweeps = index_sweep_logs(conn, repo_root)
        prs = index_prs(conn, repo_root)
        conn.commit()
    finally:
        conn.close()
    return {"sweeps": sweeps, "prs": prs}


# --------------------------------------------------------------------------
# Query
# --------------------------------------------------------------------------


def _build_fts_query(query: str) -> str:
    """Build an FTS5 MATCH expression favoring exact-phrase matches.

    Combines a quoted phrase clause with a bareword AND-of-terms clause so
    partial (non-adjacent term) matches still surface, while
    :func:`_rank_results` sorts exact-phrase hits ahead of them.
    """
    terms = re.findall(r"\w+", query)
    if not terms:
        return '""'
    escaped_phrase = '"' + query.replace('"', '""') + '"'
    if len(terms) > 1:
        return f"{escaped_phrase} OR ({' AND '.join(terms)})"
    return escaped_phrase


def query_index(repo_root: Path, query: str, top_k: int = 10) -> list[SearchResult]:
    """Run a BM25-ranked FTS5 query over the index. Empty list if no index/hits."""
    db_path = index_db_path(repo_root)
    if not db_path.exists():
        return []

    conn = sqlite3.connect(str(db_path))
    try:
        conn.executescript(_SCHEMA)  # idempotent; guards a partial/corrupt DB
        try:
            rows = conn.execute(
                """
                SELECT source_type, source_id, title, body, link, bm25(documents) AS rank
                FROM documents
                WHERE documents MATCH ?
                ORDER BY rank
                LIMIT ?
                """,
                (_build_fts_query(query), max(top_k * 4, top_k)),
            ).fetchall()
        except sqlite3.OperationalError:
            return []
    finally:
        conn.close()

    query_lower = query.strip().lower()
    results = []
    for source_type, source_id, title, body, link, rank in rows:
        summary_line = next((line.strip() for line in body.splitlines() if line.strip()), "")
        exact_phrase = bool(query_lower) and (
            query_lower in title.lower() or query_lower in body.lower()
        )
        results.append((not exact_phrase, rank, SearchResult(
            source_type=source_type,
            source_id=source_id,
            title=title,
            summary=summary_line,
            link=link,
            rank=rank,
        )))

    results.sort(key=lambda r: (r[0], r[1]))
    return [r[2] for r in results[:top_k]]


# --------------------------------------------------------------------------
# Grep fallback (disabled path)
# --------------------------------------------------------------------------


def grep_fallback(repo_root: Path, query: str, top_k: int = 10) -> list[str]:
    """Plain case-insensitive grep over sweep logs, used when search is disabled."""
    pattern = re.compile(re.escape(query), re.IGNORECASE)
    hits: list[str] = []
    for log_path in _sweep_log_paths(repo_root):
        try:
            text = log_path.read_text(encoding="utf-8", errors="replace")
        except OSError:
            continue
        for line_no, line in enumerate(text.splitlines(), start=1):
            if pattern.search(line):
                hits.append(f"{log_path}:{line_no}: {line.strip()}")
                if len(hits) >= top_k:
                    return hits
    return hits


# --------------------------------------------------------------------------
# CLI
# --------------------------------------------------------------------------


def _default_repo_root() -> Path:
    try:
        from loom_tools.common.repo import find_repo_root

        return find_repo_root()
    except FileNotFoundError:
        return Path.cwd()


def _cmd_index(repo_root: Path) -> int:
    if not is_search_enabled(repo_root):
        print(
            "Semantic search is disabled (search.enabled is false / "
            f"{SEARCH_ENABLED_ENV} not set) — indexing is a no-op. "
            "See defaults/docs/semantic-search.md to enable."
        )
        return 0
    counts = build_index(repo_root)
    print(f"Indexed {counts['sweeps']} sweep log(s), {counts['prs']} PR(s) (new/changed).")
    return 0


def _cmd_query(repo_root: Path, query: str, top_k: int) -> int:
    if not is_search_enabled(repo_root):
        print(
            "[semantic index disabled — falling back to grep over "
            f"{SWEEP_LOG_GLOB}. Enable with search.enabled or "
            f"{SEARCH_ENABLED_ENV}=1; see defaults/docs/semantic-search.md]\n"
        )
        hits = grep_fallback(repo_root, query, top_k=top_k)
        if not hits:
            print("No matches.")
            return 0
        for hit in hits:
            print(hit)
        return 0

    results = query_index(repo_root, query, top_k=top_k)
    if not results:
        print("No matches (index may be empty — run `loom-search index` first).")
        return 0
    for res in results:
        print(f"[{res.source_type}] #{res.source_id} {res.title}")
        if res.summary:
            print(f"    {res.summary}")
        print(f"    {res.link}")
    return 0


def main(argv: list[str] | None = None) -> int:
    """CLI entry point.

    Two forms, dispatched manually (rather than via argparse subparsers) so
    a bare query string is never mistaken for a subcommand name:

    - ``loom-search index [--repo-root DIR]``
    - ``loom-search "QUERY" [--top-k N] [--repo-root DIR]``
    """
    argv = list(sys.argv[1:] if argv is None else argv)

    parser = argparse.ArgumentParser(
        prog="loom-search",
        description="Local-only, opt-in semantic search over sweep summaries + PR history.",
    )
    parser.add_argument("--repo-root", type=Path, default=None, help=argparse.SUPPRESS)
    parser.add_argument("--top-k", type=int, default=10, help="Max results to return (default: 10).")
    parser.add_argument("query", nargs="*", help="Search query, or 'index' to build/refresh the index.")

    if argv and argv[0] == "index":
        args = parser.parse_args(argv[1:])
        repo_root = (args.repo_root or _default_repo_root()).resolve()
        return _cmd_index(repo_root)

    args = parser.parse_args(argv)
    repo_root = (args.repo_root or _default_repo_root()).resolve()

    if not args.query:
        parser.print_help()
        return 1
    query = " ".join(args.query)
    return _cmd_query(repo_root, query, args.top_k)


if __name__ == "__main__":
    sys.exit(main())
