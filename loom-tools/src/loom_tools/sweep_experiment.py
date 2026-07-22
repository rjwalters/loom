#!/usr/bin/env python3
"""Model-cost experiment instrumentation for /loom:sweep (issue #3725).

This module is the deterministic, unit-testable core behind the sweep skill's
``sweep.modelExperiment`` mode. The sweep skill is LLM-executed markdown, so the
load-bearing arithmetic (tri-state resolution, per-issue arm assignment, the
durable JSONL append) lives here as a small CLI the skill shells out to — the LLM
never computes a modulo by hand.

Surface (all subcommands print to stdout; warnings go to stderr):

    resolve-mode                       -> effective mode after the canary guardrail
    assign-arm --issue N [--complexity]-> deterministic arm + forced model
    banner --issue N [--complexity]    -> loud startup banner naming mode + arm
    record ...                         -> append one JSONL outcome-chain record
    harvest [--archive-dir DIR]        -> per-arm inequality inputs for #3718

Design notes
------------
* **Tri-state** ``off`` / ``observe`` / ``experiment`` resolves env-over-config,
  following the *string-valued* guard precedence (``guards.rmScope`` /
  ``guards.forceScope``), never the boolean pattern. Unknown value -> ``off`` +
  warning; a best-effort config read never raises.
* **Two arms** map onto #3718's inequality: Arm A = Builder->opus (opus-first),
  Arm B = Builder->sonnet + escalate-on-Judge-rejection (sonnet-first).
* **Deterministic, resume-safe, stratified** arm assignment is a pure function of
  the issue number and the #3702 complexity marker, so a killed-and-resumed sweep
  re-lands the same arm.
* **Exact per-role cost** is computed at harvest time by parsing each role
  subagent's ``agent-<id>.jsonl`` ``usage`` blocks (input/output + cache split)
  with cache-aware pricing (mirrors ``loom-daemon`` ``resource_usage.rs``). The
  transcripts are located through #3726's ``loom.transcript-index/v1`` archive
  index, joined on the agent-id stamped in each stats record.
"""

from __future__ import annotations

import argparse
import json
import os
import pathlib
import subprocess
import sys
from datetime import datetime, timezone
from typing import Any, Callable

# --------------------------------------------------------------------------- #
# Constants
# --------------------------------------------------------------------------- #

VALID_MODES = ("off", "observe", "experiment")
_TRUTHY = {"1", "true", "yes", "on"}

# Gitignored, repo-local sentinel that confirms a canary WITHOUT travelling in a
# committed config (issue #3731). Its confirmation power comes precisely from
# being uncommitted — a git-tracked copy is refused (see ``evaluate_canary``).
CANARY_SENTINEL = ".loom/CANARY"

# Arm -> forced Builder model. Arm A is opus-first, Arm B is sonnet-first.
ARM_MODEL = {"A": "opus", "B": "sonnet"}

DEFAULT_STATS_FILE = ".loom/stats/sweep-model-stats.jsonl"


def _now_iso() -> str:
    return datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")


def _warn(msg: str) -> None:
    print(f"[sweep-experiment] WARNING: {msg}", file=sys.stderr)


# --------------------------------------------------------------------------- #
# Config (best-effort — never raises)
# --------------------------------------------------------------------------- #


def load_config(config_path: str | os.PathLike[str] | None) -> dict[str, Any]:
    """Best-effort read of ``.loom/config.json``; malformed/absent -> ``{}``."""
    if config_path is None:
        config_path = ".loom/config.json"
    try:
        with open(config_path, encoding="utf-8") as fh:
            data = json.load(fh)
        return data if isinstance(data, dict) else {}
    except (OSError, ValueError):
        return {}


# --------------------------------------------------------------------------- #
# Tri-state mode resolution + canary guardrail
# --------------------------------------------------------------------------- #


