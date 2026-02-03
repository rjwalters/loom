"""Daemon action modules.

Each module handles a specific category of daemon actions:
- completions: Check shepherd/support role completions
- shepherds: Spawn shepherds for ready issues
- support_roles: Spawn support roles (interval and demand-based)
- proposals: Auto-promote proposals in force mode
"""

from loom_tools.daemon_v2.actions.completions import check_completions, handle_completion
from loom_tools.daemon_v2.actions.shepherds import spawn_shepherds
from loom_tools.daemon_v2.actions.support_roles import spawn_support_role
from loom_tools.daemon_v2.actions.proposals import promote_proposals

__all__ = [
    "check_completions",
    "handle_completion",
    "spawn_shepherds",
    "spawn_support_role",
    "promote_proposals",
]
