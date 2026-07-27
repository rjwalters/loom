"""Orphaned task detection and recovery (spawn-loop edition).

Detects and recovers orphaned state that occurs when:

- An issue carries the ``loom:building`` label but no spawn-loop task is
  tracking it (untracked building issue).
- A spawn-loop task entry has a stale ``last_heartbeat`` and a dead PID
  (loop crash or unresponsive tick — see #3411).

This module was ported in Phase 3.1.6 (epic #3372, tracker #3378, issue #3395).

Liveness sources (issue #3651 — SAFETY-critical fail-safe)
----------------------------------------------------------

``defaults/scripts/spawn-loop.sh`` was the historical writer of
``.loom/spawn-loop-state.json::running`` (the live-sweep roster). It was
deleted in v0.11.0, so **nothing writes that file anymore**. With no writer,
the roster is always empty, and a naive cross-check would treat *every* open
``loom:building`` issue — including live, in-flight sweeps — as an orphan and
flip it back to ``loom:issue`` (possibly cleaning its worktree) mid-build.

The fix is a **fail-safe** liveness model (:func:`gather_liveness_evidence`):

- Liveness is derived from whatever authoritative sources actually exist:
  the legacy state file (when present), a reachable ``loom-daemon`` registry
  (best-effort), and per-issue worktree-lifetime locks under
  ``.loom/locks/issue-<N>/``.
- **The invariant: absent/unreadable liveness data ⇒ treat all building
  issues as ALIVE (emit ZERO ``untracked_building`` orphans).** Absence of a
  writer is *insufficient evidence* of orphanhood, not proof of it. We fail
  toward preserving claims. Genuine-orphan cleanup is still handled by
  ``loom-clean`` (lock-based revert) and the daemon reaper.

The cross-check inputs are:

- The liveness evidence above (roster + daemon + locks).
- ``gh issue list --label loom:building`` (unchanged).

Machine-level sweep journal (issue #3953)
------------------------------------------

The daemon registry probe above (:func:`_query_daemon_live_issues`) is a
best-effort stub precisely because the daemon's sweep registry is
**in-memory** -- a restart (rate-limit kill, the print-mode ceiling, an
operator upgrade) wipes it clean, and immediately after a restart the
daemon has *nothing* to say either way. Two canary incidents hit exactly
this gap: sweeps died, their issues stayed at ``loom:building``, and this
tool found no authoritative source at all and (correctly, per the fail-safe
above) refused to touch anything -- an operator had to hand-flip labels.

``loom-daemon``'s ``SweepRegistry::dispatch`` now persists a minimal
``{repo, issue, pid, started_at}`` record to a machine-level journal at
``~/.loom/sweeps.json`` (the Rust ``sweep_journal`` module) every time it
spawns a sweep child. Unlike the in-memory registry, this **file** survives
a restart, so :func:`gather_liveness_evidence` reads it as a fourth source:

- A journal entry with a **dead** recorded PID is unconditional proof this
  claim's sweep is gone -- it is NOT added to ``live_issues``, so it falls
  through to the untracked-building check exactly like a claim the roster
  says nothing about (subject to the existing label-age grace period).
- A journal entry with a **live** recorded PID is unconditional proof of
  life -- added to ``live_issues``, so the claim is always skipped.
- The **absence** of a journal entry for a `loom:building` issue, when the
  journal file itself is present (so it IS a contributing source), is
  treated more conservatively than a dead-PID entry: the journal only
  covers *daemon-dispatched* sweeps, so "no entry" might mean a live
  **manual** ``/loom:sweep`` session the daemon was never told about. Such
  a claim needs to be stale for ``LOOM_STALE_BUILDING_HOURS`` (default 4h,
  see :func:`_get_stale_building_hours`) -- much longer than the default
  10-minute label-age grace period -- before it is treated as orphaned.

Stuck-but-running detection lives in :mod:`loom_tools.stuck_detection` (2-min
heartbeat).  This module's heartbeat threshold is intentionally higher
(5 minutes by default) because orphan recovery is post-crash cleanup, not
real-time monitoring.

Exit codes:
    0 - No orphans detected
    1 - Error occurred
    2 - Orphans detected
"""

from __future__ import annotations

import argparse
import json
import os
import pathlib
import subprocess
import sys
from dataclasses import dataclass, field
from typing import Any

from loom_tools.claim import has_valid_claim
from loom_tools.common.git import parse_porcelain_path
from loom_tools.common.github import get_repo_nwo, gh_issue_list, gh_run
from loom_tools.common.logging import log_error, log_info, log_success, log_warning
from loom_tools.common.repo import find_repo_root
from loom_tools.common.state import read_spawn_loop_state
from loom_tools.common.time_utils import elapsed_seconds, format_duration, now_utc
from loom_tools.models.spawn_loop_state import SpawnLoopState

# Default heartbeat stale threshold (5 minutes for orphan recovery).
# Intentionally higher than stuck_detection's 120s because orphan recovery
# is post-crash cleanup, not real-time monitoring.
DEFAULT_HEARTBEAT_STALE_THRESHOLD = 300

# Grace period for recently-applied loom:building labels (10 minutes).
# Issues with loom:building added less than this many seconds ago are
# assumed to be actively worked on and skipped by orphan recovery.  This
# protects newly-claimed issues and manual sweeps from being incorrectly
# recovered before claims or heartbeats are established.
DEFAULT_LABEL_GRACE_PERIOD = 600

# Deduplication window for orphan recovery comments (5 minutes).
# If an "## Orphan Recovery" comment was posted within this window,
# skip posting another to avoid duplicate noise (see issue #2658).
ORPHAN_COMMENT_DEDUP_SECONDS = 300