def resolve_raw_mode(
    env: dict[str, str], config: dict[str, Any]
) -> tuple[str, list[str]]:
    """Resolve the tri-state mode env-over-config, before the canary guardrail.

    Returns ``(mode, warnings)``. Unknown/malformed value -> ``off`` + a warning.
    """
    warnings: list[str] = []

    raw = env.get("LOOM_MODEL_EXPERIMENT")
    if raw is not None and raw.strip() != "":
        val = raw.strip().lower()
        if val in VALID_MODES:
            return val, warnings
        warnings.append(
            f"LOOM_MODEL_EXPERIMENT={raw!r} is not one of {VALID_MODES}; treating as 'off'"
        )
        return "off", warnings

    sweep = config.get("sweep")
    cfg_val = sweep.get("modelExperiment") if isinstance(sweep, dict) else None
    if isinstance(cfg_val, str) and cfg_val.strip().lower() in VALID_MODES:
        return cfg_val.strip().lower(), warnings
    if cfg_val is not None:
        warnings.append(
            f"sweep.modelExperiment={cfg_val!r} is not one of {VALID_MODES}; treating as 'off'"
        )

    return "off", warnings


def _sentinel_is_tracked(path: pathlib.Path) -> bool:
    """Best-effort: is ``path`` tracked by git in its repo? Any error -> ``False``.

    A canary sentinel that is committed defeats the whole point of #3731 (it would
    propagate with the repo exactly like the retired ``sweep.modelExperimentCanary``
    config did), so a tracked sentinel is refused as a confirmation source.
    """
    try:
        result = subprocess.run(
            ["git", "ls-files", "--error-unmatch", "--", path.name],
            cwd=str(path.parent) if str(path.parent) else None,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            check=False,
        )
        return result.returncode == 0
    except (OSError, ValueError):
        return False


def evaluate_canary(
    env: dict[str, str],
    *,
    sentinel_path: str | os.PathLike[str] | None = None,
    is_tracked: Callable[[pathlib.Path], bool] | None = None,
) -> tuple[bool, str | None, list[str]]:
    """Evaluate canary confirmation from an UNCOMMITTED signal only (#3731).

    Returns ``(confirmed, source, warnings)`` where ``source`` is ``"env"`` |
    ``"sentinel"`` | ``None``.

    Confirmation is accepted ONLY from a signal that cannot travel with a copied,
    committed ``.loom/config.json``:

    * ``(a)`` the ``LOOM_MODEL_EXPERIMENT_CANARY`` env var (truthy), or
    * ``(b)`` a **gitignored** sentinel file (``.loom/CANARY``) present on disk.

    The committed ``sweep.modelExperimentCanary`` config flag is **no longer**
    accepted — it was the accidental-production-fire vector (a config carrying both
    the mode and the confirmation propagates to prod via ``defaults/``). If the
    sentinel is git-TRACKED it is refused (with a warning): a committed sentinel is
    just the config-propagation vector by another name.
    """
    warnings: list[str] = []

    raw = env.get("LOOM_MODEL_EXPERIMENT_CANARY")
    if raw is not None and raw.strip() != "" and raw.strip().lower() in _TRUTHY:
        return True, "env", warnings

    path = (
        pathlib.Path(sentinel_path)
        if sentinel_path is not None
        else pathlib.Path(CANARY_SENTINEL)
    )
    if path.exists():
        tracked_fn = is_tracked if is_tracked is not None else _sentinel_is_tracked
        if tracked_fn(path):
            warnings.append(
                f"canary sentinel {path} is TRACKED by git — refusing it as a "
                "confirmation source. A committed sentinel defeats the "
                "uncommitted-signal guardrail (#3731); gitignore it and run "
                f"`git rm --cached {path}`."
            )
        else:
            return True, "sentinel", warnings

    return False, None, warnings


def canary_confirmed(
    env: dict[str, str],
    config: dict[str, Any] | None = None,
    *,
    sentinel_path: str | os.PathLike[str] | None = None,
    is_tracked: Callable[[pathlib.Path], bool] | None = None,
) -> bool:
    """Has the operator confirmed this target is a canary via an UNCOMMITTED signal?

    Thin boolean wrapper over :func:`evaluate_canary`. The ``config`` argument is
    accepted for backward compatibility but is **ignored** — committed config is no
    longer an accepted confirmation source (#3731).
    """
    confirmed, _source, _warnings = evaluate_canary(
        env, sentinel_path=sentinel_path, is_tracked=is_tracked
    )
    return confirmed


