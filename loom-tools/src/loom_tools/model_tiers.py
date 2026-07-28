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
# Complexity-tier → model resolution (issue #4238, "cost-of-being-wrong")
# --------------------------------------------------------------------------- #
#
# The Curator classifies each issue on one axis — *would a mistake be caught?* —
# and emits a ``<!-- loom:complexity=<tier> -->`` marker. The sweep resolves the
# dispatched model for that stratum from ``sweep.tierModels[<runtime>][<tier>]``,
# a runtime-neutral map of logical tiers (``haiku``/``sonnet``/``opus`` for the
# Claude runtime; a Codex adapter supplies its own IDs under its own runtime key).
# This is a SEPARATE, higher layer than ``sweep.modelAliases`` (the alias→ID
# indirection above): the profile/marker picks a *logical tier*, and
# ``resolve_model`` then turns that logical tier into the concrete wire ID. Never
# conflate the two — that separation is what keeps this runtime-neutral under the
# #4167 adapter contract.
#
# Absent ``sweep.tierModels`` ⇒ no mapping ⇒ ``resolve_tier_model`` returns ``""``
# and the caller falls through to its normal precedence chain, so the default
# (unconfigured) dispatch decision is byte-identical to today's behavior.

# The three cost-of-being-wrong strata. An absent/unknown marker means ``routine``.
COMPLEXITY_TIERS: tuple[str, ...] = ("mechanical", "routine", "complex")


def tier_models(config: dict[str, Any] | None = None) -> dict[str, dict[str, str]]:
    """The ``sweep.tierModels`` map: ``{runtime: {tier: logical_model}}``.

    Best-effort and tolerant — a non-dict ``sweep``/``tierModels``, non-string
    runtimes/tiers/models, or blank models are dropped rather than raising, so
    resolution never blocks a sweep. Runtime and tier keys are lower-cased.
    """
    if not isinstance(config, dict):
        return {}
    sweep = config.get("sweep")
    raw = sweep.get("tierModels") if isinstance(sweep, dict) else None
    if not isinstance(raw, dict):
        return {}
    out: dict[str, dict[str, str]] = {}
    for runtime, tiers in raw.items():
        if not isinstance(runtime, str) or not isinstance(tiers, dict):
            continue
        inner: dict[str, str] = {}
        for tier, model in tiers.items():
            if isinstance(tier, str) and isinstance(model, str) and model.strip():
                inner[tier.strip().lower()] = model.strip()
        if inner:
            out[runtime.strip().lower()] = inner
    return out


def resolve_tier_model(
    tier: str | None,
    runtime: str = "claude",
    config: dict[str, Any] | None = None,
) -> str:
    """Resolve a complexity tier to the concrete model ID to dispatch on the wire.

    Looks up ``sweep.tierModels[<runtime>][<tier>]`` (a logical tier) first — an
    operator-authored map is the more specific configuration and always wins.
    Absent that, falls back to the tier's entry (if any) in the
    ``sweep.optimization`` profile preset (see
    :func:`resolve_optimization_profile` / :func:`optimization_preset`, issue
    #4238 Phase B). Either way the resulting logical tier is passed through
    :func:`resolve_model` so a logical ``opus`` becomes the current-generation ID
    rather than a stale alias. An unrecognized/absent tier is treated as
    ``routine`` (the safe middle).

    Returns ``""`` when neither source has a mapping for that runtime/tier — the
    caller then falls through to its normal precedence chain (tier-3 role
    default), which keeps the unconfigured (or ``balanced``) dispatch decision
    byte-identical to today. Also returns ``""`` (with a warning) when the
    mapping would resolve to ``fable``: neither a Curator marker nor an
    optimization profile can ever dispatch the frontier/refusal-prone model —
    that is reserved for the objective escalation ladder or an explicit operator
    param (issue #3702).
    """
    key = (tier or "").strip().lower()
    if key not in COMPLEXITY_TIERS:
        key = "routine"
    rt = (runtime or "claude").strip().lower()
    source = "tierModels"
    logical = tier_models(config).get(rt, {}).get(key)
    if not logical:
        source = "optimization"
        profile = resolve_optimization_profile(config)
        logical = optimization_preset(profile).get(key, "")
    if not logical:
        return ""
    # No-Fable hard bound: refuse a mapping that names fable, before resolution…
    if logical.partition("@")[0].strip().lower() == "fable":
        _warn(f"{source}[{rt}][{key}] maps to 'fable' — refusing (No-Fable bound); falling through")
        return ""
    resolved = resolve_model(logical, config)
    # …and after resolution, in case an alias/override lands on a fable model ID.
    if "fable" in resolved.partition("@")[0].lower():
        _warn(f"{source}[{rt}][{key}] resolves to a fable model — refusing; falling through")
        return ""
    return resolved