# Env var overriding the "no journal record at all" staleness threshold, in
# HOURS (issue #3953). Only consulted when the sweep journal is itself a
# contributing evidence source (see `gather_liveness_evidence`) -- a
# `loom:building` issue with a journal entry uses the (much shorter)
# `DEFAULT_LABEL_GRACE_PERIOD` instead, since a recorded-dead PID is
# unconditional proof, whereas "no record" only means the daemon's journal
# was never told about this claim (it may be a live manual sweep).
LOOM_STALE_BUILDING_HOURS_ENV = "LOOM_STALE_BUILDING_HOURS"
DEFAULT_STALE_BUILDING_HOURS = 4.0

# Env var overriding the machine-level sweep journal path (issue #3953).
# Mirrors `loom_daemon::sweep_journal::JOURNAL_PATH_ENV` (Rust) so both
# surfaces resolve to the same file by default.
LOOM_SWEEPS_JOURNAL_PATH_ENV = "LOOM_SWEEPS_JOURNAL_PATH"


@dataclass
class OrphanEntry:
    """A detected orphan."""

    type: str  # untracked_building | stale_heartbeat
    issue: int | None = None
    pid: int | None = None
    title: str | None = None
    reason: str = ""
    age_seconds: int | None = None

    def to_dict(self) -> dict[str, Any]:
        d: dict[str, Any] = {"type": self.type, "reason": self.reason}
        if self.issue is not None:
            d["issue"] = self.issue
        if self.pid is not None:
            d["pid"] = self.pid
        if self.title is not None:
            d["title"] = self.title
        if self.age_seconds is not None:
            d["age_seconds"] = self.age_seconds
        return d


@dataclass
class RecoveryEntry:
    """A recovery action taken."""

    action: str  # reset_issue_label | cleanup_stale_worktree
    issue: int | None = None
    pid: int | None = None
    reason: str = ""

    def to_dict(self) -> dict[str, Any]:
        d: dict[str, Any] = {"action": self.action, "reason": self.reason}
        if self.issue is not None:
            d["issue"] = self.issue
        if self.pid is not None:
            d["pid"] = self.pid
        return d


@dataclass
class WatchedEntry:
    """A ``loom:building`` issue that was inspected but is not (yet) flagged
    as orphaned because it hasn't cleared the applicable staleness threshold.

    Recorded by :func:`check_untracked_building` every time the staleness
    gate skips a candidate -- so an operator running the tool, even *without*
    ``--verbose``, can see exactly which claims were seen and why they were
    excluded, instead of the exclusion being silent (issue #3975: two of
    three genuinely-dead claims were silently skipped with no visible trace
    in the default, non-verbose output).
    """

    issue: int
    title: str | None
    reason: str  # journal_pid_dead | no_spawn_loop_entry | no_journal_record_stale
    age_seconds: int | None
    threshold_seconds: float

    def to_dict(self) -> dict[str, Any]:
        d: dict[str, Any] = {
            "issue": self.issue,
            "reason": self.reason,
            "threshold_seconds": self.threshold_seconds,
        }
        if self.title is not None:
            d["title"] = self.title
        if self.age_seconds is not None:
            d["age_seconds"] = self.age_seconds
        return d


@dataclass
class OrphanRecoveryResult:
    """Result of orphan detection and recovery."""

    orphaned: list[OrphanEntry] = field(default_factory=list)
    recovered: list[RecoveryEntry] = field(default_factory=list)
    watched: list[WatchedEntry] = field(default_factory=list)
    recover_mode: bool = False

    @property
    def total_orphaned(self) -> int:
        return len(self.orphaned)

    @property
    def total_recovered(self) -> int:
        return len(self.recovered)

    @property
    def total_watched(self) -> int:
        return len(self.watched)

    def to_dict(self) -> dict[str, Any]:
        return {
            "orphaned": [o.to_dict() for o in self.orphaned],
            "recovered": [r.to_dict() for r in self.recovered],
            "watched": [w.to_dict() for w in self.watched],
            "total_orphaned": self.total_orphaned,
            "total_recovered": self.total_recovered,
            "total_watched": self.total_watched,
            "recover_mode": self.recover_mode,
        }


def _get_heartbeat_stale_threshold() -> int:
    """Get heartbeat stale threshold from env var or default."""
    env_val = os.environ.get("LOOM_HEARTBEAT_STALE_THRESHOLD")
    if env_val is not None:
        try:
            return int(env_val)
        except ValueError:
            pass
    return DEFAULT_HEARTBEAT_STALE_THRESHOLD


def _get_label_grace_period() -> int:
    """Get label grace period from env var or default."""
    env_val = os.environ.get("LOOM_LABEL_GRACE_PERIOD")
    if env_val is not None:
        try:
            return int(env_val)
        except ValueError:
            pass
    return DEFAULT_LABEL_GRACE_PERIOD


def _get_stale_building_hours() -> float:
    """Get the no-journal-record staleness threshold (hours) from env or default.

    A non-positive or unparseable override falls back to
    :data:`DEFAULT_STALE_BUILDING_HOURS` (mirrors
    ``loom_daemon::claim_reconciliation::resolve_stale_hours`` in Rust).
    """
    env_val = os.environ.get(LOOM_STALE_BUILDING_HOURS_ENV)
    if env_val is not None:
        try:
            hours = float(env_val)
            if hours > 0:
                return hours
        except ValueError:
            pass
    return DEFAULT_STALE_BUILDING_HOURS