def resolve_effective_mode(
    env: dict[str, str],
    config: dict[str, Any],
    *,
    sentinel_path: str | os.PathLike[str] | None = None,
    is_tracked: Callable[[pathlib.Path], bool] | None = None,
) -> tuple[str, list[str]]:
    """Resolve mode AND apply the canary guardrail.

    ``experiment`` is behavior-changing (it forces Builder models and suppresses
    the complexity bump), so it is honored only on a confirmed canary. On any
    other target it is loudly downgraded to ``observe`` (safe anywhere) rather
    than refused outright — the measurement still accrues, just without the
    model-forcing behavior change. Confirmation must come from an UNCOMMITTED
    signal (#3731); committed ``sweep.modelExperimentCanary`` is inert.
    """
    mode, warnings = resolve_raw_mode(env, config)
    if mode == "experiment":
        confirmed, _source, canary_warnings = evaluate_canary(
            env, sentinel_path=sentinel_path, is_tracked=is_tracked
        )
        warnings.extend(canary_warnings)
        if not confirmed:
            warnings.append(
                "experiment mode requested on a NON-CANARY target — downgrading to "
                "'observe' (no model forcing). Confirm a canary via an UNCOMMITTED "
                "signal: export LOOM_MODEL_EXPERIMENT_CANARY=1 or create the "
                "gitignored sentinel .loom/CANARY. (Committed "
                "sweep.modelExperimentCanary is no longer accepted — #3731.)"
            )
            mode = "observe"
    return mode, warnings


# --------------------------------------------------------------------------- #
# Deterministic, resume-safe, stratified arm assignment
# --------------------------------------------------------------------------- #


def normalize_complexity(complexity: str | None) -> str:
    """Normalize the #3702 marker to ``complex`` | ``routine`` (default routine)."""
    return "complex" if (complexity or "").strip().lower() == "complex" else "routine"


def assign_arm(issue_number: int, complexity: str | None = None) -> str:
    """Deterministically assign ``A`` or ``B`` for an issue.

    Pure function of ``issue_number`` and the complexity stratum, so a resumed
    sweep re-running the same issue lands on the same arm. The complexity bit
    offsets the parity split so the ``complex`` and ``routine`` strata each get an
    independent ~50/50 A/B balance (stratification) rather than correlating.
    """
    bit = 1 if normalize_complexity(complexity) == "complex" else 0
    return "A" if (int(issue_number) + bit) % 2 == 0 else "B"


def arm_model(arm: str) -> str:
    return ARM_MODEL.get(arm.upper().strip(), "")


# --------------------------------------------------------------------------- #
# Durable stats store (atomic O_APPEND, one JSONL line per phase invocation)
# --------------------------------------------------------------------------- #


def _stats_path(stats_file: str | None) -> pathlib.Path:
    return pathlib.Path(stats_file or DEFAULT_STATS_FILE)


def append_record(record: dict[str, Any], stats_file: str | None = None) -> None:
    """Append one JSONL record atomically.

    POSIX ``O_APPEND`` guarantees per-line atomicity for concurrent detached
    writers, so no lock is needed for single-line writes.
    """
    path = _stats_path(stats_file)
    parent = path.parent
    try:
        parent.mkdir(parents=True, exist_ok=True)
        # Best-effort: keep the stats dir private (it can carry issue context).
        try:
            os.chmod(parent, 0o700)
        except OSError:
            pass
    except OSError:
        pass
    line = json.dumps(record, ensure_ascii=False) + "\n"
    fd = os.open(str(path), os.O_WRONLY | os.O_CREAT | os.O_APPEND, 0o600)
    try:
        os.write(fd, line.encode("utf-8"))
    finally:
        os.close(fd)


