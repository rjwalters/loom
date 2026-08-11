# Config Resolution Tiers (#4039)

**Status:** Resolver shipped, additive only, **and now the majority migration
path** — the call-site migration follow-up (#4047) closed 2026-07-28 and most
of #4047's ~40 sites (including `loom-daemon/src/safehouse.rs`, §3's
`safehouse.*` row below) read through it today. A handful of documented
exceptions remain on the legacy single-tier path by design — see
["Follow-ups"](#6-follow-ups-explicitly-out-of-scope-here) below for which
ones and why.
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
| `autonomous.workFinder`, `autonomous.mainHealthGate`, `autonomous.roleRunner` | object/number | Project (default) **or** Local (host override) | Same reasoning as model overrides: the *policy* (should this repo run a work finder at all) is project-shared; the *capacity* tuning for one specific host (e.g. `autonomous.workFinder.maxConcurrent` sized to that host's disk/RAM headroom) is a natural `.loom-local` override. |
| `worktree.root` | string | Local (`.loom-local/local.json`) | Inherently host-specific (an external scratch volume path) — never meant to be shared across machines. |
| `forge.*` (type, gitea url/token) | object | Project (type/url) / Local (token, if not using a repo secret) | `forge.type`/`forge.gitea.url` describe the repo; a raw token belongs host-local, never committed. |
| `safehouse.*` (`enabled`, `socket`, `room`, `persona`) | object | Project (policy) **or** Local (host socket) | Optional daemon-side fleet-comms narration (#3997). Whether a repo narrates at all (`enabled`/`persona`/`room`) is a project-shared policy; the `socket` path (and often `enabled`) is a natural `.loom-local` host override since `safehoused` runs per-host. Resolved by `loom-daemon/src/safehouse.rs` with **env > config > default(disabled)**; see [`.loom/docs/safehouse.md`](../../.loom/docs/safehouse.md). |

`.loom-local/local.json` has no fixed schema in this issue beyond "same shape
as `.loom-project/project.json`, host-scoped override" — the epic explicitly
defers ".loom-local/ runtime-state relocation" to Phase 3+; this issue only
defines it as a config-merge tier.

## 4. Resolvers (one per language, same precedence)

**Updated 2026-08-09 (#5822).** This issue originally shipped a resolver per
language for the three languages Loom had at the time (Rust, Python, Bash).
Since then the Python `loom-tools` package was retired entirely (epic #4081;
`loom_tools/` no longer exists on `main`), and `mcp-loom` (TypeScript) grew its
own resolver when its config-reading call sites were migrated (#4064). The
current set is Rust, Bash, and TypeScript — no Python — and all three
implement the identical tier list and identical deep-merge semantics
(recursive merge for objects, override for any other value — this is exactly
`jq`'s `*` operator):

| Language | Module | Entry points |
|----------|--------|--------------|
| Rust | `loom-daemon/src/config_resolver.rs` | `resolve_effective_config(repo_root: &Path) -> serde_json::Value`, `get_path(&Value, "a.b.c") -> Option<&Value>`, `deep_merge`, `private_defaults_path()` |
| Bash | `defaults/scripts/lib/config-resolver.sh` | `loom_resolve_config <repo_root>` (echoes merged JSON), `loom_config_get <repo_root> <dotted.path> [default]` |
| TypeScript | `mcp-loom/src/shared/config-resolver.ts` | `resolveEffectiveConfig(repoRoot: string): Promise<JsonObject>`, `getPath(config, "a.b.c")`, `deepMerge`, `privateDefaultsPath()` |

A cross-language conformance fixture lives at
`defaults/scripts/tests/fixtures/config_resolver/` (relocated from
`loom-tools/tests/fixtures/config_resolver/` by #4970 when `loom-tools` was
retired) — a `repo_root`-shaped tree with a legacy `.loom/config.json`, a
`.loom-project/project.json`, and a `.loom-local/local.json`, deliberately
overlapping some keys across tiers — and is exercised from all three live
languages:

- Rust: `loom-daemon/src/config_resolver.rs` `mod tests` (`test_conformance_fixture_matches_expected_json`)
- Bash: `defaults/scripts/tests/test-config-resolver.sh`
- TypeScript: `mcp-loom/src/shared/config-resolver.test.ts`

Each asserts the same known merged output (or a value drawn from it) so a
future change to one resolver's merge semantics can't silently diverge from
the other two.

## 5. Runbook: moving one key to `.loom-local/local.json` by hand (#6008)

`loom migrate` (Epic #3835 Phase 6) routes host-local keys into
`.loom-local/local.json` automatically, but it is a **one-time full pass, not an
ongoing per-field fixer**: it short-circuits as soon as a tracked
`.loom-project/project.json` exists. So a host that already carries host-specific
dirt — an uncommitted edit sitting in a tracked `.loom/config.json`, or a value
that was committed into `.loom-project/project.json` before it was recognized as
host-local — has to be moved by hand, once, per key.

**Is the key host-local?** The criterion is not "does it look machine-ish" but
**is the value materially true of one host and false of the others** — a per-host
filesystem path, a socket, or an on/off switch that depends on how that box is
provisioned. `worktree.root` (scratch-disk path) and `safehouse.enabled` /
`safehouse.socket` (is safehoused provisioned here, and where) qualify;
`safehouse.room` / `.rooms` / `.persona` do not — they describe what the repo
does regardless of who runs it, so they stay tracked. The authoritative list the
migration acts on is `MC_HOST_LOCAL_KEYS` in
`scripts/install/migrate-consumer.sh`.

**Move it.** Run from the repo root; `<dotted.path>` is e.g. `safehouse.socket`.
This copies the effective value into the local tier only when that tier does not
already set it (never clobber an operator's own override), then removes it from
the tracked tier:

```bash
KEY='safehouse.socket'

# 1. Read the value you actually want to keep on THIS host (check both tracked
#    tiers; the working-copy edit is usually in .loom/config.json).
jq --arg p "$KEY" 'getpath($p|split("."))' .loom/config.json .loom-project/project.json 2>/dev/null

# 2. Write it into the gitignored host tier, fill-only-if-unset.
#    (If /.loom-local/ is not already in .gitignore, add it FIRST — the installer's
#    managed block normally covers it, but an old repo may predate that.)
mkdir -p .loom-local
[ -f .loom-local/local.json ] || echo '{}' > .loom-local/local.json
jq --arg p "$KEY" --argjson v '"/run/user/1000/safehoused.sock"' \
   '($p|split(".")) as $path
    | if ((getpath($path)) == null or (getpath($path)) == "")
      then setpath($path; $v) else . end' \
   .loom-local/local.json > /tmp/local.json && mv /tmp/local.json .loom-local/local.json

# 3. Remove it from every TRACKED tier so no host re-inherits it.
for f in .loom/config.json .loom-project/project.json; do
  [ -f "$f" ] || continue
  jq --arg p "$KEY" 'delpaths([$p|split(".")])' "$f" > /tmp/cfg.json && mv /tmp/cfg.json "$f"
done

# 4. Verify the merged result is unchanged on this host, then commit step 3 only.
#    (.loom/scripts/lib/config-resolver.sh in an installed consumer repo.)
source defaults/scripts/lib/config-resolver.sh
loom_config_get "$PWD" "$KEY"
git status --short   # .loom-local/ must NOT appear (it is gitignored)
```

`--argjson` (not `--arg`) is load-bearing for booleans: `safehouse.enabled=false`
must land as JSON `false`, not the string `"false"`. Likewise, do not "simplify"
step 2's `getpath(...) == null` test into `// empty` — `false // empty` discards
exactly the value this move exists to preserve.

Once step 3 is committed, every other host resolves the key from its own
`.loom-local/local.json` (or falls back to the built-in default), and the tracked
file stops going dirty on that host — so ff-only syncs
(`loom-daemon-update.sh`) stop aborting on it.

## 6. Follow-ups (explicitly out of scope here)

- **Call-site migration** — Filed as #4047, **closed 2026-07-28**: swapped the
  audited 38 non-TypeScript `.loom/config.json` ad hoc reads (42 across all
  four languages once the TypeScript scope gap was counted) over to these
  resolvers, one call site (or cohesive group) at a time. Decomposed into
  #4058/#4059/#4062/#4063/#4064 (all merged) plus #4060/#4061 (folded into
  #4081, the Python-elimination epic, and closed `not planned` since porting a
  call site to Python was moot once Python itself was on its way out).
  `resolve-model.sh` was one of the folded sites but is migrated anyway as a
  side effect of #4081/#4275 porting its Python backing (`model_tiers`) to
  Rust: the ported `script_helpers::model_tiers` reads through
  `config_resolver::resolve_effective_config`, so the Rust/Python
  `sweep.modelAliases` divergence risk #4047 flagged no longer exists (there
  is no Python side left to diverge from). One documented, permanent exception
  remains on a legacy single-tier read by deliberate design, not oversight:
  `defaults/hooks/guard-destructive-generic.sh`'s `#3687` read-only fast path
  (analyzed in #4063, widened to the project tier in #4262) keeps its own
  bounded (≤2-fork) direct reads of the project (`.loom-project/project.json`)
  and legacy (`.loom/config.json`) tiers only — never `.loom-local/local.json`
  — rather than calling `loom_resolve_config`, because the full tier-aware
  resolver costs up to 6 forks on every guarded Bash-tool call and would
  regress the fast path's whole reason for existing.
- **Installer/`.loom-project/` creation** — Epic #3835 Phase 6 (per-consumer-repo
  migration runbook): actually writing `.loom-project/project.json` /
  `.loom-local/` into a repo. Nothing in this issue creates those files
  anywhere.
- **`.loom-local/` runtime-state relocation** — Epic #3835 Phase 3+.
- **Deprecating `.loom/config.json`** — not planned; it remains a permanently
  supported legacy tier per this doc.
