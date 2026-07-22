"""Unit tests for loom_tools.sweep_experiment (issue #3725).

Covers the load-bearing deterministic core:
- tri-state mode resolution (env-over-config, malformed -> off) + canary guardrail
- deterministic, resume-safe, stratified arm assignment
- cache-aware pricing + transcript usage summation
- transcript-index join (consuming #3726's loom.transcript-index/v1)
- JSONL append + harvest aggregation into #3718's inequality inputs
"""

from __future__ import annotations

import json
import os

import pytest

from loom_tools import sweep_experiment as se


# --------------------------------------------------------------------------- #
# Tri-state mode resolution
# --------------------------------------------------------------------------- #


def test_default_mode_is_off():
    mode, warns = se.resolve_raw_mode({}, {})
    assert mode == "off"
    assert warns == []


def test_env_over_config():
    # config says experiment, env says observe -> env wins
    config = {"sweep": {"modelExperiment": "experiment"}}
    mode, _ = se.resolve_raw_mode({"LOOM_MODEL_EXPERIMENT": "observe"}, config)
    assert mode == "observe"


def test_config_used_when_env_absent():
    config = {"sweep": {"modelExperiment": "observe"}}
    mode, _ = se.resolve_raw_mode({}, config)
    assert mode == "observe"


def test_empty_env_falls_through_to_config():
    config = {"sweep": {"modelExperiment": "observe"}}
    mode, _ = se.resolve_raw_mode({"LOOM_MODEL_EXPERIMENT": ""}, config)
    assert mode == "observe"


def test_malformed_env_becomes_off_with_warning():
    mode, warns = se.resolve_raw_mode({"LOOM_MODEL_EXPERIMENT": "bogus"}, {})
    assert mode == "off"
    assert warns and "bogus" in warns[0]


def test_malformed_config_becomes_off_with_warning():
    mode, warns = se.resolve_raw_mode({}, {"sweep": {"modelExperiment": 42}})
    assert mode == "off"
    assert warns


def test_case_insensitive_values():
    mode, _ = se.resolve_raw_mode({"LOOM_MODEL_EXPERIMENT": "EXPERIMENT"}, {})
    assert mode == "experiment"


# --------------------------------------------------------------------------- #
# Canary guardrail
# --------------------------------------------------------------------------- #


def test_experiment_downgrades_to_observe_without_canary():
    mode, warns = se.resolve_effective_mode(
        {"LOOM_MODEL_EXPERIMENT": "experiment"}, {}
    )
    assert mode == "observe"
    assert any("NON-CANARY" in w for w in warns)


def test_experiment_honored_with_env_canary():
    mode, _ = se.resolve_effective_mode(
        {"LOOM_MODEL_EXPERIMENT": "experiment", "LOOM_MODEL_EXPERIMENT_CANARY": "1"},
        {},
    )
    assert mode == "experiment"


def test_committed_config_canary_no_longer_confirms():
    # #3731 BEHAVIOR CHANGE: committed sweep.modelExperimentCanary is the exact
    # accidental-production-fire vector (it propagates with a copied config), so it
    # is NO LONGER an accepted confirmation. Experiment now downgrades to observe.
    config = {"sweep": {"modelExperimentCanary": True}}
    mode, warns = se.resolve_effective_mode(
        {"LOOM_MODEL_EXPERIMENT": "experiment"}, config
    )
    assert mode == "observe"
    assert any("NON-CANARY" in w for w in warns)


def test_experiment_honored_with_sentinel(tmp_path):
    # (b) A gitignored sentinel file confirms the canary.
    sentinel = tmp_path / ".loom" / "CANARY"
    sentinel.parent.mkdir(parents=True)
    sentinel.write_text("")
    mode, warns = se.resolve_effective_mode(
        {"LOOM_MODEL_EXPERIMENT": "experiment"},
        {},
        sentinel_path=str(sentinel),
        is_tracked=lambda _p: False,
    )
    assert mode == "experiment"
    assert warns == []