def build_record(
    *,
    issue: int,
    phase: str,
    role: str,
    model: str | None = None,
    mode: str = "observe",
    arm: str | None = None,
    attempt: int = 1,
    complexity: str | None = None,
    judge_verdict: str | None = None,
    cycle_count: int = 0,
    pr: int | None = None,
    effort: str | None = None,
    agent_id: str | None = None,
    transcript: str | None = None,
    in_tok: int | None = None,
    out_tok: int | None = None,
    token_fidelity: str = "none",
) -> dict[str, Any]:
    """Assemble one outcome-chain record.

    The HARD deterministic fields (arm/model/attempt/verdict/cycle_count/
    complexity) are the load-bearing evidence for #3718. ``agent_id`` is the join
    key into #3726's transcript index for exact-cost harvest.
    """
    return {
        "ts": _now_iso(),
        "issue": int(issue),
        "pr": int(pr) if pr is not None else None,
        "mode": mode,
        "phase": phase,
        "role": role,
        "model": model,
        "effort": effort,
        "arm": (arm.upper() if arm else None),
        "attempt": int(attempt),
        "complexity": normalize_complexity(complexity),
        "judge_verdict": judge_verdict,
        "cycle_count": int(cycle_count),
        "agent_id": agent_id,
        "transcript": transcript,
        "in_tok": in_tok,
        "out_tok": out_tok,
        "token_fidelity": token_fidelity,
    }


# --------------------------------------------------------------------------- #
# Cache-aware pricing (mirrors loom-daemon resource_usage.rs::ModelPricing)
# --------------------------------------------------------------------------- #

# (input, output, cache_read, cache_write) US$ per 1k tokens.
def model_pricing(model: str | None) -> tuple[float, float, float, float]:
    m = (model or "").lower()
    if "claude-3-5-sonnet" in m or "claude-sonnet-4" in m or m == "sonnet":
        return (0.003, 0.015, 0.0003, 0.00375)
    if "claude-3-opus" in m or "claude-opus-4" in m or m == "opus":
        return (0.015, 0.075, 0.0015, 0.01875)
    if "claude-3-haiku" in m or m == "haiku":
        return (0.00025, 0.00125, 0.00003, 0.0003)
    # Default to Sonnet pricing as a reasonable middle ground (matches Rust).
    return (0.003, 0.015, 0.0003, 0.00375)


def calc_cost(
    input_tokens: int,
    output_tokens: int,
    cache_read_tokens: int,
    cache_write_tokens: int,
    model: str | None,
) -> float:
    inp, out, cr, cw = model_pricing(model)
    return (
        input_tokens / 1000.0 * inp
        + output_tokens / 1000.0 * out
        + cache_read_tokens / 1000.0 * cr
        + cache_write_tokens / 1000.0 * cw
    )


def sum_transcript_usage(path: str | os.PathLike[str]) -> dict[str, Any]:
    """Sum every ``usage`` block in a subagent transcript.

    Returns exact input/output/cache-read/cache-creation token totals plus the
    resolved model. Best-effort: unreadable lines are skipped, a missing file
    yields zeros.
    """
    totals = {
        "input_tokens": 0,
        "output_tokens": 0,
        "cache_read_input_tokens": 0,
        "cache_creation_input_tokens": 0,
    }
    model: str | None = None
    blocks = 0
    try:
        with open(path, encoding="utf-8") as fh:
            for raw in fh:
                raw = raw.strip()
                if not raw:
                    continue
                try:
                    obj = json.loads(raw)
                except ValueError:
                    continue
                msg = obj.get("message") if isinstance(obj, dict) else None
                container = msg if isinstance(msg, dict) else obj
                if not isinstance(container, dict):
                    continue
                if model is None:
                    mv = container.get("model")
                    if isinstance(mv, str) and mv:
                        model = mv
                usage = container.get("usage")
                if isinstance(usage, dict):
                    blocks += 1
                    for key in totals:
                        val = usage.get(key)
                        if isinstance(val, (int, float)):
                            totals[key] += int(val)
    except OSError:
        pass
    cost = calc_cost(
        totals["input_tokens"],
        totals["output_tokens"],
        totals["cache_read_input_tokens"],
        totals["cache_creation_input_tokens"],
        model,
    )
    return {
        "model": model,
        "usage_blocks": blocks,
        "cost_usd": cost,
        **totals,
    }


# --------------------------------------------------------------------------- #
# Transcript index join (consumes #3726's loom.transcript-index/v1)
# --------------------------------------------------------------------------- #


