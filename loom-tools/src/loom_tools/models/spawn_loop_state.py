"""Model for ``.loom/spawn-loop-state.json`` (Phase 1, #3374).

The spawn loop writes a minimal state file with the following shape::

    {
      "started_at": "2026-06-02T16:12:19Z",
      "running": [
        {
          "issue": 42,
          "pid": 12345,
          "started_at": "2026-06-02T16:15:00Z",
          "token": "robb-personal",
          "output_file": "/path/to/.loom/logs/sweep-issue-42.log"
        },
        ...
      ]
    }

There are intentionally *no* shepherd-pool slot identifiers, no support-role
status, no pipeline state, no warnings, and no completed-issue history — the
spawn loop tracks only the bare minimum needed to reap dead children and
respect MAX_PARALLEL. Anything richer comes from the forge (`gh issue list`,
`gh pr list`).

The ``output_file`` field was added in Phase 3.1.4 (#3393) so the (now-removed)
``loom-completions`` CLI could detect silent failures (AGENT_EXIT_CODE marker +
mtime staleness) without re-deriving the spawn loop's per-issue log path
convention. ``loom-completions`` was deleted in v0.11.0 (#3633); the field is
retained for backward-compatibility with existing state files.

This module is part of the Phase 3 port (epic #3372, tracker #3378) and is
read by ``loom-status`` and other operator CLIs.
"""

from __future__ import annotations

from dataclasses import dataclass, field
from typing import Any


@dataclass
class SpawnLoopTask:
    """A single sweep child tracked by the spawn loop."""

    issue: int
    pid: int
    started_at: str | None = None
    token: str | None = None
    # The spawn loop does not currently write a heartbeat field, but reserving
    # the slot makes future schema additions (e.g., per-task heartbeat from
    # ``checkpoint.sh``) backwards compatible. ``loom-stuck-detection`` (a
    # sibling port) will likely populate this.
    last_heartbeat: str | None = None
    # Absolute path to the per-task output log formerly written by the
    # (deleted) ``spawn-loop.sh`` (Phase 3.1.4, #3393). Was consumed by the
    # ``loom-completions`` CLI (removed in v0.11.0, #3633) to detect silent
    # failures (AGENT_EXIT_CODE markers + mtime staleness). Optional; retained
    # for backward-compatibility with state files written before #3393.
    output_file: str | None = None

    @classmethod
    def from_dict(cls, data: dict[str, Any]) -> SpawnLoopTask:
        # ``issue`` and ``pid`` are required by the spawn loop; if missing we
        # synthesize sentinel values rather than crash. The CLI will simply
        # display them as ``?`` / ``0``.
        return cls(
            issue=int(data.get("issue") or 0),
            pid=int(data.get("pid") or 0),
            started_at=data.get("started_at"),
            token=data.get("token"),
            last_heartbeat=data.get("last_heartbeat"),
            output_file=data.get("output_file"),
        )

    def to_dict(self) -> dict[str, Any]:
        d: dict[str, Any] = {"issue": self.issue, "pid": self.pid}
        for k in ("started_at", "token", "last_heartbeat", "output_file"):
            v = getattr(self, k)
            if v is not None:
                d[k] = v
        return d


@dataclass
class SpawnLoopState:
    """Parsed contents of ``.loom/spawn-loop-state.json``.

    ``present`` is True when the file existed and parsed successfully.
    """

    started_at: str | None = None
    running: list[SpawnLoopTask] = field(default_factory=list)
    present: bool = False

    @classmethod
    def from_dict(cls, data: dict[str, Any]) -> SpawnLoopState:
        running_raw = data.get("running") or []
        tasks: list[SpawnLoopTask] = []
        if isinstance(running_raw, list):
            for item in running_raw:
                if isinstance(item, dict):
                    tasks.append(SpawnLoopTask.from_dict(item))
        return cls(
            started_at=data.get("started_at"),
            running=tasks,
            present=True,
        )

    @classmethod
    def absent(cls) -> SpawnLoopState:
        """Sentinel for "no spawn-loop-state file on disk." """
        return cls(present=False)