def _pid_alive(pid: int) -> bool:
    """Return True if *pid* is a live process.

    Uses ``os.kill(pid, 0)`` which raises ``ProcessLookupError`` for dead
    PIDs and ``PermissionError`` for live PIDs we don't own (treated as
    alive — better to skip recovery than tear down somebody else's work).
    """
    if pid <= 0:
        return False
    try:
        os.kill(pid, 0)
    except ProcessLookupError:
        return False
    except PermissionError:
        return True
    except OSError:
        # Any other OSError (rare) — be conservative: assume alive.
        return True
    return True


def _get_building_label_age(issue: int) -> int | None:
    """Return seconds since the ``loom:building`` label was applied to *issue*.

    Queries the GitHub API for issue timeline events to find the most recent
    ``labeled`` event for ``loom:building``.  Returns ``None`` if the label
    event cannot be determined (API failure, no events, etc.).
    """
    nwo = get_repo_nwo()
    if not nwo:
        log_warning(
            f"Cannot determine label age for #{issue}: "
            "repo NWO not available"
        )
        return None

    try:
        result = gh_run(
            [
                "api",
                f"repos/{nwo}/issues/{issue}/events",
                "--jq",
                '[.[] | select(.event == "labeled" and .label.name == "loom:building")] | last | .created_at',
            ],
            check=False,
        )
    except Exception as exc:
        log_warning(
            f"Cannot determine label age for #{issue}: "
            f"API call failed ({exc})"
        )
        return None

    if result.returncode != 0:
        log_warning(
            f"Cannot determine label age for #{issue}: "
            f"gh returned exit code {result.returncode}"
        )
        return None

    timestamp = result.stdout.strip().strip('"')
    if not timestamp or timestamp == "null":
        log_warning(
            f"Cannot determine label age for #{issue}: "
            "no loom:building label events found"
        )
        return None

    try:
        return elapsed_seconds(timestamp)
    except (ValueError, OverflowError):
        log_warning(
            f"Cannot determine label age for #{issue}: "
            f"unparseable timestamp '{timestamp}'"
        )
        return None


@dataclass
class LivenessEvidence:
    """Authoritative evidence of which sweeps are currently alive.

    ``available`` is True when at least one authoritative liveness *source*
    exists (a present state file, a reachable daemon registry, the sweep
    journal, or one or more ``.loom/locks/issue-<N>/`` locks). When it is
    False we have **no** evidence either way, and the fail-safe (issue #3651)
    is to emit zero orphans.

    ``live_issues`` is the union of issue numbers known to be alive across all
    available sources. ``sources`` records which sources contributed, for
    logging/observability.

    ``journal_present``/``journal_issues`` (issue #3953) carry extra detail
    specifically about the machine-level sweep journal: whether the journal
    file exists at all, and which issue numbers have *any* entry for this repo
    (dead or alive -- a subset of ``live_issues`` is the alive ones).
    :func:`check_untracked_building` uses these to pick a stricter staleness
    threshold for a claim the journal has *never heard of* than for one it can
    prove is dead.
    """

    available: bool = False
    live_issues: set[int] = field(default_factory=set)
    sources: list[str] = field(default_factory=list)
    journal_present: bool = False
    journal_issues: set[int] = field(default_factory=set)


def _locked_issue_numbers(repo_root: pathlib.Path) -> set[int]:
    """Issue numbers with a live ``.loom/locks/issue-<N>/`` worktree lock.

    These lock dirs are the ``mkdir``-atomic claim locks whose lifetime tracks
    an in-flight worktree/sweep. A present lock is strong evidence the issue is
    being actively worked. Missing/unreadable locks-dir yields an empty set.
    """
    locks_dir = repo_root / ".loom" / "locks"
    if not locks_dir.is_dir():
        return set()
    out: set[int] = set()
    try:
        entries = list(locks_dir.iterdir())
    except OSError:
        return set()
    for entry in entries:
        if not entry.is_dir():
            continue
        name = entry.name
        if not name.startswith("issue-"):
            # Ignore non-issue locks (e.g. the repo-global ``worktree-add``
            # lock created transiently by worktree.sh).
            continue
        try:
            out.add(int(name[len("issue-") :]))
        except ValueError:
            continue
    return out


def _query_daemon_live_issues(repo_root: pathlib.Path) -> set[int] | None:
    """Best-effort query of the ``loom-daemon`` sweep registry.

    Returns the set of issue numbers with a live sweep in the daemon registry,
    or ``None`` when the daemon is not reachable / no Python client is wired up
    (the common case). The daemon is an **optional** secondary source — it is
    never hard-required (issue #3651). Returning ``None`` means "daemon is not
    a source right now", which is distinct from "daemon says nothing is live"
    (an empty set).

    A future follow-up may implement the IPC/MCP ``list_sweeps`` round-trip
    here; today there is no Python IPC client, so this is a safe no-op stub.
    """
    return None


@dataclass
class _JournalEntry:
    """One entry from the machine-level sweep journal (issue #3953)."""

    repo: str
    issue: int
    pid: int


def _default_journal_path() -> pathlib.Path:
    """Resolve the sweep journal path: env override, else ``~/.loom/sweeps.json``.

    Mirrors ``loom_daemon::sweep_journal::default_journal_path`` (Rust) so
    both surfaces read the exact same file by default.
    """
    env_val = os.environ.get(LOOM_SWEEPS_JOURNAL_PATH_ENV)
    if env_val:
        return pathlib.Path(env_val)
    return pathlib.Path.home() / ".loom" / "sweeps.json"