def build_transcript_map(archive_dir: str | os.PathLike[str] | None) -> dict[str, str]:
    """Map ``agent-<id>`` -> absolute transcript path from #3726 archive indexes.

    Walks every ``index.json`` (schema ``loom.transcript-index/v1``) under
    ``archive_dir`` and resolves each agent's transcript to an absolute path.
    The index lives at ``<...>/<uuid>/index.json`` and the transcript rel-path is
    relative to ``<...>/<uuid>/<session_uuid>/``.
    """
    mapping: dict[str, str] = {}
    if not archive_dir:
        return mapping
    root = pathlib.Path(archive_dir)
    if not root.is_dir():
        return mapping
    for index_path in root.rglob("index.json"):
        try:
            with open(index_path, encoding="utf-8") as fh:
                data = json.load(fh)
        except (OSError, ValueError):
            continue
        if not isinstance(data, dict):
            continue
        if data.get("schema") != "loom.transcript-index/v1":
            continue
        sess_dir = index_path.parent
        uuid = data.get("session_uuid") or ""
        agents = data.get("agents")
        if not isinstance(agents, list):
            continue
        for agent in agents:
            if not isinstance(agent, dict):
                continue
            agent_id = agent.get("agent_id")
            tr = agent.get("transcript")
            if not agent_id or not isinstance(tr, str):
                continue
            abs_path = sess_dir / uuid / tr if uuid else sess_dir / tr
            mapping[agent_id] = str(abs_path)
    return mapping


# --------------------------------------------------------------------------- #
# Harvest / aggregation
# --------------------------------------------------------------------------- #


def read_records(stats_file: str | None) -> list[dict[str, Any]]:
    path = _stats_path(stats_file)
    records: list[dict[str, Any]] = []
    try:
        with open(path, encoding="utf-8") as fh:
            for raw in fh:
                raw = raw.strip()
                if not raw:
                    continue
                try:
                    obj = json.loads(raw)
                except ValueError:
                    continue
                if isinstance(obj, dict):
                    records.append(obj)
    except OSError:
        pass
    return records


_PASS_VERDICTS = {"pass", "approve", "approved", "lgtm", "merged", "merge"}


def _is_pass(verdict: str | None) -> bool:
    return (verdict or "").strip().lower() in _PASS_VERDICTS


def _record_cost(
    record: dict[str, Any], transcript_map: dict[str, str]
) -> tuple[float, str]:
    """Resolve one record's cost + the actual token-fidelity source used.

    Preference order: exact transcript usage (fidelity ``transcript``) > the
    record's own best-effort sweep-aggregate tokens (``sweep-aggregate-log``) >
    nothing (``none``).
    """
    agent_id = record.get("agent_id")
    if agent_id and agent_id in transcript_map:
        summary = sum_transcript_usage(transcript_map[agent_id])
        if summary["usage_blocks"] > 0:
            return summary["cost_usd"], "transcript"
    in_tok = record.get("in_tok")
    out_tok = record.get("out_tok")
    if isinstance(in_tok, (int, float)) or isinstance(out_tok, (int, float)):
        cost = calc_cost(
            int(in_tok or 0), int(out_tok or 0), 0, 0, record.get("model")
        )
        return cost, "sweep-aggregate-log"
    return 0.0, "none"


