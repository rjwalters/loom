# Config Resolution Tiers (#4039)

**Status:** Resolver shipped, additive only. No existing call site reads through
it yet (see "Follow-up" below).
**Tracks:** Epic #3835 ("machine-level Loom install"), Phase 2.
**Related:** #3836 (Phase 1, installer gitignore/`--local` mode, closed), #3979
Phase 2 (cloud-host scale-out — blocked on this landing).

## 1. Problem

Epic #3835's target architecture splits the single, per-repo `.loom/config.json`
into three tiers: private/shared defaults living in the machine-level Loom
checkout, tracked project config living in the consumer repo, and ignored
host-local state. Nothing can migrate onto that layout until something can
*resolve* it — today `.loom/config.json` is read ad hoc at roughly 40 call
sites across Rust, Python, and Bash, each with its own path derivation and
soft-fail behavior.

This issue is **the resolver only**. It introduces the schema and one resolver
per language, with `.loom/config.json` folded into the tier chain as a legacy
tier so every existing repo (which has only that file) resolves identically to
today. It does **not** migrate any of the ~40 call sites, create
`.loom-project/` in any repo, change what the installer writes, or touch
`.loom-local/` runtime-state relocation — those are follow-ups (see epic phases
3–6).

## 2. Tier precedence

Four file tiers, listed lowest to highest precedence (a higher tier overrides a
lower one, key-by-key, via a recursive/deep JSON merge — object values merge
recursively, any other value type replaces the lower tier's value outright,
including an explicit `null` written to override a real value):

