"""Tests for the config resolution layer (#4039, Epic #3835 Phase 2)."""

from __future__ import annotations

import json
from pathlib import Path

import pytest

from loom_tools.common.config_resolver import (
    LEGACY_CONFIG_REL,
    LOCAL_CONFIG_REL,
    PRIVATE_DEFAULTS_ENV,
    PROJECT_CONFIG_REL,
    deep_merge,
    get_path,
    private_defaults_path,
    resolve_effective_config,
)


def _write(path: Path, data: dict) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(data))


def _write_raw(path: Path, text: str) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(text)


class TestDeepMerge:
    def test_disjoint_keys_union(self) -> None:
        assert deep_merge({"a": 1}, {"b": 2}) == {"a": 1, "b": 2}

    def test_nested_objects_merge_recursively(self) -> None:
        assert deep_merge({"a": {"x": 1}}, {"a": {"y": 2}}) == {"a": {"x": 1, "y": 2}}

    def test_scalar_overlay_replaces_base(self) -> None:
        assert deep_merge({"a": 1}, {"a": 2}) == {"a": 2}

    def test_explicit_null_overlay_clears_key(self) -> None:
        assert deep_merge({"a": 1}, {"a": None}) == {"a": None}

    def test_empty_overlay_is_noop(self) -> None:
        base = {"a": {"x": 1}, "b": [1, 2]}
        assert deep_merge(base, {}) == base

    def test_array_overlay_replaces_not_concatenates(self) -> None:
        assert deep_merge({"a": [1, 2]}, {"a": [3]}) == {"a": [3]}

    def test_does_not_mutate_inputs(self) -> None:
        base = {"a": {"x": 1}}
        overlay = {"a": {"y": 2}}
        result = deep_merge(base, overlay)
        assert base == {"a": {"x": 1}}
        assert overlay == {"a": {"y": 2}}
        assert result == {"a": {"x": 1, "y": 2}}


class TestPrivateDefaultsPath:
    def test_env_override(self, monkeypatch: pytest.MonkeyPatch) -> None:
        monkeypatch.setenv(PRIVATE_DEFAULTS_ENV, "/tmp/custom-defaults.json")
        assert private_defaults_path() == Path("/tmp/custom-defaults.json")

    def test_empty_env_disables_tier(self, monkeypatch: pytest.MonkeyPatch) -> None:
        monkeypatch.setenv(PRIVATE_DEFAULTS_ENV, "")
        assert private_defaults_path() is None

    def test_unset_env_derives_from_home(self, monkeypatch: pytest.MonkeyPatch) -> None:
        monkeypatch.delenv(PRIVATE_DEFAULTS_ENV, raising=False)
        resolved = private_defaults_path()
        assert resolved is not None
        assert str(resolved).endswith(".local/share/loom/config/defaults.json")