def harvest(
    stats_file: str | None = None,
    archive_dir: str | os.PathLike[str] | None = None,
) -> dict[str, Any]:
    """Aggregate the stats store into per-arm inputs #3718 needs.

    Per arm: first-attempt Judge-pass rate, mean Doctor cycles, exact total cost
    (via transcript join where available), merge-rate quality floor, and the
    derived per-issue mean cost that feeds the sonnet-first vs opus-first
    inequality.
    """
    records = read_records(stats_file)
    transcript_map = build_transcript_map(archive_dir)

    # Per-issue outcome roll-up, keyed (arm, issue).
    issues: dict[tuple[str, int], dict[str, Any]] = {}
    # Per-arm cost accumulation across every phase record.
    arm_cost: dict[str, float] = {}
    fidelity_counts: dict[str, int] = {"transcript": 0, "sweep-aggregate-log": 0, "none": 0}

    for rec in records:
        arm = rec.get("arm") or "?"
        issue = rec.get("issue")
        phase = (rec.get("phase") or "").lower()
        role = (rec.get("role") or "").lower()

        cost, fidelity = _record_cost(rec, transcript_map)
        arm_cost[arm] = arm_cost.get(arm, 0.0) + cost
        fidelity_counts[fidelity] = fidelity_counts.get(fidelity, 0) + 1

        if issue is None:
            continue
        key = (arm, int(issue))
        state = issues.setdefault(
            key,
            {
                "arm": arm,
                "issue": int(issue),
                "complexity": rec.get("complexity") or "routine",
                "builder_model": None,
                "first_judge_pass": None,
                "doctor_cycles": 0,
                "merged": False,
            },
        )
        if role == "builder" and state["builder_model"] is None:
            state["builder_model"] = rec.get("model")
        if phase == "judge" or role == "judge":
            if int(rec.get("attempt", 1)) == 1 and state["first_judge_pass"] is None:
                state["first_judge_pass"] = _is_pass(rec.get("judge_verdict"))
        if phase == "doctor" or role == "doctor":
            state["doctor_cycles"] += 1
        if phase == "merge":
            state["merged"] = True

    # Roll issues up per arm.
    arms: dict[str, dict[str, Any]] = {}
    for (arm, _issue), state in issues.items():
        a = arms.setdefault(
            arm,
            {
                "arm": arm,
                "model": ARM_MODEL.get(arm, None),
                "n_issues": 0,
                "n_judged": 0,
                "n_first_pass": 0,
                "doctor_cycles_total": 0,
                "n_merged": 0,
            },
        )
        a["n_issues"] += 1
        if state["first_judge_pass"] is not None:
            a["n_judged"] += 1
            if state["first_judge_pass"]:
                a["n_first_pass"] += 1
        a["doctor_cycles_total"] += state["doctor_cycles"]
        if state["merged"]:
            a["n_merged"] += 1

    report_arms = []
    for arm in sorted(arms):
        a = arms[arm]
        n = a["n_issues"]
        judged = a["n_judged"]
        total_cost = arm_cost.get(arm, 0.0)
        report_arms.append(
            {
                "arm": arm,
                "model": a["model"],
                "n_issues": n,
                "first_attempt_pass_rate": (a["n_first_pass"] / judged) if judged else None,
                "mean_doctor_cycles": (a["doctor_cycles_total"] / n) if n else None,
                "merge_rate": (a["n_merged"] / n) if n else None,
                "total_cost_usd": round(total_cost, 6),
                "mean_cost_per_issue_usd": round(total_cost / n, 6) if n else None,
            }
        )

    return {
        "n_records": len(records),
        "token_fidelity_counts": fidelity_counts,
        "arms": report_arms,
    }


def format_harvest_text(report: dict[str, Any]) -> str:
    lines = []
    lines.append("Sweep model-cost experiment — per-arm harvest (#3725 → #3718)")
    lines.append(f"  records: {report['n_records']}")
    fc = report["token_fidelity_counts"]
    lines.append(
        "  token fidelity: "
        f"transcript={fc.get('transcript', 0)} "
        f"aggregate={fc.get('sweep-aggregate-log', 0)} "
        f"none={fc.get('none', 0)}"
    )
    lines.append("")
    header = (
        f"  {'arm':<4} {'model':<8} {'issues':>6} {'1st-pass':>9} "
        f"{'cycles':>7} {'merge':>6} {'cost$':>10} {'$/issue':>10}"
    )
    lines.append(header)
    lines.append("  " + "-" * (len(header) - 2))
    for a in report["arms"]:
        fp = a["first_attempt_pass_rate"]
        mc = a["mean_doctor_cycles"]
        mr = a["merge_rate"]
        cpi = a["mean_cost_per_issue_usd"]
        fp_s = f"{fp:.0%}" if fp is not None else "-"
        mc_s = f"{mc:.2f}" if mc is not None else "-"
        mr_s = f"{mr:.0%}" if mr is not None else "-"
        cpi_s = f"{cpi:.4f}" if cpi is not None else "-"
        lines.append(
            f"  {a['arm']:<4} {str(a['model'] or '-'):<8} {a['n_issues']:>6} "
            f"{fp_s:>9} {mc_s:>7} {mr_s:>6} "
            f"{a['total_cost_usd']:>10.4f} {cpi_s:>10}"
        )
    # Inequality inputs the retune (#3718) consumes.
    by_arm = {a["arm"]: a for a in report["arms"]}
    a_arm = by_arm.get("A")
    b_arm = by_arm.get("B")
    if a_arm and b_arm:
        lines.append("")
        lines.append("  Inequality inputs for #3718 (cost + merge-rate floor):")
        lines.append(
            f"    opus-first  (A): mean ${a_arm['mean_cost_per_issue_usd']} / issue, "
            f"merge-rate {a_arm['merge_rate']}"
        )
        lines.append(
            f"    sonnet-first(B): mean ${b_arm['mean_cost_per_issue_usd']} / issue, "
            f"merge-rate {b_arm['merge_rate']}"
        )
    return "\n".join(lines)