def test_tracked_sentinel_is_refused_and_downgrades(tmp_path):
    # A git-TRACKED sentinel defeats the purpose (it propagates like committed
    # config), so it is refused with a warning and experiment downgrades to observe.
    sentinel = tmp_path / ".loom" / "CANARY"
    sentinel.parent.mkdir(parents=True)
    sentinel.write_text("")
    mode, warns = se.resolve_effective_mode(
        {"LOOM_MODEL_EXPERIMENT": "experiment"},
        {},
        sentinel_path=str(sentinel),
        is_tracked=lambda _p: True,
    )
    assert mode == "observe"
    assert any("TRACKED" in w for w in warns)
    assert any("NON-CANARY" in w for w in warns)


def test_evaluate_canary_reports_source():
    confirmed, source, warns = se.evaluate_canary(
        {"LOOM_MODEL_EXPERIMENT_CANARY": "yes"}
    )
    assert confirmed is True
    assert source == "env"
    assert warns == []


def test_canary_confirmed_ignores_committed_config(tmp_path):
    # Committed config is not consulted; with no uncommitted signal -> unconfirmed.
    missing = tmp_path / ".loom" / "CANARY"
    assert (
        se.canary_confirmed(
            {}, {"sweep": {"modelExperimentCanary": True}}, sentinel_path=str(missing)
        )
        is False
    )


def test_observe_is_safe_anywhere_no_guardrail():
    mode, warns = se.resolve_effective_mode({"LOOM_MODEL_EXPERIMENT": "observe"}, {})
    assert mode == "observe"
    assert warns == []


# --------------------------------------------------------------------------- #
# Arm assignment: determinism, resume-safety, stratification
# --------------------------------------------------------------------------- #


def test_arm_is_deterministic_and_resume_safe():
    # Same issue + complexity always yields the same arm (resume-safe).
    for issue in (1, 42, 3725, 999999):
        for comp in ("complex", "routine", None):
            assert se.assign_arm(issue, comp) == se.assign_arm(issue, comp)


def test_arm_model_mapping():
    assert se.arm_model("A") == "opus"
    assert se.arm_model("B") == "sonnet"
    assert se.arm_model("a") == "opus"


def test_arms_are_only_A_or_B():
    for issue in range(0, 50):
        assert se.assign_arm(issue, "complex") in ("A", "B")
        assert se.assign_arm(issue, "routine") in ("A", "B")


def test_stratification_balances_within_each_stratum():
    # Across a contiguous block of issue numbers, each stratum should split
    # A/B roughly evenly (parity-based) — both arms see a comparable mix.
    n = 200
    for comp in ("complex", "routine"):
        arms = [se.assign_arm(i, comp) for i in range(n)]
        a_count = arms.count("A")
        b_count = arms.count("B")
        assert a_count == b_count == n // 2


def test_complexity_offsets_the_parity():
    # For a fixed issue, complex and routine land on opposite arms (the
    # stratum offset decorrelates the two strata).
    for issue in (0, 1, 2, 100, 101, 3725):
        assert se.assign_arm(issue, "complex") != se.assign_arm(issue, "routine")


def test_unknown_complexity_normalizes_to_routine():
    assert se.assign_arm(10, "weird") == se.assign_arm(10, "routine")
    assert se.assign_arm(10, "") == se.assign_arm(10, None)


# --------------------------------------------------------------------------- #
# Pricing + transcript usage
# --------------------------------------------------------------------------- #


def test_cache_aware_pricing_opus():
    # input 30, output 20, cache_read 2000, cache_write 1000 on opus:
    #  30/1e3*0.015 + 20/1e3*0.075 + 2000/1e3*0.0015 + 1000/1e3*0.01875
    cost = se.calc_cost(30, 20, 2000, 1000, "claude-opus-4-8")
    assert cost == pytest.approx(0.00045 + 0.0015 + 0.003 + 0.01875)


def test_pricing_alias_and_pinned_agree():
    assert se.model_pricing("opus") == se.model_pricing("claude-opus-4-8")
    assert se.model_pricing("sonnet") == se.model_pricing("claude-sonnet-4-6")