def _load_journal_entries(path: pathlib.Path) -> list[_JournalEntry]:
    """Load and parse the sweep journal. Tolerant: missing/corrupt -> ``[]``.

    Never raises -- a garbled or absent journal must never crash orphan
    recovery; it degrades to "no journal evidence" (mirrors the Rust-side
    ``sweep_journal::load``'s tolerant-corrupt-file behavior, issue #3651's
    fail-safe philosophy applied to this new source).
    """
    try:
        raw = path.read_text()
    except OSError:
        return []
    if not raw.strip():
        return []
    try:
        data = json.loads(raw)
    except json.JSONDecodeError:
        log_warning(f"sweep journal at {path} is corrupt (invalid JSON) — treating as empty")
        return []
    if not isinstance(data, dict):
        return []
    entries: list[_JournalEntry] = []
    for row in data.get("entries", []) or []:
        try:
            entries.append(
                _JournalEntry(
                    repo=str(row["repo"]),
                    issue=int(row["issue"]),
                    pid=int(row["pid"]),
                )
            )
        except (KeyError, TypeError, ValueError):
            # Skip a malformed row rather than discarding the whole journal.
            continue
    return entries


def _journal_repo_matches(entry_repo: str, repo_root: pathlib.Path) -> bool:
    """Whether a journal entry's ``repo`` string identifies ``repo_root``.

    The daemon stamps ``repo`` as ``workspace_root.display().to_string()``
    (Rust) — an exact string match is the fast, common-case path (this repo
    checkout and the daemon's workspace_root agree byte-for-byte). Falls back
    to a resolved-path comparison so an equivalent-but-differently-formatted
    path (e.g. a trailing slash) still matches.
    """
    if entry_repo == str(repo_root):
        return True
    try:
        return pathlib.Path(entry_repo).resolve() == repo_root.resolve()
    except OSError:
        return False


def gather_liveness_evidence(
    spawn_loop_state: SpawnLoopState,
    repo_root: pathlib.Path | None,
) -> LivenessEvidence:
    """Collect authoritative liveness evidence from all available sources.

    Sources, unioned:

    1. ``.loom/spawn-loop-state.json::running`` — legacy roster. Present only
       when some writer exists (essentially never after v0.11.0).
    2. ``loom-daemon`` registry via :func:`_query_daemon_live_issues` — optional,
       best-effort; ``None`` means "not a source".
    3. ``.loom/locks/issue-<N>/`` — per-issue worktree-lifetime locks.
    4. The machine-level sweep journal (``~/.loom/sweeps.json``, issue #3953)
       — survives a daemon restart, unlike source 2. A dead recorded PID is
       proof of death (not added to ``live_issues``); a live recorded PID is
       proof of life (added). See :attr:`LivenessEvidence.journal_present` /
       :attr:`LivenessEvidence.journal_issues` for the "no record at all" case.

    ``available`` is True iff at least one of these sources is actually present.
    When it is False the caller MUST NOT flag any building issue as orphaned.
    """
    live: set[int] = set()
    sources: list[str] = []
    journal_present = False
    journal_issues: set[int] = set()

    if spawn_loop_state.present:
        sources.append("spawn-loop-state.json")
        live |= {task.issue for task in spawn_loop_state.running if task.issue}

    if repo_root is not None:
        daemon_issues = _query_daemon_live_issues(repo_root)
        if daemon_issues is not None:
            sources.append("loom-daemon")
            live |= daemon_issues

        locked = _locked_issue_numbers(repo_root)
        if locked:
            sources.append(".loom/locks")
            live |= locked

        journal_path = _default_journal_path()
        if journal_path.exists():
            journal_present = True
            sources.append("sweep-journal")
            for entry in _load_journal_entries(journal_path):
                if not _journal_repo_matches(entry.repo, repo_root):
                    continue
                journal_issues.add(entry.issue)
                if _pid_alive(entry.pid):
                    live.add(entry.issue)

    return LivenessEvidence(
        available=bool(sources),
        live_issues=live,
        sources=sources,
        journal_present=journal_present,
        journal_issues=journal_issues,
    )