_CANARY_SOURCE_LABEL = {
    "env": "env var LOOM_MODEL_EXPERIMENT_CANARY",
    "sentinel": f"gitignored sentinel {CANARY_SENTINEL}",
}


def format_banner(
    mode: str,
    issue: int,
    arm: str | None,
    model: str | None,
    canary_source: str | None = None,
) -> str:
    bar = "=" * 72
    lines = [bar]
    if mode == "experiment":
        lines.append(f"  LOOM MODEL EXPERIMENT — mode=EXPERIMENT  issue #{issue}")
        lines.append(f"  ARM {arm}  ->  Builder model forced to '{model}'")
        lines.append("  (tier-2.5 complexity bump SUPPRESSED for the forced arm)")
        src = _CANARY_SOURCE_LABEL.get(canary_source or "", "unknown source")
        lines.append(f"  CANARY confirmed via {src}.")
        lines.append("  CANARY-ONLY. Stats -> .loom/stats/sweep-model-stats.jsonl")
    elif mode == "observe":
        lines.append(f"  LOOM MODEL EXPERIMENT — mode=OBSERVE  issue #{issue}")
        lines.append("  Passive measurement only — no model forcing, no arm.")
        if canary_source == "unconfirmed":
            lines.append(
                "  (experiment requested but canary UNCONFIRMED — no uncommitted "
                "signal; downgraded.)"
            )
        lines.append("  Stats -> .loom/stats/sweep-model-stats.jsonl")
    else:
        lines.append(f"  LOOM MODEL EXPERIMENT — mode=OFF  issue #{issue}")
        lines.append("  Instrumentation disabled — zero behavior change.")
    lines.append(bar)
    return "\n".join(lines)


# --------------------------------------------------------------------------- #
# CLI
# --------------------------------------------------------------------------- #


def _cmd_resolve_mode(args: argparse.Namespace) -> int:
    config = load_config(args.config)
    mode, warnings = resolve_effective_mode(dict(os.environ), config)
    for w in warnings:
        _warn(w)
    print(mode)
    return 0


def _cmd_assign_arm(args: argparse.Namespace) -> int:
    arm = assign_arm(args.issue, args.complexity)
    model = arm_model(arm)
    if args.format == "json":
        print(
            json.dumps(
                {
                    "issue": args.issue,
                    "complexity": normalize_complexity(args.complexity),
                    "arm": arm,
                    "model": model,
                }
            )
        )
    else:
        print(f"{arm} {model}")
    return 0


def _cmd_banner(args: argparse.Namespace) -> int:
    env = dict(os.environ)
    config = load_config(args.config)
    raw_mode, _raw_warnings = resolve_raw_mode(env, config)
    mode, warnings = resolve_effective_mode(env, config)
    for w in warnings:
        _warn(w)
    arm = model = None
    canary_source: str | None = None
    if mode == "experiment":
        arm = assign_arm(args.issue, args.complexity)
        model = arm_model(arm)
        _confirmed, canary_source, _cw = evaluate_canary(env)
    elif raw_mode == "experiment":
        # Requested experiment but downgraded to observe — canary unconfirmed.
        canary_source = "unconfirmed"
    print(format_banner(mode, args.issue, arm, model, canary_source))
    return 0