# --------------------------------------------------------------------------- #
# Optimization profile → tierModels preset (issue #4238, Phase B)
# --------------------------------------------------------------------------- #
#
# ``sweep.optimization`` is an operator-facing policy switch — ``"cost"`` |
# ``"speed"`` | ``"balanced"`` (default) — that selects a PRESET over the
# ``sweep.tierModels[<runtime>][<tier>]`` map above, rather than a fixed
# one-step bump. The preset is expressed in the same runtime-neutral logical
# tiers (``haiku``/``sonnet``/``opus``) as an operator-authored ``tierModels``
# map and applies uniformly across runtimes: a Codex adapter under the #4167
# contract resolves the same logical names to its own IDs, so no per-runtime
# preset table is needed. An explicit ``sweep.tierModels[<runtime>][<tier>]``
# entry, if the operator has set one, still wins over the derived preset — see
# ``resolve_tier_model`` above, which checks ``tierModels`` before falling back
# to the profile preset.

OPTIMIZATION_PROFILES: tuple[str, ...] = ("cost", "speed", "balanced")

# tier -> logical model, per profile. "balanced" is intentionally EMPTY: an
# absent/default profile must not materialize any preset, so a repo with no
# `sweep.optimization` configured (or explicitly set to "balanced") dispatches
# byte-identically to pre-Phase-B behavior — the acceptance-criterion this
# module is tested against.
_OPTIMIZATION_PRESETS: dict[str, dict[str, str]] = {
    # Cheapest model the Judge gate can safely correct, full 3-stratum spread.
    "cost": {"mechanical": "haiku", "routine": "sonnet", "complex": "opus"},
    # Wall-clock in a sweep is dominated by Judge-rejection / Doctor round-trip
    # COUNT, not per-turn latency — so "speed" starts a tier higher than
    # "balanced" to buy fewer retry cycles, rather than fewer/cheaper tokens per
    # turn. `complex` is already at the ceiling (`opus`) under "balanced" via the
    # tier-2.5 bump, so "speed" leaves it unchanged and instead raises the two
    # strata that would otherwise dispatch below opus.
    "speed": {"mechanical": "sonnet", "routine": "opus", "complex": "opus"},
    "balanced": {},
}


def resolve_optimization_profile(
    config: dict[str, Any] | None = None,
    env: dict[str, str] | None = None,
) -> str:
    """The effective ``sweep.optimization`` profile: env > config > default.

    Precedence: ``LOOM_SWEEP_OPTIMIZATION`` env var, then ``.loom/config.json``
    → ``sweep.optimization``, then ``"balanced"``. An unrecognized value from
    either source warns and falls back to ``"balanced"`` — optimization-profile
    resolution is best-effort and must never fail dispatch, matching every other
    soft-fail config read in this module.
    """
    src = env if env is not None else os.environ
    raw: Any = src.get("LOOM_SWEEP_OPTIMIZATION")
    source = "env LOOM_SWEEP_OPTIMIZATION"
    if not raw:
        sweep = config.get("sweep") if isinstance(config, dict) else None
        raw = sweep.get("optimization") if isinstance(sweep, dict) else None
        source = "config sweep.optimization"
    if raw is None or raw == "":
        # Genuinely unset (neither source provided a value) — the silent,
        # expected default. No warning: this is the common case.
        return "balanced"
    if not isinstance(raw, str):
        _warn(f"{source}={raw!r} is not a string; falling back to 'balanced'")
        return "balanced"
    value = raw.strip().lower()
    if not value or value not in OPTIMIZATION_PROFILES:
        _warn(f"{source}={raw!r} is not one of {OPTIMIZATION_PROFILES}; falling back to 'balanced'")
        return "balanced"
    return value


def optimization_preset(profile: str) -> dict[str, str]:
    """The tier → logical-model preset for an optimization profile.

    An unrecognized profile name returns the empty (``balanced``) preset rather
    than raising — callers should resolve the profile through
    :func:`resolve_optimization_profile` first (which already normalizes and
    falls back), but this stays defensive against direct callers.
    """
    return _OPTIMIZATION_PRESETS.get(profile, {})


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
# Task-tool degradation (issue #4282)
# --------------------------------------------------------------------------- #
#
# The daemon/process-spawn path dispatches a model as `--model <id>`, which the
# `claude` CLI accepts as either an alias or a pinned ID — so #3982's resolved IDs
# (`claude-opus-5`) ride through unchanged. But the *in-session* Task/Agent tool's
# `model` parameter is an **alias-only enum** (`sonnet | opus | haiku | fable`): a
# pinned ID is an invalid value there, so on the dispatch path `/loom:sweep` uses
# for its per-role subagents, a resolved ID must degrade back to its family alias.
# `task_alias_of` is that reverse mapping — a deterministic lookup so the
# degradation is not per-orchestrator judgement. It composes with the #3705
# `@effort` degradation (the Task tool exposes no effort knob either).