def check_untracked_building(
    evidence: LivenessEvidence,
    result: OrphanRecoveryResult,
    *,
    repo_root: pathlib.Path | None = None,
    label_grace_period: int = DEFAULT_LABEL_GRACE_PERIOD,
    verbose: bool = False,
) -> None:
    """Find ``loom:building`` issues that no live sweep is tracking.

    Cross-references ``gh issue list --label loom:building`` against the live
    issue set in *evidence* (roster + daemon + ``.loom/locks/`` + the sweep
    journal).  Issues with a valid file-based claim are skipped (CLI-driven
    sweeps may hold a claim without a lock).

    **Staleness threshold (issue #3953):** a claim with a **dead** sweep-journal
    entry (the strongest possible evidence — a recorded PID that is provably
    gone) uses the standard, short *label_grace_period*. A claim with **no**
    journal entry at all — when the journal is itself a contributing source —
    uses the much longer ``LOOM_STALE_BUILDING_HOURS`` threshold instead,
    since "no entry" might mean a live manual ``/loom:sweep`` the daemon was
    never told about, not a dead one. When the journal is not a source at all
    (e.g. it has never been written on this machine), every claim falls back
    to *label_grace_period* — byte-for-byte pre-#3953 behavior.

    **Fail-safe (issue #3651):** if *evidence* reports no authoritative liveness
    source is available, this emits **zero** orphans — absence of a writer is
    not proof of orphanhood, and tearing down a live sweep is the worst
    possible outcome.
    """
    # SAFETY GATE: with no authoritative liveness source we cannot distinguish
    # a live sweep from an orphan, so we treat every building issue as ALIVE.
    if not evidence.available:
        log_warning(
            "No authoritative liveness source available (no "
            "spawn-loop-state.json, no reachable loom-daemon registry, no "
            ".loom/locks/issue-<N>/ locks) — refusing to flag any loom:building "
            "issue as orphaned (fail-safe: absent liveness data means treat "
            "claims as ALIVE, not orphaned). See issue #3651."
        )
        return

    try:
        building_issues = gh_issue_list(labels=["loom:building"])
    except Exception as exc:
        log_error(f"Failed to list loom:building issues: {exc}")
        return

    if not building_issues:
        if verbose:
            log_info("No loom:building issues found")
        return

    tracked_issues: set[int] = evidence.live_issues

    for issue_data in building_issues:
        issue_num = issue_data.get("number", 0)
        issue_title = issue_data.get("title", "")

        if verbose:
            log_info(f"Checking issue #{issue_num}")

        if issue_num in tracked_issues:
            if verbose:
                log_info(
                    f"  OK: #{issue_num} tracked by live source "
                    f"({', '.join(evidence.sources)})"
                )
            continue

        # File-based claim check (primary protection, no API call).
        # A CLI-driven sweep may hold a valid claim without a spawn-loop
        # entry, e.g. during a long builder subprocess.
        if repo_root is not None:
            if has_valid_claim(repo_root, issue_num):
                if verbose:
                    log_info(
                        f"  SKIPPED: #{issue_num} has a valid file-based claim"
                    )
                continue
            elif verbose:
                log_info(
                    f"  No valid file-based claim for #{issue_num}"
                )
        else:
            log_warning(
                f"  repo_root is None — skipping file-based claim check "
                f"for #{issue_num} (this may cause false positives)"
            )

        # Select the staleness threshold + reason (#3953): a journal entry for
        # this issue is unconditional dead-PID proof (it would have been in
        # `tracked_issues`/skipped above if the recorded PID were alive), so
        # the standard short grace period applies. No entry at all — only
        # when the journal is itself a contributing source — requires the
        # much longer no-record threshold instead (see docstring).
        has_journal_entry = issue_num in evidence.journal_issues
        if evidence.journal_present and not has_journal_entry:
            threshold_seconds = _get_stale_building_hours() * 3600
            reason = "no_journal_record_stale"
        else:
            threshold_seconds = float(label_grace_period)
            reason = "journal_pid_dead" if has_journal_entry else "no_spawn_loop_entry"

        # Staleness gate: skip issues that haven't been in loom:building long
        # enough yet under the selected threshold. Protects newly-claimed
        # issues from premature orphan recovery before claims/heartbeats are
        # established (short threshold) or before a no-journal-record claim
        # has had a fair chance to prove itself alive (long threshold).
        if threshold_seconds > 0:
            label_age = _get_building_label_age(issue_num)
            if label_age is not None and label_age < threshold_seconds:
                if verbose:
                    log_info(
                        f"  SKIPPED: #{issue_num} label loom:building "
                        f"applied {label_age}s ago (threshold: "
                        f"{threshold_seconds:.0f}s, reason if stale: {reason})"
                    )
                # Always record the skip -- issue #3975: this used to be
                # visible only with --verbose, so an operator running the
                # default (non-verbose) invocation had no trace that a claim
                # was seen and excluded. `result.watched` makes every
                # staleness-gated skip visible in both human and JSON output.
                result.watched.append(
                    WatchedEntry(
                        issue=issue_num,
                        title=issue_title,
                        reason=reason,
                        age_seconds=label_age,
                        threshold_seconds=threshold_seconds,
                    )
                )
                continue

        if verbose:
            log_warning(
                f"  ORPHANED: #{issue_num} has loom:building "
                f"but no active spawn-loop task (reason: {reason})"
            )
        result.orphaned.append(
            OrphanEntry(
                type="untracked_building",
                issue=issue_num,
                title=issue_title,
                reason=reason,
            )
        )


def check_stale_heartbeats(
    spawn_loop_state: SpawnLoopState,
    result: OrphanRecoveryResult,
    *,
    heartbeat_threshold: int = DEFAULT_HEARTBEAT_STALE_THRESHOLD,
    verbose: bool = False,
) -> None:
    """Flag spawn-loop tasks whose heartbeat is stale and PID is dead.

    The spawn loop refreshes ``last_heartbeat`` every tick for every live
    child PID (#3411).  A stale heartbeat therefore implies either:

    - The spawn loop itself crashed or hung (no ticks happening), or
    - The PID is gone but the state entry was not reaped (shouldn't happen,
      but defensive).

    Either way the entry is orphaned and should be cleaned up.  If the PID
    is still alive we skip the entry — the spawn loop may have just been
    paused / SIGSTOPped, and tearing down active work is the worst possible
    outcome.
    """
    for task in spawn_loop_state.running:
        if verbose:
            log_info(
                f"Checking task: issue=#{task.issue}, pid={task.pid}, "
                f"heartbeat={task.last_heartbeat or '<missing>'}"
            )

        hb = task.last_heartbeat
        if not hb:
            # No heartbeat is expected for pre-#3411 state files; nothing
            # to flag.  (stuck_detection.py handles missing-heartbeat
            # diagnostics on a faster cadence.)
            if verbose:
                log_info(
                    f"  Skipping issue #{task.issue}: no heartbeat field"
                )
            continue

        try:
            age = elapsed_seconds(hb)
        except (ValueError, OverflowError):
            if verbose:
                log_info(
                    f"  Skipping issue #{task.issue}: "
                    f"unparseable heartbeat '{hb}'"
                )
            continue

        if age <= heartbeat_threshold:
            if verbose:
                log_info(
                    f"  OK: issue #{task.issue} heartbeat {age}s old "
                    f"(threshold: {heartbeat_threshold}s)"
                )
            continue

        # Stale heartbeat — but skip if PID is still alive (loop paused,
        # not crashed).  Tearing down an active sweep is the worst case.
        if _pid_alive(task.pid):
            if verbose:
                log_info(
                    f"  Skipping issue #{task.issue}: heartbeat stale "
                    f"({age}s) but pid {task.pid} is alive"
                )
            continue

        if verbose:
            log_warning(
                f"  ORPHANED: issue #{task.issue} heartbeat "
                f"{age // 60}m old, pid {task.pid} dead"
            )
        result.orphaned.append(
            OrphanEntry(
                type="stale_heartbeat",
                issue=task.issue if task.issue else None,
                pid=task.pid,
                age_seconds=age,
                reason="heartbeat_stale",
            )
        )