| # | Tier | Path (repo-root-relative unless noted) | Tracked? | Introduced |
|---|------|------------------------------------------|----------|------------|
| 1 | **Private/shared defaults** | `$LOOM_CONFIG_DEFAULTS_FILE`, else `~/.local/share/loom/config/defaults.json` | N/A (machine-level, outside any repo) | Epic #3835 Phase 3 (not yet shipped — this tier is a no-op today; the file does not exist on any host) |
| 2 | **Legacy config** | `.loom/config.json` | Tracked (today's status quo) | Pre-existing |
| 3 | **Project config** | `.loom-project/project.json` | Tracked | This issue (schema only; no repo has this file yet) |
| 4 | **Local config** | `.loom-local/local.json` | Ignored (gitignored) | This issue (schema only; no repo has this file yet) |

A missing file, an unreadable file, or malformed JSON at **any** tier
soft-fails to "that tier contributes nothing" (an empty object) — it never
raises, never aborts the daemon/script, and never blocks the other tiers from
being read. This exactly mirrors the existing soft-fail contract already used
by `.loom/config.json` readers (e.g.
`loom-daemon/src/work_finder::read_work_finder_config`,
`loom-daemon/src/main_health_gate::read_build_gate_config`).

**Composition with `env > config > default` (#3813, the `autonomous` block):**
env vars remain the highest-precedence override for every individual knob —
that rule is unchanged. This resolver only replaces what "config" means in that
chain: instead of "the parsed content of `.loom/config.json`", **"config" is
now the merged result of tiers 1–4 above.** So the full effective precedence
for any given knob is:

```
env var  >  (private defaults ⊕ legacy .loom/config.json ⊕ .loom-project/project.json ⊕ .loom-local/local.json)  >  built-in default
                                        ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
                                                     this is "config" — the resolver's output
```

Because tier 2 alone holds every existing repo's real content and tiers 1, 3,
4 are empty everywhere until later epic phases ship, a repo with only
`.loom/config.json` merges to **exactly that file's parsed content** — the
resolver is byte-for-byte behavior-preserving for the status quo.

## 3. `.loom-project/project.json` schema

The epic names five categories of project-specific, tracked config that belong
in `.loom-project/project.json` once a repo migrates off `.loom/config.json`
(Phase 6). Each key below states the tier that **owns** it — i.e., the tier an
operator is expected to set it in once migrated, even though the resolver will
happily read it from any tier that has it (that flexibility is what lets a
partially-migrated repo work correctly).

| Key | Type | Owning tier | Notes |
|-----|------|-------------|-------|
| `terminals` | array | Project (`.loom-project/project.json`) | Enabled agents/terminals for the workspace — repo-specific, meant to be shared with the team via git. |
| `buildGate` | object (`enabled`, `command`, …) | Project | Repo-specific build/verification gate wired into `main_health_gate`. Team-shared, not a host preference. |
| `guards.*` | object (`readOnlyFastPath`, `sqlDdl`, `cloudCli`, `rmScope`, `forceScope`, …) | Project | Guard-hook category toggles are a property of the repo's risk profile, not the host running the agent. |
| `labels` / label maps | object | Project | Any future per-repo label-name overrides (label *set* itself stays defined in `.github/labels.yml`; this is for maps/aliases, not label creation). |
| `autonomous.model` / model overrides | string/object | Project (default) **or** Local (host override) | The *default* model policy for a repo is project-shared; an individual host wanting a different model temporarily (e.g. cost/availability) overrides at `.loom-local/local.json` — local wins per the tier order above. |
| `autonomous.workFinder`, `autonomous.mainHealthGate`, `autonomous.roleRunner`, `autonomous.perTokenConcurrency` | object/number | Project (default) **or** Local (host override) | Same reasoning as model overrides: the *policy* (should this repo run a work finder at all) is project-shared; the *capacity* tuning for one specific host (e.g. `perTokenConcurrency` sized to that host's account pool) is a natural `.loom-local` override. |
| `worktree.root` | string | Local (`.loom-local/local.json`) | Inherently host-specific (an external scratch volume path) — never meant to be shared across machines. |
| `forge.*` (type, gitea url/token) | object | Project (type/url) / Local (token, if not using a repo secret) | `forge.type`/`forge.gitea.url` describe the repo; a raw token belongs host-local, never committed. |

`.loom-local/local.json` has no fixed schema in this issue beyond "same shape
as `.loom-project/project.json`, host-scoped override" — the epic explicitly
defers ".loom-local/ runtime-state relocation" to Phase 3+; this issue only
defines it as a config-merge tier.

## 4. Resolvers (one per language, same precedence)

All three resolvers implement the identical tier list and identical
deep-merge semantics (recursive merge for objects, override for any other
value — this is exactly `jq`'s `*` operator, and the Rust/Python
implementations are written to match it key-for-key):

| Language | Module | Entry points |
|----------|--------|--------------|
| Rust | `loom-daemon/src/config_resolver.rs` | `resolve_effective_config(repo_root: &Path) -> serde_json::Value`, `get_path(&Value, "a.b.c") -> Option<&Value>`, `deep_merge`, `private_defaults_path()` |
| Python | `loom_tools/common/config_resolver.py` | `resolve_effective_config(repo_root: Path) -> dict`, `get_path(config, "a.b.c")`, `deep_merge`, `private_defaults_path()` |
| Bash | `defaults/scripts/lib/config-resolver.sh` | `loom_resolve_config <repo_root>` (echoes merged JSON), `loom_config_get <repo_root> <dotted.path> [default]` |

A cross-language conformance fixture lives at
`loom-tools/tests/fixtures/config_resolver/` (a `repo_root`-shaped tree with a
legacy `.loom/config.json`, a `.loom-project/project.json`, and a
`.loom-local/local.json`, deliberately overlapping some keys across tiers) and
is exercised from all three languages:

- Rust: `loom-daemon/src/config_resolver.rs` `mod tests` (`test_conformance_fixture_*`)
- Python: `loom-tools/tests/test_config_resolver.py` (`TestConformanceFixture`)
- Bash: `defaults/scripts/tests/test-config-resolver.sh`

Each asserts the same known merged output (or a value drawn from it) so a
future change to one resolver's merge semantics can't silently diverge from
the other two.

## 5. Follow-ups (explicitly out of scope here)

- **Call-site migration** — Filed as #4047: swap the ~40 existing
  `.loom/config.json` ad hoc reads over to these resolvers, one call site (or
  cohesive group) at a time, so the resolver's non-null behavior is verified in
  each real reader before the next is touched.
- **Installer/`.loom-project/` creation** — Epic #3835 Phase 6 (per-consumer-repo
  migration runbook): actually writing `.loom-project/project.json` /
  `.loom-local/` into a repo. Nothing in this issue creates those files
  anywhere.
- **`.loom-local/` runtime-state relocation** — Epic #3835 Phase 3+.
- **Deprecating `.loom/config.json`** — not planned; it remains a permanently
  supported legacy tier per this doc.
