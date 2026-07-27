#!/usr/bin/env python3
"""Logical model-tier → concrete model-ID resolution (issue #3982).

The ``/loom:sweep`` escalation ladder, the tier-2.5 complexity bump, the
No-Fable-Judge fallback, the ``fable`` refusal fallback, the model-cost
experiment's Arm A, the role-default ``suggestedModel`` fields, and the daemon's
autonomous dispatch model all name *logical tiers* by their CLI alias —
``sonnet``, ``opus``, ``fable``. Two of those aliases resolve to the current
generation on the wire; ``opus`` lags a generation. Probed 2026-07-27::

    opus   → claude-opus-4-8   (generation 4)   ← the lagging rung
    sonnet → claude-sonnet-5   (generation 5)
    fable  → claude-fable-5    (generation 5)

That makes the shipped escalation ladder (``sonnet → sonnet@xhigh → opus →
fable``) **non-monotonic** — the ``sonnet@xhigh → opus`` step steps *down* a
generation, and Arm A of the #3718 model-cost experiment has been measuring a
previous-generation model against a current-generation Arm B.

This module is the **single indirection point** that maps a logical tier to the
concrete model ID it should dispatch on the wire, so every consumer keeps saying
``opus`` and exactly one place decides what ``opus`` means. The three consumers:

* the ``sweep.md`` skill — via the ``resolve-model.sh`` shell stub (this module's
  CLI), which it calls to resolve each rung/tier/arm before a subagent dispatch;
* ``loom_tools`` Python — ``sweep_experiment.resolved_arm_model`` resolves Arm A
  through here (``ARM_MODEL`` itself stays aliases);
* the Rust daemon — ``sweep_registry::resolve_dispatch_model`` reads the same
  ``.loom/config.json`` → ``sweep.modelAliases`` block and applies the same
  shipped default.

Design rules
------------
* **Only stale aliases are pinned.** The shipped default map pins exactly the
  tiers whose bare CLI alias resolves to an older generation — today just
  ``opus → claude-opus-5``. ``sonnet``/``fable`` are **not** pinned: the CLI
  already resolves them to the current generation, so they pass through unchanged
  and automatically track future generations with no edit here. Drop the ``opus``
  pin once the CLI's own ``opus`` alias rolls to gen 5.
* **Configurable.** ``.loom/config.json`` → ``sweep.modelAliases`` (an additive
  tier → ID object) overrides / extends the default map, so an operator can
  repoint a tier — or drop a pin (``{"opus": "opus"}`` maps ``opus`` back to the
  bare alias) — with no code change.
* **Passthrough is the default.** An input that is not a mapped tier (an unknown
  alias, or a pinned ID like ``claude-sonnet-4-6``) is returned unchanged, so a
  workspace that pins an exact ID is never rewritten.
* **The ``model@effort`` grammar is preserved.** A ``@effort`` suffix
  (``sonnet@xhigh``) is split off, the model half resolved, and the suffix
  reattached.
"""

from __future__ import annotations

import argparse
import json
import os
import re
import sys
from typing import Any

# --------------------------------------------------------------------------- #
# Constants
# --------------------------------------------------------------------------- #

# The generation each bare CLI alias resolves to on the wire TODAY (probed
# 2026-07-27). This is the guardrail the monotonicity test reads: ``opus`` lags
# at generation 4, which is the bug #3982 fixes at the resolution layer. Update
# this table (and drop the matching pin below) when Anthropic repoints an alias.
_ALIAS_GENERATION: dict[str, int] = {
    "haiku": 5,
    "sonnet": 5,
    "opus": 4,
    "fable": 5,
}

# Default logical-tier → pinned model-ID map. Pin ONLY tiers whose bare CLI alias
# resolves to a stale generation (see ``_ALIAS_GENERATION``). Everything else
# passes through unchanged. Overridable via ``.loom/config.json`` →
# ``sweep.modelAliases``.
_DEFAULT_TIER_ALIASES: dict[str, str] = {
    "opus": "claude-opus-5",
}


def _warn(msg: str) -> None:
    print(f"[model-tiers] WARNING: {msg}", file=sys.stderr)


# --------------------------------------------------------------------------- #
# Config (best-effort — never raises)
# --------------------------------------------------------------------------- #


