"""Config resolution layer (#4039, Epic #3835 Phase 2).

A single resolver over the tier chain: private/shared defaults, the legacy
``.loom/config.json``, tracked ``.loom-project/project.json``, and ignored
``.loom-local/local.json``. See ``docs/design/config-resolution-tiers.md``
for the full design (schema, precedence table, and how this composes with
the existing **env > config > default** rule used by e.g. the ``autonomous``
block).

**This module is additive only.** No existing call site (e.g.
``loom_tools.common.paths._read_config_worktree_root``) is migrated onto it
in this PR. A follow-up issue migrates call sites one at a time.

Mirrors ``loom-daemon/src/config_resolver.rs`` and
``defaults/scripts/lib/config-resolver.sh`` key-for-key so the three
resolvers agree on one fixture tree (cross-language conformance test).

Example::

    from pathlib import Path
    from loom_tools.common.config_resolver import (
        get_path,
        resolve_effective_config,
    )

    effective = resolve_effective_config(Path("/path/to/repo"))
    enabled = get_path(effective, "autonomous.workFinder.enabled")
"""

from __future__ import annotations

import os
from pathlib import Path
from typing import Any

#: Repo-relative path to the legacy, currently load-bearing config file.
#: Every existing repo has (at most) this tier populated.
LEGACY_CONFIG_REL = ".loom/config.json"

#: Repo-relative path to the tracked, project-specific config file
#: introduced by Epic #3835. Empty (absent) in every repo until a repo
#: migrates onto the new layout (Phase 6).
PROJECT_CONFIG_REL = ".loom-project/project.json"

#: Repo-relative path to the ignored, host-local config override file.
#: Empty (absent) in every repo until Phase 3+.
LOCAL_CONFIG_REL = ".loom-local/local.json"

#: Env var overriding the private/shared defaults file location. Set to a
#: non-empty path to point at an alternate file; set to the empty string to
#: disable this tier entirely.
PRIVATE_DEFAULTS_ENV = "LOOM_CONFIG_DEFAULTS_FILE"

#: Home-relative default location of the private/shared defaults file. Epic
#: #3835 Phase 3 introduces the machine-level checkout this lives in; until
#: that phase ships the file simply does not exist on any host, so this
#: tier resolves to "contributes nothing" everywhere today.
_DEFAULT_PRIVATE_DEFAULTS_REL = ".local/share/loom/config/defaults.json"


def private_defaults_path() -> Path | None:
    """Resolve the path to the private/shared defaults file.

    Honors :data:`PRIVATE_DEFAULTS_ENV`. Returns ``None`` when the env var
    is explicitly set to an empty string (tier disabled) or when no home
    directory can be determined.

    Returns:
        The resolved path (which may or may not exist on disk), or
        ``None`` when this tier is disabled.
    """
    if PRIVATE_DEFAULTS_ENV in os.environ:
        override = os.environ[PRIVATE_DEFAULTS_ENV]
        return Path(override) if override else None
    try:
        home = Path.home()
    except RuntimeError:
        return None
    return home / _DEFAULT_PRIVATE_DEFAULTS_REL


def _soft_read_json_object(path: Path) -> dict[str, Any]:
    """Soft-fail JSON-object read.

    A missing file, an unreadable file, malformed JSON, or a non-object
    top-level value all resolve to an empty dict (``{}``) — that tier
    simply contributes nothing to the merge. Never a hard error, mirroring
    the soft-fail contract already used by
    ``loom_tools.common.paths._read_config_worktree_root``.

    Args:
        path: Path to the candidate JSON file.

    Returns:
        The parsed object, or ``{}`` on any failure.
    """
    # Imported lazily to avoid a module-level import cycle
    # (common.state has no dependency on config_resolver, but keeping the
    # import lazy here matches the existing convention in
    # common.paths._read_config_worktree_root).
    from loom_tools.common.state import read_json_file

    data = read_json_file(path, default={})
    return data if isinstance(data, dict) else {}


def deep_merge(base: Any, overlay: Any) -> Any:
    """Deep-merge ``overlay`` onto ``base``, returning the merged value.

    Semantics (matching ``jq``'s ``*`` operator exactly, so the Bash
    resolver can implement this via ``jq -n '$a * $b * $c * $d'`` and stay
    in lockstep):

    - When both sides are dicts, merge recursively key-by-key.
    - Otherwise, ``overlay`` replaces ``base`` outright — including when
      ``overlay`` is ``None``/``null`` (an explicit null in a higher tier
      means "clear this key", not "leave the lower tier's value").

    Args:
        base: The lower-precedence value.
        overlay: The higher-precedence value.

    Returns:
        The merged value. Never mutates ``base`` or ``overlay`` in place.
    """
    if isinstance(base, dict) and isinstance(overlay, dict):
        merged = dict(base)
        for key, value in overlay.items():
            if key in merged:
                merged[key] = deep_merge(merged[key], value)
            else:
                merged[key] = value
        return merged
    return overlay


def resolve_effective_config(repo_root: Path) -> dict[str, Any]:
    """Resolve the full effective config tree for ``repo_root``.

    Deep-merges, lowest to highest precedence:

    1. Private/shared defaults (:func:`private_defaults_path`) — a no-op
       tier on every host until Epic #3835 Phase 3 ships.
    2. Legacy ``.loom/config.json`` — the sole tier every existing repo has.
    3. Tracked ``.loom-project/project.json`` — new in #4039; empty until a
       repo migrates (Phase 6).
    4. Ignored ``.loom-local/local.json`` — new in #4039; host-local
       override.

    A missing or malformed file at **any** tier soft-fails to that tier
    contributing nothing, and this function never raises. When only tier 2
    has content — the state of every repo today — the result is exactly
    that file's parsed content: byte-for-byte identical to reading
    ``.loom/config.json`` directly, satisfying the #4039
    behavior-preservation requirement.

    Args:
        repo_root: The absolute repository root path.

    Returns:
        The merged effective config dict.
    """
    effective: dict[str, Any] = {}

    defaults_path = private_defaults_path()
    if defaults_path is not None:
        effective = deep_merge(effective, _soft_read_json_object(defaults_path))

    effective = deep_merge(effective, _soft_read_json_object(repo_root / LEGACY_CONFIG_REL))
    effective = deep_merge(effective, _soft_read_json_object(repo_root / PROJECT_CONFIG_REL))
    effective = deep_merge(effective, _soft_read_json_object(repo_root / LOCAL_CONFIG_REL))

    return effective


_MISSING = object()


def get_path(config: dict[str, Any], dotted: str, default: Any = None) -> Any:
    """Look up a dotted key path in an already-resolved effective config tree.

    Args:
        config: The merged effective config (from
            :func:`resolve_effective_config`).
        dotted: A dotted key path, e.g. ``"autonomous.workFinder.enabled"``.
        default: Value to return when the path is absent or indexes through
            a non-dict value.

    Returns:
        The resolved value, or ``default`` when the path can't be
        resolved.
    """
    cur: Any = config
    for segment in dotted.split("."):
        if not isinstance(cur, dict):
            return default
        cur = cur.get(segment, _MISSING)
        if cur is _MISSING:
            return default
    return cur