def _cleanup_stale_worktree(repo_root: pathlib.Path, issue: int) -> bool:
    """Remove a stale worktree and its local/remote branches for an issue.

    A worktree is considered stale when it has zero commits ahead of main
    and no meaningful uncommitted changes (build artifacts are ignored).

    Returns True if cleanup was performed, False otherwise.
    """
    from loom_tools.common.paths import LoomPaths

    worktree_path = LoomPaths(repo_root).worktree_path(issue)
    if not worktree_path.is_dir():
        return False

    # Check for commits ahead of main
    log_result = subprocess.run(
        ["git", "-C", str(worktree_path), "log", "--oneline", "origin/main..HEAD"],
        capture_output=True,
        text=True,
        check=False,
    )
    if log_result.returncode != 0:
        log_warning(
            f"Cannot determine commit status for worktree issue-{issue}, "
            "skipping cleanup"
        )
        return False

    if log_result.stdout.strip():
        log_info(
            f"Worktree issue-{issue} has commits ahead of main, skipping cleanup"
        )
        return False

    # Check for meaningful uncommitted changes (ignore build artifacts)
    status_result = subprocess.run(
        ["git", "-C", str(worktree_path), "status", "--porcelain"],
        capture_output=True,
        text=True,
        check=False,
    )
    if status_result.returncode != 0:
        log_warning(
            f"Cannot determine status for worktree issue-{issue}, skipping cleanup"
        )
        return False

    build_artifact_patterns = (
        "node_modules",
        "pnpm-lock.yaml",
        ".venv",
        "target/",
        "Cargo.lock",
        "coverage/",
        ".loom-checkpoint",
        ".loom-in-use",
    )
    for line in status_result.stdout.strip().splitlines():
        filepath = parse_porcelain_path(line)
        if not any(pat in filepath for pat in build_artifact_patterns):
            log_info(
                f"Worktree issue-{issue} has meaningful uncommitted changes, "
                "skipping cleanup"
            )
            return False

    # Get branch name before removal
    branch_result = subprocess.run(
        ["git", "-C", str(worktree_path), "rev-parse", "--abbrev-ref", "HEAD"],
        capture_output=True,
        text=True,
        check=False,
    )
    branch = branch_result.stdout.strip() if branch_result.returncode == 0 else ""

    # Remove worktree
    remove_result = subprocess.run(
        ["git", "worktree", "remove", str(worktree_path), "--force"],
        cwd=str(repo_root),
        capture_output=True,
        text=True,
        check=False,
    )
    if remove_result.returncode != 0:
        log_warning(
            f"Failed to remove worktree issue-{issue}: "
            f"{remove_result.stderr.strip()}"
        )
        return False

    # Delete local branch (best-effort)
    if branch and branch != "main":
        subprocess.run(
            ["git", "-C", str(repo_root), "branch", "-D", branch],
            capture_output=True,
            check=False,
        )

    # Delete remote branch (best-effort)
    if branch and branch != "main":
        subprocess.run(
            ["git", "-C", str(repo_root), "push", "origin", "--delete", branch],
            capture_output=True,
            check=False,
        )

    log_info(
        f"Cleaned up stale worktree issue-{issue}"
        + (f" (branch {branch})" if branch else "")
    )
    return True


def _has_recent_orphan_comment(
    issue: int, dedup_seconds: int = ORPHAN_COMMENT_DEDUP_SECONDS
) -> bool:
    """Check if an orphan recovery comment was posted recently on this issue.

    Returns True if a comment starting with ``## Orphan Recovery`` was posted
    within *dedup_seconds*, preventing duplicate comments from concurrent or
    rapid-succession recovery runs (see issue #2658).
    """
    try:
        result = gh_run(
            [
                "issue", "view", str(issue),
                "--json", "comments",
                "--jq",
                '.comments | map(select(.body | startswith("## Orphan Recovery"))) '
                '| sort_by(.createdAt) | last | .createdAt // empty',
            ],
            check=False,
        )
        if result.returncode != 0 or not result.stdout.strip():
            return False
        last_ts = result.stdout.strip()
        age = elapsed_seconds(last_ts)
        if age < dedup_seconds:
            log_info(
                f"Orphan recovery comment already posted on #{issue} "
                f"{age}s ago (dedup window: {dedup_seconds}s)"
            )
            return True
    except Exception:
        # If we can't check, allow the comment to be posted
        pass
    return False