class TestResolveEffectiveConfig:
    def test_only_legacy_tier_present_matches_legacy_content_exactly(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        # Disable the private-defaults tier for deterministic test output.
        monkeypatch.setenv(PRIVATE_DEFAULTS_ENV, "")
        _write(
            tmp_path / LEGACY_CONFIG_REL,
            {"nextAgentNumber": 3, "autonomous": {"perTokenConcurrency": 2}},
        )

        effective = resolve_effective_config(tmp_path)

        assert effective == {"nextAgentNumber": 3, "autonomous": {"perTokenConcurrency": 2}}

    def test_no_files_present_is_empty_dict(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        monkeypatch.setenv(PRIVATE_DEFAULTS_ENV, "")
        assert resolve_effective_config(tmp_path) == {}

    def test_malformed_legacy_config_soft_fails_never_raises(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        monkeypatch.setenv(PRIVATE_DEFAULTS_ENV, "")
        _write_raw(tmp_path / LEGACY_CONFIG_REL, "{not json")
        _write(tmp_path / PROJECT_CONFIG_REL, {"buildGate": {"enabled": True}})

        effective = resolve_effective_config(tmp_path)

        # Legacy tier contributed nothing (malformed), but the project tier
        # still resolved fine -- one bad tier never blocks the others.
        assert effective == {"buildGate": {"enabled": True}}

    def test_non_object_top_level_soft_fails(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        monkeypatch.setenv(PRIVATE_DEFAULTS_ENV, "")
        _write_raw(tmp_path / LEGACY_CONFIG_REL, "[1, 2, 3]")

        assert resolve_effective_config(tmp_path) == {}

    def test_precedence_local_overrides_project_overrides_legacy(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        monkeypatch.setenv(PRIVATE_DEFAULTS_ENV, "")
        _write(tmp_path / LEGACY_CONFIG_REL, {"a": "legacy", "shared": 1})
        _write(tmp_path / PROJECT_CONFIG_REL, {"a": "project", "shared": 2})
        _write(tmp_path / LOCAL_CONFIG_REL, {"a": "local"})

        effective = resolve_effective_config(tmp_path)

        assert effective["a"] == "local"
        assert effective["shared"] == 2

    def test_disjoint_keys_across_tiers_all_present(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        monkeypatch.setenv(PRIVATE_DEFAULTS_ENV, "")
        _write(tmp_path / LEGACY_CONFIG_REL, {"legacyOnly": 1})
        _write(tmp_path / PROJECT_CONFIG_REL, {"projectOnly": 2})
        _write(tmp_path / LOCAL_CONFIG_REL, {"localOnly": 3})

        effective = resolve_effective_config(tmp_path)

        assert effective == {"legacyOnly": 1, "projectOnly": 2, "localOnly": 3}

    def test_nested_autonomous_block_merges_across_tiers(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        monkeypatch.setenv(PRIVATE_DEFAULTS_ENV, "")
        _write(tmp_path / LEGACY_CONFIG_REL, {"autonomous": {"workFinder": {"enabled": True}}})
        _write(tmp_path / LOCAL_CONFIG_REL, {"autonomous": {"perTokenConcurrency": 4}})

        effective = resolve_effective_config(tmp_path)

        assert effective == {
            "autonomous": {"workFinder": {"enabled": True}, "perTokenConcurrency": 4}
        }

    def test_private_defaults_tier_contributes_when_present(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        defaults_file = tmp_path / "defaults.json"
        _write(defaults_file, {"fromDefaults": True, "shared": "default"})
        monkeypatch.setenv(PRIVATE_DEFAULTS_ENV, str(defaults_file))
        _write(tmp_path / LEGACY_CONFIG_REL, {"shared": "legacy"})

        effective = resolve_effective_config(tmp_path)

        assert effective["fromDefaults"] is True
        assert effective["shared"] == "legacy"  # legacy overrides defaults


class TestConformanceFixture:
    """The same fixture tree must resolve identically from Rust, Python,
    and Bash -- see loom-tools/tests/fixtures/config_resolver/README.md.

    Rust: loom-daemon/src/config_resolver.rs
    (test_conformance_fixture_matches_expected_json). Bash:
    defaults/scripts/tests/test-config-resolver.sh.
    """

    FIXTURE_DIR = Path(__file__).parent / "fixtures" / "config_resolver"

    def test_matches_expected_json(self, monkeypatch: pytest.MonkeyPatch) -> None:
        monkeypatch.setenv(PRIVATE_DEFAULTS_ENV, "")

        effective = resolve_effective_config(self.FIXTURE_DIR)

        expected = json.loads((self.FIXTURE_DIR / "expected.json").read_text())

        assert effective == expected, (
            "Python resolver diverged from the cross-language conformance "
            "fixture's expected.json"
        )


class TestGetPath:
    def test_resolves_nested_dotted_key(self) -> None:
        config = {"autonomous": {"workFinder": {"enabled": True}}}
        assert get_path(config, "autonomous.workFinder.enabled") is True

    def test_missing_segment_returns_default(self) -> None:
        config: dict = {"autonomous": {}}
        assert get_path(config, "autonomous.workFinder.enabled") is None
        assert get_path(config, "autonomous.workFinder.enabled", default="x") == "x"

    def test_indexing_through_scalar_returns_default(self) -> None:
        config = {"a": 1}
        assert get_path(config, "a.b") is None

    def test_top_level_key(self) -> None:
        assert get_path({"nextAgentNumber": 3}, "nextAgentNumber") == 3
