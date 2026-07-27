"""Unit tests for loom_tools.model_tiers (issue #3982).

The module is the single logical-tier → concrete-model-ID indirection point that
fixes the non-monotonic escalation ladder: the bare `opus` alias resolves to a
previous-generation model on the wire, so every consumer keeps naming `opus` and
exactly one place decides that `opus` means `claude-opus-5`.

Covered:
- the shipped default map pins only the stale `opus` tier; everything else passes
  through unchanged (unknown aliases, current-gen aliases, pinned IDs);
- the `model@effort` suffix is preserved across resolution;
- `.loom/config.json` → `sweep.modelAliases` overrides / drops a pin with no code
  change, and malformed config soft-falls to the shipped default;
- generation extraction (modern IDs, legacy IDs, bare aliases);
- ladder monotonicity — no rung resolves to an older generation than the rung
  below it (the regression guard for the reported bug).
"""

from __future__ import annotations

import json

import pytest

from loom_tools import model_tiers as mt


# --------------------------------------------------------------------------- #
# resolve_model
# --------------------------------------------------------------------------- #


def test_pins_only_stale_opus_tier():
    assert mt.resolve_model("opus") == "claude-opus-5"
    # sonnet/fable are NOT pinned — the CLI resolves them to the current gen.
    assert mt.resolve_model("sonnet") == "sonnet"
    assert mt.resolve_model("fable") == "fable"


def test_pinned_id_and_unknown_alias_pass_through():
    assert mt.resolve_model("claude-sonnet-4-6") == "claude-sonnet-4-6"
    assert mt.resolve_model("mystery-model") == "mystery-model"


@pytest.mark.parametrize("value", [None, "", "  "])
def test_empty_inputs_resolve_to_empty_string(value):
    # Whitespace-only input has no tier to resolve; base.strip() → "".
    assert mt.resolve_model(value) == ""


def test_effort_suffix_preserved():
    assert mt.resolve_model("opus@xhigh") == "claude-opus-5@xhigh"
    assert mt.resolve_model("sonnet@xhigh") == "sonnet@xhigh"
    # A malformed/empty effort still round-trips (never raises).
    assert mt.resolve_model("opus@") == "claude-opus-5@"


def test_case_insensitive_alias():
    assert mt.resolve_model("OPUS") == "claude-opus-5"
    assert mt.resolve_model("Opus") == "claude-opus-5"


# --------------------------------------------------------------------------- #
# config overrides
# --------------------------------------------------------------------------- #


def test_config_override_repoints_a_tier():
    cfg = {"sweep": {"modelAliases": {"opus": "claude-opus-6"}}}
    assert mt.resolve_model("opus", cfg) == "claude-opus-6"


def test_config_override_can_add_a_new_tier():
    cfg = {"sweep": {"modelAliases": {"sonnet": "claude-sonnet-9"}}}
    assert mt.resolve_model("sonnet", cfg) == "claude-sonnet-9"
    # opus still uses the shipped default (override is additive).
    assert mt.resolve_model("opus", cfg) == "claude-opus-5"


def test_config_can_drop_the_pin():
    # Mapping opus back to the bare alias drops the pin (CLI resolves it).
    cfg = {"sweep": {"modelAliases": {"opus": "opus"}}}
    assert mt.resolve_model("opus", cfg) == "opus"


@pytest.mark.parametrize(
    "cfg",
    [
        None,
        {},
        {"sweep": None},
        {"sweep": {}},
        {"sweep": {"modelAliases": None}},
        {"sweep": {"modelAliases": "not-a-dict"}},
        {"sweep": {"modelAliases": {"opus": ""}}},  # blank value dropped
        {"sweep": {"modelAliases": {"opus": 5}}},  # non-string value dropped
        "not-a-dict",
    ],
)
def test_malformed_config_soft_falls_to_shipped_default(cfg):
    assert mt.resolve_model("opus", cfg) == "claude-opus-5"


def test_load_config_missing_and_malformed(tmp_path):
    assert mt.load_config(tmp_path / "nope.json") == {}
    bad = tmp_path / "bad.json"
    bad.write_text("{ not valid json", encoding="utf-8")
    assert mt.load_config(bad) == {}
    good = tmp_path / "config.json"
    good.write_text(json.dumps({"sweep": {"modelAliases": {"opus": "x"}}}), encoding="utf-8")
    assert mt.load_config(good) == {"sweep": {"modelAliases": {"opus": "x"}}}


# --------------------------------------------------------------------------- #
# generation_of
# --------------------------------------------------------------------------- #


@pytest.mark.parametrize(
    "model,expected",
    [
        ("opus", 5),  # resolves through the map first
        ("sonnet", 5),
        ("fable", 5),
        ("haiku", 5),
        ("claude-opus-5", 5),
        ("claude-sonnet-4-6", 4),
        ("claude-opus-5@xhigh", 5),
        ("claude-3-5-sonnet", 3),  # legacy naming
        ("claude-3-opus", 3),
        ("mystery", None),
        ("", None),
        (None, None),
    ],
)
def test_generation_of(model, expected):
    assert mt.generation_of(model) == expected


def test_generation_of_honors_config_override():
    cfg = {"sweep": {"modelAliases": {"opus": "claude-opus-6"}}}
    assert mt.generation_of("opus", cfg) == 6


# --------------------------------------------------------------------------- #
# ladder monotonicity — the regression guard for #3982
# --------------------------------------------------------------------------- #


def test_shipped_ladder_is_monotonic():
    ladder = ["sonnet", "sonnet@xhigh", "opus", "fable"]
    gens = [mt.generation_of(rung) for rung in ladder]
    assert None not in gens, f"a ladder rung has no known generation: {list(zip(ladder, gens))}"
    for lo, hi in zip(gens, gens[1:]):
        assert hi >= lo, f"escalation ladder is non-monotonic: {list(zip(ladder, gens))}"
    # The previously-broken rung is specifically gen-5 now.
    assert mt.generation_of("opus") == 5


def test_probed_alias_table_records_the_bug():
    # The guardrail table documents the CLI reality the fix compensates for:
    # `opus` alone still resolves to a gen-4 model, which is why it is pinned.
    assert mt._ALIAS_GENERATION["opus"] == 4
    assert mt._ALIAS_GENERATION["sonnet"] == 5
    assert mt._ALIAS_GENERATION["fable"] == 5


# --------------------------------------------------------------------------- #
# CLI
# --------------------------------------------------------------------------- #


def test_cli_prints_resolved_id(capsys):
    rc = mt.main(["opus"])
    assert rc == 0
    assert capsys.readouterr().out.strip() == "claude-opus-5"


def test_cli_generation_flag(capsys):
    rc = mt.main(["opus", "--generation"])
    assert rc == 0
    assert capsys.readouterr().out.strip() == "5"


def test_cli_passthrough_and_config(tmp_path, capsys):
    cfg = tmp_path / "config.json"
    cfg.write_text(json.dumps({"sweep": {"modelAliases": {"opus": "claude-opus-6"}}}), encoding="utf-8")
    rc = mt.main(["opus", "--config", str(cfg)])
    assert rc == 0
    assert capsys.readouterr().out.strip() == "claude-opus-6"