def recover_issue(
    issue: int,
    reason: str,
    result: OrphanRecoveryResult,
    *,
    repo_root: pathlib.Path | None = None,
    label_grace_period: int = DEFAULT_LABEL_GRACE_PERIOD,
) -> None:
    """Recovery action: Reset issue labels from ``loom:building`` to ``loom:issue``.

    If ``repo_root`` is provided and a valid file-based claim exists for the
    issue, recovery is skipped to avoid disrupting an active sweep.

    A label-age grace period provides defense-in-depth: if the
    ``loom:building`` label was applied recently (within *label_grace_period*
    seconds), recovery is skipped regardless of claim state.
    """
    # Defense-in-depth: skip recovery if the label was recently applied.
    if label_grace_period > 0:
        label_age = _get_building_label_age(issue)
        if label_age is not None and label_age < label_grace_period:
            log_warning(
                f"Skipping recovery for issue #{issue}: "
                f"loom:building label applied {label_age}s ago "
                f"(grace period: {label_grace_period}s)"
            )
            return

    if repo_root is not None and has_valid_claim(repo_root, issue):
        log_warning(
            f"Skipping recovery for issue #{issue}: valid file-based claim exists"
        )
        return

    if repo_root is None:
        log_warning(
            f"repo_root is None for issue #{issue} recovery — "
            "cannot verify claims"
        )

    # Clean up stale worktree if present (0 commits ahead, no meaningful changes)
    worktree_cleaned = False
    if repo_root is not None:
        worktree_cleaned = _cleanup_stale_worktree(repo_root, issue)
        if worktree_cleaned:
            result.recovered.append(
                RecoveryEntry(
                    action="cleanup_stale_worktree",
                    issue=issue,
                    reason=reason,
                )
            )

    try:
        gh_run([
            "issue", "edit", str(issue),
            "--remove-label", "loom:building",
            "--add-label", "loom:issue",
        ])
    except Exception as exc:
        log_warning(f"Failed to update labels for issue #{issue}: {exc}")
        return

    ts = now_utc().strftime("%Y-%m-%dT%H:%M:%SZ")
    actions = [
        "- Removed `loom:building` label",
        "- Added `loom:issue` label to return to ready queue",
    ]
    if worktree_cleaned:
        actions.append("- Cleaned up stale worktree and branches")

    comment = (
        "## Orphan Recovery\n\n"
        "This issue was automatically recovered from an orphaned state.\n\n"
        f"**Reason**: {reason}\n"
        "**What happened**:\n"
        "- The spawn-loop task that was working on this issue "
        "crashed or was terminated\n"
        "- The issue was left in `loom:building` state with no active worker\n\n"
        "**Action taken**:\n"
        + "\n".join(actions)
        + "\n\n"
        "This issue is now available for a new sweep to pick up.\n\n"
        "---\n"
        f"*Recovered by loom-recover-orphans at {ts}*"
    )

    if not _has_recent_orphan_comment(issue):
        try:
            gh_run(["issue", "comment", str(issue), "--body", comment])
        except Exception as exc:
            log_warning(f"Failed to add comment to issue #{issue}: {exc}")

    result.recovered.append(
        RecoveryEntry(
            action="reset_issue_label",
            issue=issue,
            reason=reason,
        )
    )

    log_success(f"Recovered issue #{issue}")


def run_orphan_recovery(
    repo_root: pathlib.Path,
    *,
    recover: bool = False,
    verbose: bool = False,
) -> OrphanRecoveryResult:
    """Run all orphan detection phases and optionally recover.

    Gathers authoritative liveness evidence (:func:`gather_liveness_evidence`)
    and cross-checks it against ``gh issue list --label loom:building``. When no
    liveness source is available the untracked-building check fails safe and
    emits zero orphans (issue #3651).

    Known invocation path:

    - CLI: ``./.loom/scripts/recover-orphaned-shepherds.sh [--recover]``
      (script is a thin stub delegating here), also reachable from
      ``/loom:sweep all`` aggressive mode.

    Returns an :class:`OrphanRecoveryResult` with all detected orphans and
    any recovery actions taken.
    """
    result = OrphanRecoveryResult(recover_mode=recover)
    heartbeat_threshold = _get_heartbeat_stale_threshold()
    label_grace_period = _get_label_grace_period()

    spawn_loop_state = read_spawn_loop_state(repo_root)

    # Gather authoritative liveness evidence (roster + daemon + locks + the
    # #3953 sweep journal). When no source is available the untracked-building
    # cross-check fails safe and emits zero orphans — see issue #3651.
    evidence = gather_liveness_evidence(spawn_loop_state, repo_root)

    if verbose:
        if evidence.available:
            log_info(
                "Liveness sources: "
                f"{', '.join(evidence.sources)} "
                f"(live issues: {sorted(evidence.live_issues) or 'none'})"
            )
        else:
            log_info(
                "No authoritative liveness source found — untracked-building "
                "cross-check will fail safe (emit zero orphans). See #3651."
            )

    # Phase A: cross-check loom:building issues against the live issue set.
    check_untracked_building(
        evidence,
        result,
        repo_root=repo_root,
        label_grace_period=label_grace_period,
        verbose=verbose,
    )

    # Phase B: flag spawn-loop tasks with stale heartbeats whose PID is dead.
    check_stale_heartbeats(
        spawn_loop_state,
        result,
        heartbeat_threshold=heartbeat_threshold,
        verbose=verbose,
    )

    if not recover:
        return result

    # Perform recovery for detected orphans.  Both orphan types resolve to
    # the same recovery action: flip the issue label back to loom:issue so
    # a new sweep can pick it up.
    for orphan in list(result.orphaned):
        if orphan.issue:
            recover_issue(
                orphan.issue,
                orphan.reason,
                result,
                repo_root=repo_root,
                label_grace_period=label_grace_period,
            )

    return result