def test_sum_transcript_usage(tmp_path):
    tr = tmp_path / "agent-x.jsonl"
    tr.write_text(
        json.dumps(
            {
                "message": {
                    "model": "claude-opus-4-8",
                    "usage": {
                        "input_tokens": 10,
                        "output_tokens": 5,
                        "cache_creation_input_tokens": 1000,
                        "cache_read_input_tokens": 2000,
                    },
                }
            }
        )
        + "\n"
        + json.dumps(
            {"message": {"model": "claude-opus-4-8", "usage": {"input_tokens": 20, "output_tokens": 15}}}
        )
        + "\n"
        + "not json, skipped\n"
    )
    summary = se.sum_transcript_usage(str(tr))
    assert summary["input_tokens"] == 30
    assert summary["output_tokens"] == 20
    assert summary["cache_creation_input_tokens"] == 1000
    assert summary["cache_read_input_tokens"] == 2000
    assert summary["usage_blocks"] == 2
    assert summary["model"] == "claude-opus-4-8"
    assert summary["cost_usd"] == pytest.approx(0.0237)


def test_sum_transcript_usage_missing_file():
    summary = se.sum_transcript_usage("/no/such/file.jsonl")
    assert summary["usage_blocks"] == 0
    assert summary["cost_usd"] == 0.0


# --------------------------------------------------------------------------- #
# Transcript-index join (#3726 loom.transcript-index/v1)
# --------------------------------------------------------------------------- #


def _make_archive(tmp_path):
    sess = tmp_path / "archive" / "myrepo" / "2026-07-22" / "UUID1"
    (sess / "UUID1" / "subagents").mkdir(parents=True)
    (sess / "UUID1" / "subagents" / "agent-bld1.jsonl").write_text(
        json.dumps(
            {
                "message": {
                    "model": "claude-opus-4-8",
                    "usage": {"input_tokens": 10, "output_tokens": 5,
                              "cache_creation_input_tokens": 1000,
                              "cache_read_input_tokens": 2000},
                }
            }
        )
        + "\n"
    )
    (sess / "index.json").write_text(
        json.dumps(
            {
                "schema": "loom.transcript-index/v1",
                "session_uuid": "UUID1",
                "repo": "myrepo",
                "agents": [
                    {"agent_id": "agent-bld1", "role": "loom-builder",
                     "transcript": "subagents/agent-bld1.jsonl"}
                ],
            }
        )
    )
    return tmp_path / "archive"


def test_build_transcript_map(tmp_path):
    archive = _make_archive(tmp_path)
    mapping = se.build_transcript_map(str(archive))
    assert "agent-bld1" in mapping
    assert os.path.isfile(mapping["agent-bld1"])
    summary = se.sum_transcript_usage(mapping["agent-bld1"])
    assert summary["input_tokens"] == 10


def test_build_transcript_map_ignores_wrong_schema(tmp_path):
    d = tmp_path / "arch" / "s"
    d.mkdir(parents=True)
    (d / "index.json").write_text(json.dumps({"schema": "other", "agents": []}))
    assert se.build_transcript_map(str(tmp_path / "arch")) == {}


def test_build_transcript_map_empty_when_no_dir():
    assert se.build_transcript_map(None) == {}
    assert se.build_transcript_map("/no/such/dir") == {}


# --------------------------------------------------------------------------- #
# JSONL append + harvest aggregation
# --------------------------------------------------------------------------- #


def test_append_and_read_records(tmp_path):
    stats = tmp_path / "stats.jsonl"
    se.append_record(se.build_record(issue=1, phase="builder", role="builder", arm="A"), str(stats))
    se.append_record(se.build_record(issue=2, phase="judge", role="judge", arm="B"), str(stats))
    records = se.read_records(str(stats))
    assert len(records) == 2
    assert records[0]["issue"] == 1
    assert records[1]["arm"] == "B"
    # File is created 0600.
    assert oct(os.stat(stats).st_mode)[-3:] == "600"