# The alias values the in-session Task/Agent tool's `model` parameter accepts.
_TASK_TOOL_ALIASES: frozenset[str] = frozenset({"haiku", "sonnet", "opus", "fable"})


def task_alias_of(model: str | None, config: dict[str, Any] | None = None) -> str:
    """Map a model to the nearest value the in-session Task/Agent tool accepts.

    The Task tool's ``model`` parameter is an alias-only enum
    (``haiku``/``sonnet``/``opus``/``fable``); a pinned ID like ``claude-opus-5``
    is an invalid value there, so #3982's resolved IDs cannot be passed on the
    in-session dispatch path and must degrade to their family alias (issue #4282).

    * A value already in the Task enum passes through unchanged.
    * A concrete ID maps to its family alias by parsing the family segment
      (``claude-opus-5 → opus``, ``claude-sonnet-4-6 → sonnet``, the legacy
      ``claude-3-5-sonnet → sonnet``) — the same ID grammar :func:`generation_of`
      reads. A fable-family ID maps to ``fable`` mechanically; the No-Fable-Judge
      invariant is a caller responsibility (fall to ``opus`` *before* aliasing),
      not policy baked in here.
    * An ``@effort`` suffix is stripped (the Task tool has no effort parameter
      either — composes with the #3705 degradation rule).
    * Anything unrecognized/unparseable returns ``""`` — the caller then omits the
      ``model`` parameter so the subagent inherits the parent/agent-definition
      model rather than dispatching a guessed alias. Resolution never raises.

    ``config`` is accepted for signature parity with the other resolvers; the
    reverse mapping is mechanical and config-independent (its input is already a
    resolved ID/alias, so there is nothing left to look up).
    """
    if not model:
        return ""
    base = model.partition("@")[0].strip().lower()
    if not base:
        return ""
    # Already a Task-passable alias (or a bare alias that resolved to itself).
    if base in _TASK_TOOL_ALIASES:
        return base
    # Modern IDs: claude-<family>-<gen>[-<minor>] → the family segment is the alias.
    m = re.match(r"^claude-([a-z]+)-\d", base)
    if m and m.group(1) in _TASK_TOOL_ALIASES:
        return m.group(1)
    # Legacy IDs: claude-<gen>-...-<family> (claude-3-5-sonnet, claude-3-opus).
    for family in _TASK_TOOL_ALIASES:
        if base.endswith(f"-{family}"):
            return family
    return ""


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
        nargs="?",
        default=None,
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
    parser.add_argument(
        "--task-alias",
        action="store_true",
        help=(
            "Map the model back to the nearest value the in-session Task/Agent "
            "tool's `model` enum accepts (haiku|sonnet|opus|fable), for the "
            "dispatch path that cannot pass a pinned ID (issue #4282). A concrete "
            "ID degrades to its family alias; an @effort suffix is stripped. Exits "
            "3 with no output when the input has no Task-passable alias (caller "
            "then omits `model` so the subagent inherits the parent model)."
        ),
    )
    parser.add_argument(
        "--tier",
        default=None,
        help=(
            "Complexity-tier mode (issue #4238): resolve the model for "
            "sweep.tierModels[<runtime>][<tier>] instead of a bare alias. Exits 3 "
            "with no output when the runtime/tier has no mapping (caller falls "
            "through to its normal precedence chain)."
        ),
    )
    parser.add_argument(
        "--runtime",
        default="claude",
        help="Worker runtime for --tier resolution (default: claude).",
    )
    return parser


def main(argv: list[str] | None = None) -> int:
    parser = build_parser()
    args = parser.parse_args(argv)
    config = load_config(args.config)
    # Complexity-tier mode (issue #4238): sweep.tierModels[<runtime>][<tier>].
    if args.tier is not None:
        resolved = resolve_tier_model(args.tier, args.runtime, config)
        if not resolved:
            # No mapping (or a refused fable map): print nothing, signal
            # fall-through with exit 3 so the caller keeps its normal chain.
            return 3
        print(resolved)
        return 0
    if args.model is None:
        parser.error("a model argument or --tier is required")
    # Task-tool degradation mode (issue #4282): map the resolved model back to a
    # Task-passable alias for the in-session dispatch path.
    if args.task_alias:
        alias = task_alias_of(args.model, config)
        if not alias:
            # No Task-passable alias: print nothing, signal fall-through with
            # exit 3 (mirror --tier) so the caller omits `model` and the subagent
            # inherits the parent/agent-definition model.
            return 3
        print(alias)
        return 0
    if args.generation:
        gen = generation_of(args.model, config)
        print("" if gen is None else gen)
    else:
        print(resolve_model(args.model, config))
    return 0


if __name__ == "__main__":
    sys.exit(main())