def format_result_json(result: OrphanRecoveryResult) -> str:
    """Format result as JSON string."""
    return json.dumps(result.to_dict(), indent=2)


def format_result_human(result: OrphanRecoveryResult) -> str:
    """Format result as human-readable text."""
    lines: list[str] = []

    if result.total_orphaned == 0:
        lines.append("No orphaned tasks found")
    else:
        lines.append(f"Found {result.total_orphaned} orphaned task(s)")
        lines.append("")

        for orphan in result.orphaned:
            if orphan.type == "untracked_building":
                lines.append(
                    f"  [{orphan.type}] #{orphan.issue}: "
                    f"{orphan.title or 'no title'} "
                    f"-- no active spawn-loop task"
                )
            elif orphan.type == "stale_heartbeat":
                age_str = format_duration(orphan.age_seconds or 0)
                lines.append(
                    f"  [{orphan.type}] issue #{orphan.issue} "
                    f"(pid {orphan.pid}): heartbeat stale ({age_str})"
                )

        if result.recover_mode:
            lines.append("")
            lines.append(f"Recovered {result.total_recovered} item(s)")
        else:
            lines.append("")
            lines.append("Run with --recover to fix these issues")

    # Always surface watched (seen-but-not-yet-stale) claims, even in the
    # zero-orphan case and without --verbose -- issue #3975: a claim excluded
    # by the staleness gate must never be silent.
    if result.watched:
        lines.append("")
        lines.append(
            f"{result.total_watched} claim(s) seen but not yet stale enough to reclaim:"
        )
        for w in result.watched:
            age_str = format_duration(w.age_seconds or 0)
            threshold_str = format_duration(int(w.threshold_seconds))
            remaining = max(0, int(w.threshold_seconds) - (w.age_seconds or 0))
            lines.append(
                f"  [watched] #{w.issue}: {w.title or 'no title'} -- "
                f"skipped ({w.reason}): label age {age_str}, "
                f"threshold {threshold_str}, eligible in {format_duration(remaining)}"
            )

    return "\n".join(lines)


def main(argv: list[str] | None = None) -> int:
    """Main entry point for orphan recovery CLI."""
    parser = argparse.ArgumentParser(
        description="Detect and recover orphaned spawn-loop task state",
        formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog="""\
Exit codes:
    0 - No orphans detected
    1 - Error occurred
    2 - Orphans detected

Orphan types:
    untracked_building  - Issue has loom:building but no spawn-loop task
    stale_heartbeat     - Spawn-loop task heartbeat is stale and pid is dead

Watched claims (issue #3975):
    A loom:building issue the staleness gate skipped (label age below the
    applicable threshold) is never silently dropped -- it is always listed
    as a "watched" entry, in both human and --json output, with the reason
    and the remaining time before it becomes eligible. This is distinct
    from an orphan: a watched claim MAY still be alive.

Recovery actions:
    reset_issue_label       - Swap loom:building -> loom:issue on issue
    cleanup_stale_worktree  - Remove stale worktree + branches (0 commits, no changes)

Liveness sources (fail-safe, #3651):
    .loom/spawn-loop-state.json           - Legacy roster (no writer post-v0.11.0)
    loom-daemon registry                  - Optional, best-effort
    .loom/locks/issue-<N>/                - Per-issue worktree-lifetime locks
    ~/.loom/sweeps.json                   - Machine-level sweep journal (#3953),
                                             survives a daemon restart
    gh issue list --label loom:building   - Forge label cross-check
  With NO authoritative liveness source, zero untracked_building orphans
  are emitted (absent evidence => treat claims as ALIVE, not orphaned).

Environment variables:
    LOOM_HEARTBEAT_STALE_THRESHOLD  Seconds before heartbeat is stale (default: 300)
    LOOM_LABEL_GRACE_PERIOD         Seconds to skip recently-labeled issues (default: 600)
    LOOM_STALE_BUILDING_HOURS       Hours before a claim with NO sweep-journal entry
                                    is treated as orphaned (default: 4.0)
    LOOM_SWEEPS_JOURNAL_PATH        Override the sweep journal path (default:
                                    ~/.loom/sweeps.json)
""",
    )

    parser.add_argument(
        "--recover",
        action="store_true",
        help="Actually perform recovery (default is dry-run)",
    )
    parser.add_argument(
        "--json",
        action="store_true",
        help="Output JSON for programmatic use",
    )
    parser.add_argument(
        "--verbose", "-v",
        action="store_true",
        help="Show detailed progress",
    )

    args = parser.parse_args(argv)

    try:
        repo_root = find_repo_root()
    except FileNotFoundError:
        log_error("Not in a git repository with .loom directory")
        return 1

    if not args.json:
        log_info("Orphaned Spawn-Loop Task Detection & Recovery")
        if not args.recover:
            log_info("DRY RUN - No changes will be made")
            log_info("Use --recover to actually perform recovery")

    try:
        result = run_orphan_recovery(
            repo_root,
            recover=args.recover,
            verbose=args.verbose,
        )
    except Exception as exc:
        log_error(f"Error during orphan recovery: {exc}")
        return 1

    if args.json:
        print(format_result_json(result))
    else:
        print(format_result_human(result))

    if result.total_orphaned > 0 and not args.recover:
        return 2

    return 0


if __name__ == "__main__":
    sys.exit(main())