def test_record_stamps_hard_fields():
    rec = se.build_record(
        issue=3725, phase="judge", role="judge", model="opus", mode="experiment",
        arm="a", attempt=2, complexity="complex", judge_verdict="pass",
        cycle_count=1, agent_id="agent-z",
    )
    assert rec["issue"] == 3725
    assert rec["arm"] == "A"  # uppercased
    assert rec["attempt"] == 2
    assert rec["complexity"] == "complex"
    assert rec["judge_verdict"] == "pass"
    assert rec["agent_id"] == "agent-z"
    assert rec["mode"] == "experiment"
    assert "ts" in rec


def test_harvest_aggregation(tmp_path):
    archive = _make_archive(tmp_path)
    stats = tmp_path / "stats.jsonl"
    sf = str(stats)
    # Arm A: builder(joins transcript) -> judge pass -> merge (first-attempt pass, merged)
    se.append_record(se.build_record(issue=100, phase="builder", role="builder",
                                     model="opus", arm="A", agent_id="agent-bld1"), sf)
    se.append_record(se.build_record(issue=100, phase="judge", role="judge",
                                     arm="A", attempt=1, judge_verdict="pass"), sf)
    se.append_record(se.build_record(issue=100, phase="merge", role="merge", arm="A"), sf)
    # Arm B: builder(best-effort tokens) -> judge changes -> doctor -> judge pass (1 cycle, not merged)
    se.append_record(se.build_record(issue=101, phase="builder", role="builder",
                                     model="sonnet", arm="B", in_tok=5000, out_tok=800,
                                     token_fidelity="sweep-aggregate-log"), sf)
    se.append_record(se.build_record(issue=101, phase="judge", role="judge",
                                     arm="B", attempt=1, judge_verdict="changes"), sf)
    se.append_record(se.build_record(issue=101, phase="doctor", role="doctor",
                                     arm="B", attempt=2), sf)
    se.append_record(se.build_record(issue=101, phase="judge", role="judge",
                                     arm="B", attempt=2, judge_verdict="pass"), sf)

    report = se.harvest(sf, str(archive))
    assert report["n_records"] == 7
    assert report["token_fidelity_counts"]["transcript"] == 1
    assert report["token_fidelity_counts"]["sweep-aggregate-log"] == 1

    arms = {a["arm"]: a for a in report["arms"]}
    a, b = arms["A"], arms["B"]

    assert a["model"] == "opus"
    assert a["first_attempt_pass_rate"] == 1.0
    assert a["mean_doctor_cycles"] == 0.0
    assert a["merge_rate"] == 1.0
    # exact from transcript (single usage block):
    #  10/1e3*0.015 + 5/1e3*0.075 + 2000/1e3*0.0015 + 1000/1e3*0.01875
    assert a["total_cost_usd"] == pytest.approx(0.022275)

    assert b["model"] == "sonnet"
    assert b["first_attempt_pass_rate"] == 0.0
    assert b["mean_doctor_cycles"] == 1.0
    assert b["merge_rate"] == 0.0
    # best-effort cost: 5000/1e3*0.003 + 800/1e3*0.015
    assert b["total_cost_usd"] == pytest.approx(0.027)


def test_harvest_empty_store_does_not_crash(tmp_path):
    report = se.harvest(str(tmp_path / "missing.jsonl"), None)
    assert report["n_records"] == 0
    assert report["arms"] == []


def test_concurrent_appends_are_line_atomic(tmp_path):
    # Simulate interleaved detached writers: many small O_APPEND writes must not
    # corrupt lines. We can't easily fork here, but we can assert every line
    # round-trips as valid JSON after a burst of appends.
    stats = tmp_path / "stats.jsonl"
    for i in range(50):
        se.append_record(se.build_record(issue=i, phase="builder", role="builder",
                                         arm="A" if i % 2 else "B"), str(stats))
    records = se.read_records(str(stats))
    assert len(records) == 50
    assert all(isinstance(r["issue"], int) for r in records)