def load_config(config_path: str | os.PathLike[str] | None = None) -> dict[str, Any]:
    """Best-effort read of ``.loom/config.json``; malformed/absent → ``{}``."""
    if config_path is None:
        config_path = ".loom/config.json"
    try:
        with open(config_path, encoding="utf-8") as fh:
            data = json.load(fh)
        return data if isinstance(data, dict) else {}
    except (OSError, ValueError):
        return {}


def _config_overrides(config: dict[str, Any] | None) -> dict[str, str]:
    """Extract the ``sweep.modelAliases`` override map (best-effort, tolerant).

    A non-dict ``sweep``/``modelAliases``, non-string keys/values, or blank
    values are dropped rather than raising — resolution must never block a sweep.
    """
    if not isinstance(config, dict):
        return {}
    sweep = config.get("sweep")
    aliases = sweep.get("modelAliases") if isinstance(sweep, dict) else None
    if not isinstance(aliases, dict):
        return {}
    out: dict[str, str] = {}
    for key, val in aliases.items():
        if isinstance(key, str) and isinstance(val, str) and val.strip():
            out[key.strip().lower()] = val.strip()
    return out


def tier_map(config: dict[str, Any] | None = None) -> dict[str, str]:
    """The effective tier→ID map: shipped defaults overlaid with config overrides."""
    return {**_DEFAULT_TIER_ALIASES, **_config_overrides(config)}


# --------------------------------------------------------------------------- #
# Resolution
# --------------------------------------------------------------------------- #


def resolve_model(model: str | None, config: dict[str, Any] | None = None) -> str:
    """Resolve a logical tier / alias / pinned ID to the concrete model ID.

    Preserves any ``@effort`` suffix (``sonnet@xhigh`` → ``sonnet@xhigh``,
    ``opus@xhigh`` → ``claude-opus-5@xhigh``). Unknown aliases and pinned IDs pass
    through unchanged. ``None``/empty → ``""``.
    """
    if not model:
        return ""
    base, sep, effort = model.partition("@")
    key = base.strip().lower()
    resolved = tier_map(config).get(key, base.strip())
    return f"{resolved}{sep}{effort}" if sep else resolved


def generation_of(model: str | None, config: dict[str, Any] | None = None) -> int | None:
    """The model generation a logical tier / ID resolves to on the wire.

    Resolves the input through the tier map first, then extracts the generation
    from the concrete ID (``claude-opus-5`` → 5, ``claude-sonnet-4-6`` → 4, the
    legacy ``claude-3-5-sonnet`` → 3). A bare alias that stays unmapped
    (``sonnet``/``fable``) is looked up in the probed ``_ALIAS_GENERATION`` table.
    Returns ``None`` for anything unrecognized.
    """
    resolved = resolve_model(model, config)
    base = resolved.partition("@")[0].strip().lower()
    if not base:
        return None
    # Modern IDs: claude-<family>-<gen>[-<minor>]
    m = re.match(r"^claude-[a-z]+-(\d+)", base)
    if m:
        return int(m.group(1))
    # Legacy IDs: claude-<gen>-...
    m = re.match(r"^claude-(\d+)-", base)
    if m:
        return int(m.group(1))
    # A bare alias that passed through unmapped.
    return _ALIAS_GENERATION.get(base)


# --------------------------------------------------------------------------- #
# CLI (the surface `resolve-model.sh` shells out to)
# --------------------------------------------------------------------------- #


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        prog="loom-resolve-model",
        description=(
            "Resolve a logical model tier/alias to the concrete model ID to "
            "dispatch on the wire (issue #3982). Unknown aliases and pinned IDs "
            "pass through unchanged."
        ),
    )
    parser.add_argument(
        "model",
        help="A logical tier/alias (opus, sonnet, sonnet@xhigh) or a pinned model ID.",
    )
    parser.add_argument(
        "--config",
        default=None,
        help="Path to .loom/config.json (default: ./.loom/config.json).",
    )
    parser.add_argument(
        "--generation",
        action="store_true",
        help="Print the resolved generation number instead of the model ID.",
    )
    return parser


def main(argv: list[str] | None = None) -> int:
    args = build_parser().parse_args(argv)
    config = load_config(args.config)
    if args.generation:
        gen = generation_of(args.model, config)
        print("" if gen is None else gen)
    else:
        print(resolve_model(args.model, config))
    return 0


if __name__ == "__main__":
    sys.exit(main())
