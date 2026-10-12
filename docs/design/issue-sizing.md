# Issue Sizing: Story Points as the Only Up-Front Signal (Issue #11659)

This is the design doc for #11659. It covers how Loom sizes an issue at
curation time and what reads that size. It lists every reader and writer of
the three current sizing signals, maps each consumer onto story points, answers
the five questions the operator ruling left open, and sets out a three-phase
plan. Each phase is its own child issue.

**Status: proposal for operator sign-off.** This doc changes documentation
only. It changes no code, prompt, label or config, and it does not restart
story points. The child issues do that.

This is a repo-local design doc under `docs/design/`. It is never installed
into consumer repos. The format follows
[`persistence-inventory.md`](persistence-inventory.md).

**Source SHA.** Every citation is against `1a2356921` (`main`, 2026-10-12).
Paths without a prefix are repo-relative.

**Contents**

- [1. Fixed input: the operator ruling](#1-fixed-input-the-operator-ruling)
- [2. What exists today](#2-what-exists-today)
- [3. Inventory](#3-inventory)
- [4. Consumer map under the ruling](#4-consumer-map-under-the-ruling)
- [5. Open questions](#5-open-questions)
- [6. Migration and back-compat](#6-migration-and-back-compat)
- [7. Phased plan](#7-phased-plan)
- [8. Out of scope](#8-out-of-scope)
- [9. Sources](#9-sources)

## 1. Fixed input: the operator ruling

The [operator ruling](https://github.com/rjwalters/loom/issues/11659#issuecomment-6131613162)
(@rjwalters, 2026-10-12) is fixed input. This doc does not reopen it.

1. **Story points are the only up-front sizing signal.** The
   `loom:complexity=mechanical|routine|complex` tier is retired as the
   curation-time classifier. Its gate, `require-complexity-marker.sh`, is
   replaced by a points gate. Story points (Fibonacci 1/2/3/5/8/13, rubric in
   `defaults/docs/story-points.md`, epic #9429) are restarted.
2. **The Curator's points marker is machine-parseable from comment bodies**,
   not only the issue body. The syntax has one source of truth, and the gate
   rejects malformed values, such as the `loom:complexity=3` that reached
   `loom:curated` on rjwalters/repo#623.

Sign-off is from @rjwalters only.

## 2. What exists today

Loom has three curation-time sizing signals. They were designed separately.

| Signal | Origin | Written by | Status |
|---|---|---|---|
| `<!-- loom:complexity=<tier> -->` tier | #3702 (`routine`/`complex`); #4238 via #4278 added `mechanical` and `sweep.tierModels`/`sweep.optimization`; #4448 made the vocabulary closed | Curator, in the issue body | The marker is live. Every routing consumer is inert, because no config sets the keys. |
| Story points: `points:N` labels (#9431, #9432) and the `<!-- loom:points=<N> -->` body marker (#9056) | epic #9429 | Curator | **Paused** since `6dee533` (2026-09-30). 54 open issues still carry a `points:*` label. |
| `<!-- loom:estimate difficulty= lines= files= doctor_risk= -->` | `curator-estimate.md` | Curator, in the issue body | **Live.** `loom-daemon curation-complete` refuses to journal a curation without it. |

What we found:

- **The tier routes nothing.** The `sweep` block is absent from
  `.loom/config.json` and `defaults/config.json` (checked at `1a2356921`).
  Neither robb-studio's `.loom-local/local.json` overlay, its tier-1 host
  defaults, nor any fleet-gitops checkout file sets `tierModels`,
  `optimization` or `minComplexity` (fleet-gitops checked at `e1fd8f8`,
  2026-10-10). So every Builder runs on its role default.
- **The model-cost experiment records, but assigns no arm.**
  `sweep.modelExperiment` is `"observe"` fleet-wide, set in the host defaults
  file `~/.local/share/loom/config/defaults.json` (operator decision #11450,
  2026-10-11). Observe mode appends one stats record per phase, carrying the
  normalized tier as `complexity`. It assigns no arm. `experiment` mode is
  configured nowhere.
- **The tier still costs a forge call.** At every Builder dispatch,
  `resolve-tier-model.sh` fetches the issue body by GraphQL, falling back to
  REST, and then exits 3 because no map is configured.
- **The gate is self-run, and nothing enforces it.** On rjwalters/repo#623 the
  tier marker was posted in a **comment** with the value `3`. `loom:curated`
  followed 5 seconds later. The gate reads the body only and matches
  `[a-z]*`, so it would have exited 1. Either it was never run, or its result
  was ignored.
- **A points gate already exists, unused.** `CheckPointsMarker` (#9056) has
  had no caller since `6dee533`. `points_marker.rs` and `story_points.rs`
  share `POINTS_VALUES`. There is also a **third** points parser,
  `forge_pr_congestion::points_from_labels`. It accepts out-of-vocabulary
  values such as `points:4`, which `story_points.rs` rejects.
- **The research favours points.** `2AMLogic/loom-experiments:story-points/RESULTS.md`
  used a pre-registered holdout (n=133). The v6 rubric predicted landed size at
  ρ≈0.82, against 0.64 for the Curator's tier. It also found that Fibonacci
  classes are **ordinal, not summable** (measured ratios
  1 : 2.3 : 4.0 : 7.1 : 11.1 : 14.5), and it recommended landed size (LSI,
  #9466) as the throughput KPI.

## 3. Inventory

**Method.** The inventory covers every file on `1a2356921` matched by:

```bash
git grep -lE 'loom:complexity|loom:points|points:[0-9]|tierModels|sweep\.optimization|modelExperiment|loom:estimate|require-complexity-marker|resolve-tier-model' \
  | grep -vE '^(WORK_LOG|CHANGELOG)\.md$|issue-sizing\.md'
```

That gives 82 files. Each file gets one status, which is the status of the
mechanism it implements or documents:

- **live**: runs or applies today with the committed configuration.
- **inert-unless-configured**: the code runs or is reachable, but it has no
  effect unless a config key or env var is set. We verified that none is set
  (see §2).
- **dormant**: shipped, with no caller.
- **dead**: a comment, a test fixture or a cited precedent only. Nothing
  reads it as a signal.

A test file takes its subject's status.

### 3.1 Tier marker and gate

| File | Role | Status |
|---|---|---|
| `defaults/scripts/require-complexity-marker.sh` | The tier gate. Reads the body only, anchored `<!-- … -->`, `[a-z]*`, last match wins (#4840). Exits 0/1/2. | live (advisory: the Curator runs it on itself) |
| `defaults/scripts/tests/test-require-complexity-marker.sh` | Its test | live |
| `defaults/scripts/tests/ci-wired.txt` | Registers the tier gate and resolver tests in CI | live |
| `scripts/shell-allowlist.txt` | Lists `require-complexity-marker.sh` and `resolve-tier-model.sh` (`contract`) and their tests (`test`) | live |
| `defaults/forge/call-bypass-baseline.toml` | Baseline entry for the gate's raw `gh` calls | live |
| `defaults/.claude/commands/loom/curator.md` | Writes the tier marker and requires the gate. Says points are paused. | live |
| `defaults/.agents/skills/loom-curator/SKILL.md` | Codex mirror of the Curator prompt | live |
| `defaults/docs/label-state-machine.md` | "Points paused; complexity marker remains required"; the capability-marker syntax is described as mirroring the tier marker | live |
| `defaults/.claude/commands/loom/champion-pr-merge.md` | Mentions the gate's anchoring as precedent | dead |
| `defaults/scripts/classify-ac-verification.sh` | Same | dead |
| `defaults/scripts/extract-capability-markers.sh` | Same | dead |
| `defaults/scripts/check-phantom-labels.sh` | Uses the tier marker as an example of non-label text | dead |
| `loom-daemon/src/premise_check/record.rs` | Mentions the gate's anchoring | dead |
| `loom-daemon/src/capability.rs` | Mentions anchoring; tier marker in a test fixture | dead |
| `loom-daemon/src/host_affinity.rs` | Tier marker in a test fixture | dead |
| `loom-daemon/src/work_finder/recheck_interval.rs` | Mentions the tier marker as precedent | dead |
| `loom-daemon/src/cli/duplicate_scan_fixtures/issue-8505.json` | Issue-body fixture that contains a tier marker | dead |
| `loom-daemon/src/cli/duplicate_scan_fixtures/issue-8561.json` | Same | dead |

### 3.2 Builder model routing (tier 2.5)

| File | Role | Status |
|---|---|---|
| `defaults/scripts/resolve-tier-model.sh` | Tier 2.5 resolver. Fetches the body on every Builder dispatch, then exits 3 when no map is set. | inert-unless-configured |
| `defaults/scripts/tests/test-resolve-tier-model.sh` | Its test | inert-unless-configured |
| `loom-daemon/src/script_helpers/model_tiers.rs` | `COMPLEXITY_TIERS`, `sweep.tierModels`, `OPTIMIZATION_PRESETS`, `resolve_optimization_profile` (`LOOM_SWEEP_OPTIMIZATION`) | inert-unless-configured |
| `loom-daemon/src/script_helpers/mod.rs` | Module index that names the resolver | inert-unless-configured |
| `loom-daemon/src/cli/legacy_script_cmds.rs` | `resolve-model` backs `resolve-tier-model.sh` | inert-unless-configured (its `--tier` path) |
| `loom-daemon/src/main.rs` | `resolve-model --tier` flag; `jev-tier` subcommand | inert-unless-configured |
| `defaults/.claude/commands/loom/sweep-execution-model.md` | Tier 2.5, `sweep.optimization`, experiment-mode suppression, and experiment mode | inert-unless-configured |
| `defaults/.claude/commands/loom/sweep-summary-output.md` | Downgrade text that mentions tier routing | inert-unless-configured |
| `loom-daemon/tests/sweep_md_doc_lint.rs` | CI lint that **requires** `sweep.md` to document the tier marker, `sweep.tierModels` and `resolve-tier-model.sh` | live (P3 must update it) |
| `defaults/docs/model-selection.md` | Tier map and optimization profiles | inert-unless-configured |
| `defaults/docs/configuring-resources.md` | `tierModels`/`optimization`; `minComplexity` example | inert-unless-configured |
| `defaults/docs/runtime-adapters.md` | Tier routing per runtime; `minComplexity` | inert-unless-configured |
| `defaults/scripts/spawn-codex.sh` | Comment on the `tierModels` indirection | dead |
| `defaults/docs/gh-cached.md` | The resolver's cached body read | inert-unless-configured |
| `defaults/docs/github-authentication.md` | The resolver as a wired call site | inert-unless-configured |
| `defaults/scripts/lib/forge-helpers.sh` | Notes that the resolver sources it | inert-unless-configured |
| `defaults/scripts/tests/test-gh-cached.sh` | Tests the resolver's cached read | inert-unless-configured |

### 3.3 Backstop ceiling, work finder and dispatch carrier

| File | Role | Status |
|---|---|---|
| `loom-daemon/src/runtime_preference/ceiling.rs` | `ComplexityTier`; `minComplexity` eligibility (#8555); config validation **rejects** unknown keys (`KNOWN_KEYS`) and unknown values | inert-unless-configured (validation is live) |
| `loom-daemon/src/runtime_preference.rs` | `DispatchContext.complexity` passed to the ceiling | inert-unless-configured |
| `loom-daemon/src/runtime_preference/tests.rs` | Tests a missing marker at the ceiling | inert-unless-configured |
| `loom-daemon/src/work_finder.rs` | `WorkItem::complexity()` reads the tier out of the listing body. It is carried to dispatch for arm stratification and the ceiling. | live (it carries the value, but every consumer is inert) |
| `loom-daemon/src/work_finder/forge.rs` | Carries the listing `body` that holds the stratum | live |
| `loom-daemon/src/work_finder/ordering.rs` | `PriorityCandidate.complexity`. Not an ordering key. | live |
| `loom-daemon/src/work_finder/tests.rs` | Tests the carrier | live |
| `loom-daemon/src/daemon_service.rs` | Jev shadow hook on `resolve-model --tier` | inert-unless-configured (`TYPESAFE_API_KEY`) |
| `loom-daemon/src/jev_tier.rs` | Jev shadow tier classifier (#8543) | inert-unless-configured (`TYPESAFE_API_KEY`; not set on the verified host) |
| `loom-daemon/src/sweep_outcomes.rs` | Field docs: tier routing and the Jev second opinion | inert-unless-configured |

### 3.4 Model-cost experiment

| File | Role | Status |
|---|---|---|
| `loom-daemon/src/script_helpers/sweep_experiment.rs` | `sweep.modelExperiment` mode; `extract_complexity_marker`; `normalize_complexity` (anything but `complex` folds to `routine`); `assign_arm` | live (`observe` via the host defaults, #11450; `assign_arm` runs only in `experiment` mode, configured nowhere) |
| `loom-daemon/src/script_helpers/sweep_experiment_arms.rs` | `sweep.modelExperimentArms` and `…BudgetFraction`; `unit_hash(domain, issue, stratum)` | inert-unless-configured |
| `loom-daemon/src/sweep_registry/model.rs` | `fetch_issue_complexity`, which fetches lazily and only in `experiment` mode; daemon arm assignment | inert-unless-configured |
| `defaults/scripts/sweep-experiment.sh` | `resolve-mode`, `assign-arm --complexity`, and `record --complexity` | live (`record` in `observe`; `assign-arm` inert) |
| `defaults/scripts/tests/test-sweep-experiment.sh` | Its test | live |
| `defaults/docs/model-cost-experiment.md` | Experiment design (#3725, #9122) | live (observe recording) |
| `docs/model-selection-retune.md` | Cheap-first retune (#3718) | inert-unless-configured |
| `defaults/scripts/experiments/README.md` | Cites `sweep.modelExperiment` as flag precedent | dead |
| `defaults/scripts/experiments/judge-fanout-experiment.sh` | Same | dead |
| `docs/research/dynamic-workflows-evaluation.md` | Same | dead |
| `loom-daemon/tests/native_tools.rs` | Fixture config `autonomous.modelExperiment`, a key path nothing reads | dead |

### 3.5 Story points (labels and marker)

| File | Role | Status |
|---|---|---|
| `loom-daemon/src/points_marker.rs` | Marker parser: `POINTS_VALUES`, regex `<!--\s*loom:points=([0-9]+)\s*-->`, last match wins, **body only** | dormant (its only callers are the dormant gate and the opt-in write-back) |
| `loom-daemon/src/cli/points_marker_check.rs` | `CheckPointsMarker`: body on stdin, exits 0/1 | dormant |
| `loom-daemon/src/cli/script_ports.rs` | Registers `CheckPointsMarker` | dormant |
| `loom-daemon/src/story_points.rs` | `points:N` label reader; `PointsLabels` {Absent, One, OutOfVocabulary, Multiple}; never guesses | live (it reads labels the paused Curator no longer writes) |
| `loom-daemon/src/forge_pr_congestion.rs` | `points_from_labels`, a **separate** parser that accepts `points:4` | live |
| `loom-daemon/src/work_finder/ready_queue_tests.rs` | Ready-queue `story_points` for the disposition exporter | live |
| `loom-daemon/src/sweep_registry/guards/issue_snapshot_tests.rs` | `points:3` label fixture | live |
| `loom-daemon/src/observability/captain_gauges/facts_tests.rs` | `points:3` label fixture | live |
| `loom-daemon/src/label_registry/tests.rs` | `points:1` used as a `replaced_by` fixture | live |
| `.github/labels.yml` | 6 `points:*` labels marked "(paused)" | live |
| `defaults/.github/labels.yml` | Same (consumer copy) | live |
| `defaults/labels.json` | Same (JSON form) | live |
| `loom-daemon/src/sweep_registry/outcome_journal/points_signal.rs` | Write-back's marker read | inert-unless-configured (`autonomous.sweepOutcomeWriteback.enabled`) |
| `loom-daemon/src/sweep_registry/outcome_journal/writeback.rs` | Write-back comment with the tier and points | inert-unless-configured |
| `loom-daemon/src/sweep_registry/outcome_journal/writeback_tests.rs` | Its tests | inert-unless-configured |
| `defaults/docs/daemon-reference.md` | Write-back docs. They point to a "Points estimate marker" section in `curator.md`, which has been gone since `6dee533`. Also documents the recheck-interval marker by analogy to the tier marker. | inert-unless-configured |

### 3.6 Telemetry and the `loom:estimate` marker

| File | Role | Status |
|---|---|---|
| `loom-daemon/src/sweep_registry/outcome_journal/complexity_signal.rs` | Fetches the body at every terminal transition. Fills `sweep.outcome.complexity` (closed enum, no fold) and `story_points` from labels. | live |
| `loom-daemon/src/sweep_registry/outcome_journal/complexity_tests.rs` | Its tests | live |
| `loom-daemon/src/telemetry/mod.rs` | `complexity` and `story_points` fields on `sweep.started`/`sweep.outcome` | live |
| `defaults/docs/telemetry-schema.md` | Documents those fields and `issue.estimate` | live |
| `loom-daemon/src/telemetry/kinds/issue_estimate.rs` | `loom:estimate` parser. Counts only standalone lines, outside code fences; rejects duplicates; fails closed on malformed markers. | live |
| `loom-daemon/src/telemetry/kinds/issue_estimate/completion.rs` | Single completion producer and journal | live |
| `loom-daemon/src/telemetry/kinds/issue_estimate/completion_tests.rs` | Its tests | live |
| `loom-daemon/src/cli/curation_complete_tests.rs` | `curation-complete` tests | live |
| `defaults/.claude/commands/loom/curator-estimate.md` | Writes the estimate marker and runs `curation-complete` | live |
| `defaults/.agents/skills/loom-curator-estimate/SKILL.md` | Codex mirror | live |

Outside the grep, but read and changed by the phases below:
`loom-daemon/src/cli/curation_complete.rs` (live; fetches the body by REST and
journals), `loom-daemon/src/sweep_registry/dispatch.rs` (step 2.71, live;
`story_points` read from dispatch labels), `loom-daemon/src/work_finder/ready_queue.rs`
(live; `points_of`), `loom-daemon/src/comment_trust.rs` (live; the trust
predicate), `defaults/docs/story-points.md` (the paused rubric),
and `defaults/docs/runtime-model-trials.md` (a `minComplexity` example).

## 4. Consumer map under the ruling

| Consumer | Reads today | Under the ruling | Phase |
|---|---|---|---|
| Builder model routing (tier 2.5: `resolve-tier-model.sh`, `sweep.tierModels`, `sweep.optimization`, `model_tiers.rs`) | Tier from the body | **Deleted.** Routing uses tiers 1–3 plus the escalation ladder. Points do not route (§5e). | P3 |
| Experiment-mode tier-2.5 suppression (`sweep-execution-model.md`) | Tier 2.5 existence | **Deleted** with tier 2.5. The forced arm stays above the tier-3 role default. | P3 |
| Experiment arm assignment (`assign_arm`, `ExperimentArms::assign`, `in_experiment`) | Binary stratum: `complex` vs. everything else | **No stratum.** The hash takes the issue number only (§6.4). | P3 |
| Observe-mode stats record (`sweep-experiment.sh record --complexity`) | Normalized tier | **Kept** (live, #11450). Records `story_points` from the issue's label instead, empty when unsized. | P3 |
| Backstop ceiling `minComplexity` (#8555) | Tier; unmarked counts as `routine` | **Deleted, no replacement** (§6.5). | P3 |
| Work-finder carrier (`WorkItem::complexity`, `PriorityCandidate.complexity`) | Tier from the listing body | **`story_points`** from the listing's labels, already read by `ready_queue::points_of`. | P3 |
| `sweep.outcome.complexity` and `complexity_signal.rs`'s body fetch | Tier from a body fetch | **Deleted.** `story_points` (label) already exists on `sweep.started` and `sweep.outcome`. | P3 |
| Sweep-outcome write-back (`writeback.rs`, `points_signal.rs`) | Tier and body points marker | Points from the label | P3 |
| Jev shadow classifier (#8543) | Asks the same three-way tier question; its only trigger is `resolve-tier-model.sh` | **Deleted** with its trigger | P3 |
| `issue.estimate` fact | `difficulty`, `lines`, `files`, `doctor_risk` | Journals the resolved `points` (P1); drops `difficulty` (P2) | P1, P2 |
| PR congestion (`forge_pr_congestion.rs`) | Its own label parser | `story_points::classify_points_labels` (the duplicate parser is removed) | P1 |
| Disposition exporter / dispatch telemetry (`ready_queue.rs`, `dispatch.rs` step 2.71) | `points:*` label | Unchanged | none |
| Calibration (#9434) and the LSI throughput KPI (#9466) | `points:*` labels | Out of this plan. Those issues own it; they read the label and the `issue.estimate` points. | — |

## 5. Open questions

### 5a. Storage: marker, label, or both

| Rank | Option | Verdict |
|---|---|---|
| **1 (recommended)** | **Both.** The marker is the authored source of truth: written by the Curator in the body or a comment, and parsed by `points_marker.rs`. The `points:N` label is a **projection** that only the gate writes (§5d). Consumers read the label. | Consumers keep zero-cost reads: every listing and dispatch read already carries labels (`story_points.rs`), and `gh issue list --label points:3` still filters server-side (#9431's reason for choosing labels). The marker is validated **before** the label lands, so trust is settled once, at write time. Only insiders can apply labels, so label readers need no trust check. |
| 2 | Label only (#9431 as shipped) | Rejected. The ruling requires a marker that can be parsed from comments, and the gate would have nothing to validate before the value lands. |
| 3 | Marker only | Rejected. Every consumer would need a body plus comments fetch (the work-finder listing carries no comments), and server-side filtering is lost. |

**Single parsers, one vocabulary.** Markers go through `points_marker.rs` and
labels through `story_points.rs`. Both already share `POINTS_VALUES`.
`forge_pr_congestion::points_from_labels` is folded into
`story_points::classify_points_labels`. We do not add a new parser.
`points_marker.rs` is hardened in place to the discipline of
`issue_estimate::parse`: a marker counts only on a standalone line outside
fenced or indented code, a malformed `loom:points=` value is rejected instead
of being silently skipped, and conflicting duplicates in one source are
rejected. The #4840 anchoring guarantee is kept.

**Labels.** No new labels. The six existing `points:*` labels are reused.
Their descriptions lose "(paused)", and their owner becomes "Curator, via
`loom-daemon curation-complete`".

### 5b. Comment-sourced trust

A marker read from comments can be forged by anyone who can comment. The rule
follows #9548 and `defaults/docs/comment-trust.md`: **outsider text is
content, not control.**

| Rank | Option | Verdict |
|---|---|---|
| **1 (recommended)** | Filter with `comment_trust` (insider `author_association`, fleet Apps, the daemon identity, `forge.trustedCommenters`, the fleet admin roster). The **latest trusted comment** that has a points marker wins. The **body** counts only when the issue's author is trusted, which is the `premise-check` precedent. | Uses the one existing predicate. Comments have per-comment authorship; the body's only author field is the filer, even after a Curator edit, so on public repos a trusted comment is the *more* reliable source. |
| 2 | Accept only markers whose author is the Curator's identity | Rejected. The Curator runs under several identities (attended `gh` user, fleet App, Codex), and an operator's correction would not count. |
| 3 | Body first; ignore comments unless the body has no marker | Rejected. A stale body marker would beat a fresh re-curation comment, and for outsider-filed issues the body is the untrusted source. |

**Precedence and edge cases** (P1 tests each one):

- **Body and comment markers both present:** the latest trusted comment wins.
  The body is the fallback.
- **Several comment markers:** the newest trusted comment by `created_at`
  wins. Inside one comment, two different values are rejected as conflicting.
- **The winning source is malformed or out of vocabulary:** reject (fail
  closed). Never fall back to an older valid marker.
- **The winning source carries a `loom:complexity=` marker:** reject, naming
  the retired tier and the points syntax. This is exactly the repo#623 case.
  Tier markers in older comments or bodies are ignored.
- **Prose that quotes the syntax** (`<!-- loom:points=<N> -->` in backticks,
  fences or indented code): not a marker, because of the standalone-line and
  fence rules plus the digit-only capture.
- **Closed issue with only a `points:*` label:** consumers read the label
  as-is.

### 5c. Overlap with `loom:estimate`

`difficulty` (1–5) is a second up-front size scale. `lines` and `files` are
forecasts of the landed change. If all three stay as signals, "points are the
only signal" is false.

| Rank | Option | Verdict |
|---|---|---|
| **1 (recommended)** | **Keep `loom:estimate` as an observational forecast, drop `difficulty`, and journal `points` in the same `issue.estimate` fact.** `lines`, `files` and `doctor_risk` stay. The docs and `curator-estimate.md` state that **no consumer may route, gate or order on them**. | Points become the only *decision-bearing* size. The live completion path (`curation-complete`, the journal, the OTLP export) is reused, not duplicated. Dropping `difficulty` removes the only scale that competes with points. |
| 2 | Retire `loom:estimate` entirely | Rejected for now. It is live and required, and its `lines`/`files` forecasts are the only pre-registered estimate-vs-actual covariates. |
| 3 | Fold points into the estimate marker (`loom:estimate points=5 lines=…`) | Rejected. A second points parser beside `points_marker.rs`, and points would be body-only again. |
| 4 | Derive points from `lines`/`difficulty` | Rejected. The ruling has the Curator *produce* points, and `RESULTS.md` measured the rubric directly (ρ≈0.82). |

### 5d. Where the gate is enforced

| Rank | Option | Verdict |
|---|---|---|
| **1 (recommended)** | **Inside `loom-daemon curation-complete`, which also applies the labels.** It already fetches the issue and validates before completion. It gains a comments fetch, points resolution (§5b) and validation, then projects `points:N` (removing any other `points:*`) and applies `loom:curated`. Nothing is journaled or labeled on failure. | The label transition is *done by* the gate, so a skipped or ignored check can no longer produce `loom:curated`. That closes the repo#623 failure mode structurally. It is daemon-native (`.loom/docs/shell-language-policy.md`). `CheckPointsMarker`'s message text is reused as the error output. |
| 2 | A standalone `loom-daemon` subcommand (revive `CheckPointsMarker` with its own fetch) | Rejected. It is still a self-run check: one more command to forget, which is the original failure. |
| 3 | A check at the label transition (a `labeled` Action, or a daemon repair pass on `loom:curated`) | Rejected. It is reactive (the label is already applied and may race Champion), Actions are off by default, and option 1 already makes the bad transition impossible. |

`require-complexity-marker.sh` is deleted, with its test and its allowlist, CI
and baseline entries, in the same change that switches the Curator prompts
(P2). Prompts and scripts ship together through resync, so no shim is needed.

### 5e. Model routing

| Rank | Option | Verdict |
|---|---|---|
| **1 (recommended)** | **Points do not drive model choice.** Tier 2.5, `sweep.tierModels` and `sweep.optimization` are deleted, and the Builder falls back to tiers 1–3 (operator param, `roleConfig.model`, role default) plus the Judge-rejection escalation ladder. | No config uses tier routing today (§2), so the dispatch result is byte-identical. Size is not cost-of-being-wrong, which was the question the tier answered: a tiny security fix is small but risky. And #3718's cheap-first retune has no evidence that a smaller model succeeds on low-point work. Deleting also removes one GraphQL body fetch per Builder dispatch. |
| 2 | A points-band model map (e.g. `sweep.pointsModels`) | Rejected. It recreates an unconfigured surface with no evidence behind it. It can be proposed again once observe records grouped by `story_points` show a difference. |
| 3 | Map points onto legacy tiers and keep `tierModels` | Rejected. It keeps a dead config surface alive and mixes up size and risk under the old names. |

## 6. Migration and back-compat

Counts are from GitHub search on 2026-10-12 (body and comments, so upper
bounds):

| Population | Count |
|---|---|
| Open issues matching `"loom:complexity"` | 114 |
| … of which in `loom:issue`/`loom:curated`/`loom:building` | 73 |
| Open issues with a `points:*` label | 54 |
| Open issues matching `"loom:points"` | 11 |
| `.loom/stats/sweep-model-stats.jsonl` records on robb-studio | 33, all `observe`, all `arm: null` (2026-07-31 to 2026-10-03) |

### 6.1 Tier markers on open issues

**Leave them in place. Never convert them.** A tier measures cost of being
wrong, not size, so mapping it onto points would publish numbers no Curator
assigned (the "never a guess" rule in `story_points.rs`). After P3 nothing
reads them. We do not mass-edit 114 bodies: that spams notifications and can
clobber a concurrent Curator's body write. In-flight issues without points run
unsized, because routing does not depend on points (§5e), and get points at
their next curation. The gate rejects a tier marker only in the source it is
validating (§5b).

### 6.2 Existing `points:*` labels and `loom:points` markers

**Keep them.** They were assigned under the rubric before the pause, and
consumers read them as before. The 11 body markers are valid syntax for the
hardened parser. Where the body author is trusted they still count, until a
re-curation comment replaces them.

### 6.3 `sweep.outcome` records and the stats store

- `sweep.outcome.complexity` is deleted from the schema in P3. Historical
  records are not rewritten. `telemetry-schema.md` records the cutover Loom
  version once, so queries can split on it. Queries group by `story_points`.
- Observe-mode stats records are live (#11450), so recording keeps working:
  new records carry `story_points` in place of `complexity`. Old records keep
  their `complexity` field. There is no version field; a reader treats a
  missing field as absent.

### 6.4 Experiment arm assignment

Both arm hashes take `(issue, normalize_complexity(tier))`. Dropping the tier
bit changes which arm an issue would get. That is safe with no transition
step: `experiment` mode is configured nowhere, and `observe` (the live mode)
assigns no arm; all 33 local records have `arm: null`. So there is no
in-flight arm to protect. P3 hashes on the issue number only, keeps the
built-in A/B parity split for the absent-config case, and updates the parity
test.

### 6.5 `minComplexity` and the `ComplexityTier` vocabulary

`runtimes.backstopCeiling.minComplexity` (#8555) is set nowhere (§2). P3
deletes it outright, with `ComplexityTier` (`ceiling.rs`), `COMPLEXITY_TIERS`
(`model_tiers.rs`) and `extract_complexity_marker`/`normalize_complexity`
(`sweep_experiment.rs`). No `minPoints` replaces it: a points-keyed
eligibility filter would contradict §5e. The back-compat behaviour is the
existing one: `ceiling.rs` rejects unknown keys, so a stale config fails
validation loudly, naming the key, rather than silently widening metered-tap
admission.

## 7. Phased plan

Three phases, each one child issue of #11659 with its own tests. P1 comes
first; P2 and P3 each depend on it.

| Phase | Scope | Key tests |
|---|---|---|
| **P1** Points gate (#11663) | Harden `points_marker.rs` (§5a). Resolve the marker from the body and trusted comments through `comment_trust` (§5b). `curation-complete` validates, projects `points:N`, applies `loom:curated` and journals `points` in `issue.estimate`. Fold `forge_pr_congestion::points_from_labels` into `story_points`. | Rejects `3`, `loom:complexity=3`, `loom:points=x`, `points=21` and conflicting duplicates. Accepts a valid marker in a trusted comment; ignores an untrusted commenter's. Body-vs-comment precedence. Quoted and fenced syntax. Projection replaces a stale `points:*` label. No label or journal entry on failure. |
| **P2** Curator switch and gate deletion (#11664) | `curator.md`, `curator-estimate.md` and their skill mirrors emit `<!-- loom:points=N -->` and drop the tier section. Un-pause `story-points.md`. Drop `difficulty` from `loom:estimate` (prompt and parser). Update `label-state-machine.md` and the `points:*` label descriptions. **Delete** `require-complexity-marker.sh`, its test, and its allowlist, CI and baseline entries. | A missing `difficulty` is accepted. Prompt-budget and markdown-token-budget gates. Codex mirror sync. Shell budget shrinks. |
| **P3** Delete the tier consumers (#11665) | Delete `resolve-tier-model.sh`, tier 2.5, `tierModels`/`optimization`, the `--tier` flag, `minComplexity`/`ComplexityTier`, Jev (`jev_tier.rs`, its hook and subcommand), `complexity_signal.rs`'s body fetch and `sweep.outcome.complexity`, and the experiment stratum (§6.4). Observe records carry `story_points` (§6.3). The work-finder carrier and write-back read the label. Update the docs in §3.2–§3.4 and `sweep_md_doc_lint.rs`, re-anchor the dead cross-references (§3.1), and fix the dangling "Points estimate marker" reference in `daemon-reference.md`. Split into a Rust change and a docs change only if the diff is too large to review. | Builder resolution is byte-identical without tier config. A stale `minComplexity` fails validation. Arm parity test on the issue-only hash. Observe record carries `story_points`. No Jev call path remains. The inventory grep in §3 matches only historical docs and existing markers. |

**Child issues.** #11663 (P1), #11664 (P2) and #11665 (P3) are kept and
rescoped to the rows above. #11666, #11667 and #11668 fold into #11665.
#11669 (restart calibration) closes in favour of #9434, which already owns
calibration, with #9466 owning the LSI KPI.

## 8. Out of scope

- Any code, prompt, label or config change. Those are the child issues.
- Reopening the ruling, including whether to keep the tier as a second axis.
- Calibration and the throughput KPI (#9434, #9466).
- A points-driven model router (§5e, option 2).
- Retiring `loom:estimate` entirely (§5c, option 2).

## 9. Sources

- Ruling: [#11659 comment](https://github.com/rjwalters/loom/issues/11659#issuecomment-6131613162). Incident: rjwalters/repo#623. Simplicity standard: #11662.
- Tier: #3702 (two strata), #4238/#4278 (`mechanical`, `tierModels`, `optimization`), #4448 (closed vocabulary), #4827 (work-finder stratum), #4840 (marker anchoring).
- Experiments: #3718 (cheap-first retune), #3725 (model-cost A/B), #3731 (canary confirmation), #9122 (N arms, budget fraction), #11450 (`observe` fleet-wide).
- Jev shadow: #8543. Backstop ceiling: #8555.
- Story points: #9429 (epic), #9056 (marker and gate), #9431/#9432 (labels), #9434 (calibration), #9446, #9466 (LSI), #9674 (ready-queue points), #9867 (pause bypass; related).
- Trust: #9548, `defaults/docs/comment-trust.md`. Language policy: `.loom/docs/shell-language-policy.md`, #7810.
- Research: `2AMLogic/loom-experiments:story-points/RESULTS.md`, `REVIEW.md`, `PREREG.md`.
