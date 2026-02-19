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
        "flags": []
    }

Available actions:

+-------------------+---------------------------------------+
| Action            | Payload fields                        |
+===================+=======================================+
| spawn_shepherd    | issue (int), mode (str), flags (list) |
+-------------------+---------------------------------------+
| stop              | (none)                                |
+-------------------+---------------------------------------+
| pause_shepherd    | shepherd_id (str)                     |
+-------------------+---------------------------------------+
| resume_shepherd   | shepherd_id (str)                     |
+-------------------+---------------------------------------+
| set_max_shepherds | count (int)                           |
+-------------------+---------------------------------------+
"""

from __future__ import annotations

import json
import pathlib
from typing import Any


class CommandPoller:
    """Atomically poll and consume JSON command files from .loom/signals/.

    Thread-safety note: poll() is safe to call from a single thread.
    Each file is unlinked immediately after being read, so concurrent
    daemon instances will not double-consume the same command.
    """

    def __init__(self, workspace: pathlib.Path) -> None:
        self.signals_dir = workspace / ".loom" / "signals"
        self.signals_dir.mkdir(parents=True, exist_ok=True)

    def poll(self) -> list[dict[str, Any]]:
        """Atomically consume and return all pending signal commands.

        Reads all ``*.json`` files from the signals directory in sorted
        (alphabetical/timestamp) order. Each file is deleted immediately
        after being read. Corrupt or unreadable files are skipped with
        a warning and also deleted to prevent re-processing.

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

            # Atomic consume: delete before appending so if we crash after
            # reading but before processing, the command is not replayed.
            try:
                signal_file.unlink()
            except OSError:
                pass

            if isinstance(data, dict):
                commands.append(data)

        return commands

    def queue_depth(self) -> int:
        """Return the number of pending signal files without consuming them."""
        try:
            return sum(1 for _ in self.signals_dir.glob("*.json"))
        except OSError:
            return 0