def _cmd_record(args: argparse.Namespace) -> int:
    record = build_record(
        issue=args.issue,
        phase=args.phase,
        role=args.role,
        model=args.model,
        mode=args.mode,
        arm=args.arm,
        attempt=args.attempt,
        complexity=args.complexity,
        judge_verdict=args.verdict,
        cycle_count=args.cycle_count,
        pr=args.pr,
        effort=args.effort,
        agent_id=args.agent_id,
        transcript=args.transcript,
        in_tok=args.in_tok,
        out_tok=args.out_tok,
        token_fidelity=args.token_fidelity,
    )
    append_record(record, args.stats_file)
    if not args.quiet:
        print(json.dumps(record, ensure_ascii=False))
    return 0


def _cmd_harvest(args: argparse.Namespace) -> int:
    archive_dir = args.archive_dir or os.environ.get("LOOM_TRANSCRIPT_ARCHIVE")
    # An env value of ""/off/0/no/disabled is a disable sentinel, not a dir.
    if archive_dir and archive_dir.strip().lower() in {"", "off", "0", "no", "disabled"}:
        archive_dir = None
    report = harvest(args.stats_file, archive_dir)
    if args.format == "json":
        print(json.dumps(report, indent=2))
    else:
        print(format_harvest_text(report))
    return 0


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        prog="sweep-experiment",
        description="Model-cost experiment instrumentation for /loom:sweep (#3725).",
    )
    sub = parser.add_subparsers(dest="command", required=True)

    p_mode = sub.add_parser("resolve-mode", help="Print the effective tri-state mode.")
    p_mode.add_argument("--config", default=None)
    p_mode.set_defaults(func=_cmd_resolve_mode)

    p_arm = sub.add_parser("assign-arm", help="Deterministic per-issue arm + model.")
    p_arm.add_argument("--issue", type=int, required=True)
    p_arm.add_argument("--complexity", default=None)
    p_arm.add_argument("--format", choices=("text", "json"), default="text")
    p_arm.set_defaults(func=_cmd_assign_arm)

    p_ban = sub.add_parser("banner", help="Loud startup banner naming mode + arm.")
    p_ban.add_argument("--issue", type=int, required=True)
    p_ban.add_argument("--complexity", default=None)
    p_ban.add_argument("--config", default=None)
    p_ban.set_defaults(func=_cmd_banner)

    p_rec = sub.add_parser("record", help="Append one JSONL outcome-chain record.")
    p_rec.add_argument("--issue", type=int, required=True)
    p_rec.add_argument("--phase", required=True)
    p_rec.add_argument("--role", required=True)
    p_rec.add_argument("--model", default=None)
    p_rec.add_argument("--mode", default="observe")
    p_rec.add_argument("--arm", default=None)
    p_rec.add_argument("--attempt", type=int, default=1)
    p_rec.add_argument("--complexity", default=None)
    p_rec.add_argument("--verdict", default=None)
    p_rec.add_argument("--cycle-count", dest="cycle_count", type=int, default=0)
    p_rec.add_argument("--pr", type=int, default=None)
    p_rec.add_argument("--effort", default=None)
    p_rec.add_argument("--agent-id", dest="agent_id", default=None)
    p_rec.add_argument("--transcript", default=None)
    p_rec.add_argument("--in-tok", dest="in_tok", type=int, default=None)
    p_rec.add_argument("--out-tok", dest="out_tok", type=int, default=None)
    p_rec.add_argument("--token-fidelity", dest="token_fidelity", default="none")
    p_rec.add_argument("--stats-file", dest="stats_file", default=None)
    p_rec.add_argument("--quiet", action="store_true")
    p_rec.set_defaults(func=_cmd_record)

    p_harv = sub.add_parser("harvest", help="Aggregate the store into #3718 inputs.")
    p_harv.add_argument("--stats-file", dest="stats_file", default=None)
    p_harv.add_argument("--archive-dir", dest="archive_dir", default=None)
    p_harv.add_argument("--format", choices=("text", "json"), default="text")
    p_harv.set_defaults(func=_cmd_harvest)

    return parser


def main(argv: list[str] | None = None) -> int:
    parser = build_parser()
    args = parser.parse_args(argv)
    return args.func(args)


if __name__ == "__main__":
    sys.exit(main())
