# Config resolver conformance fixture (#4039)

A single `repo_root`-shaped tree used by the cross-language conformance test
required by #4039's acceptance criteria: "the same fixture tree resolves to
the same effective config from Rust, Python, and Bash".

Deliberately exercises, in one tree:

- **Disjoint keys** at each tier (`legacyOnly`, `projectOnly`, `worktree.root`)
- **A key overridden across two tiers** (`overriddenByLocal`: set in
  `.loom-project/project.json`, overridden in `.loom-local/local.json`)
- **Nested-object recursive merge** (`autonomous.workFinder` gets fields from
  both the legacy and project tiers; `guards` gets fields from both legacy
  and project)
- **A key set at the lowest and highest tiers with an untouched middle tier**
  (`autonomous.perTokenConcurrency`: legacy=2, local=4, project doesn't
  mention it — local should win)

The private/shared-defaults tier is intentionally left out of this fixture
(every consumer sets `LOOM_CONFIG_DEFAULTS_FILE=""` before resolving it, to
keep the expected output host-independent) — that tier's soft-fail behavior
is already covered by each language's own unit tests.

`expected.json` is the canonical merged result all three resolvers must
produce. Consumers:

- Rust: `loom-daemon/src/config_resolver.rs` (`test_conformance_fixture_*`)
- Python: `loom-tools/tests/test_config_resolver.py`
  (`TestConformanceFixture`)
- Bash: `defaults/scripts/tests/test-config-resolver.sh` (conformance-fixture
  test case)
