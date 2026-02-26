"""CommandPoller: IPC between /loom skill and standalone daemon.

The /loom Claude Code skill writes JSON command files to .loom/signals/.
The standalone daemon calls CommandPoller.poll() to atomically consume
and act on those commands.

Signal files are named cmd-{timestamp}-{random}.json and processed
in sorted (timestamp) order. Each file is deleted immediately after
being read, ensuring no command is processed twice even if two daemon
instances race (only one will see a given file).

Signal payload format::

    {
        "action": "spawn_shepherd",
        "issue": 42,
        "mode": "default",
        "flags": [],
        "created_at": "2026-02-23T19:25:00Z",
        "ttl_seconds": 3600
    }

The ``created_at`` and ``ttl_seconds`` fields are optional but recommended.
Signals older than ``ttl_seconds`` are discarded. If ``ttl_seconds`` is
absent, ``max_age_seconds`` (set at construction time, default 1 hour) is
used as a fallback age limit based on the file's modification time.

Available actions:

+----------------------+---------------------------------------+
| Action               | Payload fields                        |
+======================+=======================================+
| start_orchestration  | mode (str: "default"|"force")         |
+----------------------+---------------------------------------+
| spawn_shepherd       | issue (int), mode (str), flags (list) |
+----------------------+---------------------------------------+
| stop                 | (none)                                |
+----------------------+---------------------------------------+
| pause_shepherd       | shepherd_id (str)                     |
+----------------------+---------------------------------------+
| resume_shepherd      | shepherd_id (str)                     |
+----------------------+---------------------------------------+
| set_max_shepherds    | count (int)                           |
+----------------------+---------------------------------------+

Note: ``start_orchestration`` must be sent by the /loom skill before the
daemon will run its autonomous iteration loop. Until received, the daemon
processes signals (including ``spawn_shepherd``) but does not auto-spawn
work from the GitHub snapshot. This allows ``/shepherd <N>`` to work
without activating full loom orchestration.
"""

from __future__ import annotations

import json
import logging
import os
import pathlib
import time
from datetime import datetime, timezone
from typing import Any

logger = logging.getLogger(__name__)

# Default maximum age for signal files (1 hour). Overridable via env var.
_DEFAULT_MAX_AGE_SECONDS = int(os.environ.get("LOOM_SIGNAL_MAX_AGE_SECONDS", "3600"))


class CommandPoller:
    """Atomically poll and consume JSON command files from .loom/signals/.

    Thread-safety note: poll() is safe to call from a single thread.
    Each file is unlinked immediately after being read, so concurrent
    daemon instances will not double-consume the same command.

    Args:
        workspace: Repository root directory.
        max_age_seconds: Discard signal files older than this many seconds
            (based on file mtime). Use 0 or None to disable age filtering.
            Defaults to ``LOOM_SIGNAL_MAX_AGE_SECONDS`` env var (1 hour).
    """

    def __init__(
        self,
        workspace: pathlib.Path,
        max_age_seconds: int | None = _DEFAULT_MAX_AGE_SECONDS,
    ) -> None:
        self.signals_dir = workspace / ".loom" / "signals"
        self.signals_dir.mkdir(parents=True, exist_ok=True)
        self.max_age_seconds = max_age_seconds or 0

    def _is_expired(self, signal_file: pathlib.Path, data: dict[str, Any]) -> bool:
        """Return True if this signal should be discarded as stale.

        Checks ``created_at`` + ``ttl_seconds`` from the payload first.
        Falls back to file mtime vs ``self.max_age_seconds`` if the payload
        fields are absent.
        """
        now = time.time()

        # Payload-based TTL check (authoritative when present and parseable)
        created_at_str = data.get("created_at")
        ttl_seconds = data.get("ttl_seconds")
        if created_at_str and ttl_seconds is not None:
            try:
                created_dt = datetime.fromisoformat(
                    created_at_str.replace("Z", "+00:00")
                )
                age = now - created_dt.replace(tzinfo=timezone.utc).timestamp()
                # Payload TTL is authoritative: return its verdict without
                # falling through to the mtime check.
                return age > ttl_seconds
            except (ValueError, TypeError):
                pass  # Malformed timestamp; fall through to mtime check

        # Fallback: file mtime check
        if self.max_age_seconds > 0:
            try:
                file_age = now - signal_file.stat().st_mtime
                if file_age > self.max_age_seconds:
                    return True
            except OSError:
                pass

        return False

    def poll(self) -> list[dict[str, Any]]:
        """Atomically consume and return all pending signal commands.

        Reads all ``*.json`` files from the signals directory in sorted
        (alphabetical/timestamp) order. Each file is deleted immediately
        after being read. Corrupt or unreadable files are skipped with
        a warning and also deleted to prevent re-processing. Stale files
        (older than ``max_age_seconds`` or expired per payload TTL) are
        discarded with a warning.

        Returns a list of command dicts (possibly empty).
        """
        commands: list[dict[str, Any]] = []

        try:
            signal_files = sorted(self.signals_dir.glob("*.json"))
        except OSError:
            return commands

        for signal_file in signal_files:
            try:
                data = json.loads(signal_file.read_text())
            except (OSError, json.JSONDecodeError):
                # Corrupt or unreadable — delete to prevent re-processing
                try:
                    signal_file.unlink(missing_ok=True)
                except OSError:
                    pass
                continue

            # Check staleness before consuming
            if isinstance(data, dict) and self._is_expired(signal_file, data):
                logger.warning(
                    "Discarding stale signal file %s (action=%s)",
                    signal_file.name,
                    data.get("action", "unknown"),
                )
                try:
                    signal_file.unlink(missing_ok=True)
                except OSError:
                    pass
                continue

            # Atomic consume: delete before appending so if we crash after
            # reading but before processing, the command is not replayed.
            try:
                signal_file.unlink()
            except OSError:
                pass

            if isinstance(data, dict):
                commands.append(data)

        return commands

    def requeue(self, command: dict[str, Any]) -> bool:
        """Write a command back to the signals directory for later processing.

        Used when a command cannot be processed immediately (e.g. daemon
        state not yet loaded during startup). The re-queued file is named
        with the current timestamp so it sorts after any freshly-written
        commands and is picked up on the next poll().

        Returns True if the command was successfully re-queued.
        """
        try:
            ts = int(time.time() * 1000)
            rand = os.urandom(4).hex()
            filename = f"cmd-{ts}-{rand}-requeued.json"
            signal_file = self.signals_dir / filename
            signal_file.write_text(json.dumps(command))
            return True
        except OSError:
            return False

    def queue_depth(self) -> int:
        """Return the number of pending signal files without consuming them."""
        try:
            return sum(1 for _ in self.signals_dir.glob("*.json"))
        except OSError:
            return 0
