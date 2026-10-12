# Issue Sizing: Story Points as the Only Up-Front Signal (Issue #11659)

This is the design doc for #11659. It covers how Loom sizes an issue at
curation time and what reads that size. It lists every reader and writer of
the three current sizing signals, maps each consumer onto story points, answers
the five questions the operator ruling left open, and sets out a phased plan.
Each phase is filed as its own child issue.

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
  It is also absent from robb-studio's `.loom-local/local.json` overlay, and
  no fleet-gitops checkout file names `tierModels`, `optimization`,
  `modelExperiment` or `minComplexity` (checked at `e1fd8f8`, 2026-10-10). So
  every Builder runs on its role default.
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
| `loom-daemon/src/script_helpers/sweep_experiment.rs` | `sweep.modelExperiment` mode; `extract_complexity_marker`; `normalize_complexity` (anything but `complex` folds to `routine`); `assign_arm` | inert-unless-configured (`off` by default; `observe` was used historically on robb-studio) |
| `loom-daemon/src/script_helpers/sweep_experiment_arms.rs` | `sweep.modelExperimentArms` and `…BudgetFraction`; `unit_hash(domain, issue, stratum)` | inert-unless-configured |
| `loom-daemon/src/sweep_registry/model.rs` | `fetch_issue_complexity`, which fetches lazily and only in `experiment` mode; daemon arm assignment | inert-unless-configured |
| `defaults/scripts/sweep-experiment.sh` | `resolve-mode`, `assign-arm --complexity`, and `record --complexity` | inert-unless-configured |
| `defaults/scripts/tests/test-sweep-experiment.sh` | Its test | inert-unless-configured |
| `defaults/docs/model-cost-experiment.md` | Experiment design (#3725, #9122) | inert-unless-configured |
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
`defaults/observability/story-points-calibration-queries.sql` and
`defaults/observability/story-points-calibration-questions.md` (#9434),
and `defaults/docs/runtime-model-trials.md` (a `minComplexity` example).

## 4. Consumer map under the ruling

| Consumer | Reads today | Under the ruling | Phase |
|---|---|---|---|
| Builder model routing (tier 2.5: `resolve-tier-model.sh`, `sweep.tierModels`, `sweep.optimization`, `model_tiers.rs`) | Tier from the body | **Retired.** Routing uses tiers 1–3 plus the escalation ladder. Points do not route (§5e). | P3 |
| Experiment-mode tier-2.5 suppression (`sweep-execution-model.md`) | Tier 2.5 existence | **Retired** with tier 2.5. The forced arm stays above the tier-3 role default. | P3 |
| Model-cost experiment stratification (`assign_arm`, `ExperimentArms::assign`, `in_experiment`) | Binary stratum: `complex` vs. everything else | **Points band** (`small` 1–2, `medium` 3–5, `large` 8–13, `unsized`) on versioned hash domains (§6.4) | P4 |
| Backstop ceiling `minComplexity` (#8555) | Tier; unmarked counts as `routine` | **`minPoints`** (one of `POINTS_VALUES`); unsized counts as 3. `minComplexity` is translated with a warning for one release (§6.5). | P3 |
| Work-finder carrier (`WorkItem::complexity`, `PriorityCandidate.complexity`) | Tier from the listing body | **`story_points`** from the listing's labels, already read by `ready_queue::points_of`. The tier body extraction is removed. | P3 |
| `sweep.outcome.complexity` | Tier from a body fetch | **Deprecated.** Nullable and kept in the schema; no longer filled after P5. `story_points` (label) already exists on `sweep.started` and `sweep.outcome`. | P5 |
| Sweep-outcome write-back (`writeback.rs`, `points_signal.rs`) | Tier and body points marker | Points from the label projection | P5 |
| Jev shadow classifier (#8543) | Asks the same three-way tier question; its only trigger is `resolve-tier-model.sh` | **Retired** with its trigger. A points shadow could be proposed again with evidence. | P5 |
| Calibration (#9434) | `points:*` labels (paused) | The journaled `issue.estimate` fact, extended with `points` and the Curator's model and runtime, joined against landed size | P5, P7 |
| Throughput KPI (LSI, #9466) | Not live | Landed-size index per `RESULTS.md`. Points are ordinal, so the KPI never sums them; it counts per class and reports the LSI. | P7 |
| PR congestion (`forge_pr_congestion.rs`) | Its own label parser | `story_points::classify_points_labels` (the duplicate parser is removed) | P1 |
| Disposition exporter / dispatch telemetry (`ready_queue.rs`, `dispatch.rs` step 2.71) | `points:*` label | Unchanged: it already reads the projection | none |

## 5. Open questions

### 5a. Storage: marker, label, or both

| Rank | Option | Verdict |
|---|---|---|
| **1 (recommended)** | **Both.** The marker is the authored source of truth: written by the Curator in the body or a comment, and parsed by `points_marker.rs`. The `points:N` label is a **projection** that only the gate writes (§5d). Consumers read the label. | Consumers keep zero-cost reads: every listing and dispatch read already carries labels (`story_points.rs`), and `gh issue list --label points:3` still filters server-side (#9431's reason for choosing labels). The marker carries provenance and is validated **before** the label lands. Trust is settled once, at write time, by the gate. Only insiders can apply labels, so label readers need no trust check. |
| 2 | Label only (#9431 as shipped) | Rejected. The ruling requires a marker that can be parsed from comments. A bare label has no record of who assigned it or which model and rubric did. The gate would have nothing to validate before the value lands. |
| 3 | Marker only | Rejected. Every consumer would need a body plus comments fetch. The work-finder listing carries bodies but not comments, so this adds one comments call per candidate per tick. It also loses server-side filtering. |

**Single parsers, one vocabulary.** Markers go through `points_marker.rs` and
labels through `story_points.rs`. Both already share `POINTS_VALUES`.
`forge_pr_congestion::points_from_labels` is folded into
`story_points::classify_points_labels`. We do not add a new parser.
`points_marker.rs` is hardened in place to the discipline of
`issue_estimate::parse`: a marker counts only on a standalone line outside
fenced or indented code, a malformed `loom:points=` value is detected and
rejected instead of being silently skipped, and conflicting duplicates in one
source are rejected. The #4840 anchoring guarantee is kept.

**Labels.** No new labels. The six existing `points:*` labels are reused.
Their descriptions lose "(paused)", and their owner becomes "Curator, via
`loom-daemon curation-complete`".

### 5b. Comment-sourced trust

A marker read from comments can be forged by anyone who can comment. The rule
follows #9548 and `defaults/docs/comment-trust.md`: **outsider text is
content, not control.**

| Rank | Option | Verdict |
|---|---|---|
| **1 (recommended)** | Filter with `comment_trust` (insider `author_association`, fleet Apps, the daemon identity, `forge.trustedCommenters`, the fleet admin roster). The **latest trusted comment** that has a points marker wins. The **body** counts only when the issue's author is trusted, which is the `premise-check` precedent. | Uses the one existing predicate. Comments have per-comment authorship and timestamps; the body has only one author field. That field is the filer, even after a Curator edit, so on public repos a trusted comment is the *more* reliable source. |
| 2 | Accept only markers whose author is the Curator's identity | Rejected. The Curator runs under several identities (attended `gh` user, fleet App, Codex). An operator's correction would not count. Pinning one identity breaks attended curation. |
| 3 | Body first; ignore comments unless the body has no marker | Rejected. A stale body marker from a pre-pause curation would beat a fresh re-curation comment. And for outsider-filed issues, the body is the untrusted source. |

**Precedence and edge cases** (P1 tests each one):

- **Body and comment markers both present:** the latest trusted comment wins.
  The body is the fallback.
- **Several comment markers:** the newest trusted comment by `created_at`
  wins. Inside one comment, two different values are rejected as conflicting.
- **The winning source is malformed or out of vocabulary:** reject (fail
  closed). Never fall back to an older valid marker, because the newest intent
  is what the gate checks.
- **The winning source carries a `loom:complexity=` marker:** reject, naming
  the retired tier and the points syntax. This is exactly the repo#623 case.
  Tier markers in older comments or bodies are ignored.
- **Prose that quotes the syntax** (`<!-- loom:points=<N> -->` in backticks,
  fences or indented code): not a marker, because of the standalone-line and
  fence rules plus the digit-only capture.
- **Closed issue with only a `points:*` label:** consumers read the label
  as-is. Calibration counts it as *label-only, no journaled provenance* and
  leaves it out of fits by default (§6.2).

### 5c. Overlap with `loom:estimate`

`difficulty` (1–5) is a second up-front size scale. `lines` and `files` are
forecasts of the landed change. If all three stay as signals, "points are the
only signal" is false.

| Rank | Option | Verdict |
|---|---|---|
| **1 (recommended)** | **Keep `loom:estimate` as an observational forecast, drop `difficulty`, and journal `points` in the same `issue.estimate` fact.** `lines`, `files` and `doctor_risk` stay as calibration covariates. The docs and `curator-estimate.md` state that **no consumer may route, gate or order on them**. | Points become the only *decision-bearing* size. The live completion path (`curation-complete`, the journal, the OTLP export) is reused rather than duplicated. Calibration gets points with the Curator's model and runtime in one fact. Dropping `difficulty` removes the only scale that competes with points. |
| 2 | Retire `loom:estimate` entirely | Rejected for now. It is live and required, and its `lines`/`files` forecasts are the only pre-registered estimate-vs-actual covariates. Retiring it is a separate decision with its own evidence. |
| 3 | Fold points into the estimate marker (`loom:estimate points=5 lines=…`) | Rejected. It would create a second points parser beside `points_marker.rs`, which breaks the one-source-of-truth rule, and it would make points body-only again. |
| 4 | Derive points from `lines`/`difficulty` | Rejected. The ruling has the Curator *produce* points. `RESULTS.md` measured the rubric directly (ρ≈0.82), and a derived value would be a guess nobody assigned. |

### 5d. Where the gate is enforced

| Rank | Option | Verdict |
|---|---|---|
| **1 (recommended)** | **Inside `loom-daemon curation-complete`, which also applies the labels.** It already fetches the issue and validates before completion. It gains: a comments fetch, points resolution (§5b), validation, then projection of `points:N` (removing any other `points:*`) and application of `loom:curated`. Nothing is journaled or labeled on failure. A `--check` dry-run gives the Curator the same verdict before completion. | The label transition is *done by* the gate, so a skipped or ignored check can no longer produce `loom:curated`. That closes the repo#623 failure mode structurally. It is daemon-native (`.loom/docs/shell-language-policy.md`). `CheckPointsMarker`'s message text is reused as the error output. |
| 2 | A standalone `loom-daemon` subcommand (revive `CheckPointsMarker` with its own fetch) | Rejected as the enforcement point. It is still a self-run check: one more command to forget, which is the original failure. It survives only as the `--check` dry-run of option 1. |
| 3 | A check at the label transition (a `labeled` Action, or a daemon pass on `loom:curated`) | Rejected as primary. It is reactive (the label is already applied and may race Champion), and Actions are off by default. **Kept as a repair backstop:** a daemon pass finds post-cutover `loom:curated` issues with no valid single `points:*` label and sends them back to curation. This fits repair-over-gate, and it is P1's optional slice. |

`require-complexity-marker.sh` becomes a deprecation shim for one release. It
is a `contract` script that older consumer prompts still call. The shim prints
a notice and runs `loom-daemon curation-complete --check`. After that release
it is removed, along with its allowlist, CI and baseline entries.

### 5e. Model routing

| Rank | Option | Verdict |
|---|---|---|
| **1 (recommended)** | **Points do not drive model choice.** Tier 2.5, `sweep.tierModels` and `sweep.optimization` are retired, and the Builder falls back to tiers 1–3 (operator param, `roleConfig.model`, role default) plus the Judge-rejection escalation ladder. | No config uses tier routing today (§2), so the dispatch result is byte-identical. Size is not cost-of-being-wrong, which was the question the tier answered: a tiny security fix is small but risky. Points are ordinal. And #3718's cheap-first retune has no evidence that a smaller model succeeds on low-point work. Retiring also removes one GraphQL body fetch per Builder dispatch. |
| 2 | A points-band model map (e.g. `sweep.pointsModels`) | Rejected for now. It recreates an unconfigured surface with no evidence behind it. It can be proposed again once P4's stratified experiment produces arm outcomes per band. |
| 3 | Map points onto legacy tiers (1–2 → `mechanical`, 3–5 → `routine`, 8–13 → `complex`) and keep `tierModels` | Rejected. It keeps a dead config surface alive and mixes up size and risk under the old names. |

## 6. Migration and back-compat

Counts are from GitHub search on 2026-10-12 (body and comments, so upper
bounds), re-run for this doc:

| Population | Count |
|---|---|
| Open issues matching `"loom:complexity"` | 114 |
| … of which in `loom:issue`/`loom:curated`/`loom:building` (98 in those states) | 73 |
| Open issues with a `points:*` label | 54 |
| Open issues matching `"loom:points"` | 11 |
| `.loom/stats/sweep-model-stats.jsonl` records on robb-studio | 33, all `observe`, all `arm: null` (2026-07-31 to 2026-10-03) |

### 6.1 Tier markers on open issues

**Leave them in place. Never convert them.** A tier measures cost of being
wrong, not size. Mapping it onto points would publish numbers no Curator
assigned, which breaks the "never a guess" rule in `story_points.rs`. Once
P3–P5 land, nothing reads them, so the stale HTML comments are inert. We do not
mass-edit 114 bodies: that would spam notifications and risk clobbering a
concurrent Curator's body write. In-flight issues without points run unsized,
because routing does not depend on points (§5e), and they get points at their
next curation. The gate rejects a tier marker only in the source it is
validating (§5b).

### 6.2 Existing `points:*` labels and `loom:points` markers

**Keep them.** They were assigned under the rubric before the pause. Consumers
read them as before. They have no journaled `issue.estimate` provenance, so
calibration (P7) tags them `pre-restart` and leaves them out of fits by
default. The 11 body markers are valid syntax for the hardened parser. Where
the body author is trusted they still count, until a re-curation comment
replaces them.

### 6.3 `sweep.outcome` records and the stats store

- `sweep.outcome.complexity` stays in the schema as nullable and deprecated.
  It is not filled after P5. Historical records are not rewritten.
  `telemetry-schema.md` records the cutover Loom version so queries can split
  on it. Queries group by `story_points` going forward.
- Stats-store records (`sweep-experiment.sh record`) gain `stratum` and
  `stratum_version: 2`. Records without `stratum_version` read as version 1
  (tier stratum). The harvest and `agent-metrics.sh --model-experiment`
  accept both.

### 6.4 Experiment arm rehashing

Today both hashes take `(issue, normalize_complexity(tier))`, where the
stratum is binary. Rehashing on a points band changes arm assignment, and an
in-flight `experiment`-mode sweep would switch arms when it resumes.

**Cut over by version. Do not rehash silently.**

1. Use new domain separators (`…/v2`) keyed on the points band (`small`,
   `medium`, `large`, `unsized`). `unsized` is its own stratum and is not folded
   into `medium`.
2. **An arm recorded in the checkpoint wins on resume.** The hash applies only
   to a sweep that has never been assigned one. This removes the in-flight
   flip whatever the cutover timing.
3. Keep the built-in A/B parity split for the absent-config case, with the band
   replacing the tier bit, and update the parity test to the new definition.

This is acceptable. `experiment` is canary-only, no verified config enables
it, and the only local stats store holds no assigned arm.

### 6.5 `minComplexity` and the `ComplexityTier` vocabulary

`runtimes.backstopCeiling.minComplexity` (#8555) is strictly validated:
`ceiling.rs` rejects keys outside `KNOWN_KEYS` and any value outside the tier
vocabulary. Dropping the key outright would turn a stale config into a hard
error.

1. **Deprecation release:** accept `minComplexity` with a warning naming
   `minPoints`, and translate it (`mechanical` → no filter, `routine` →
   `minPoints: 3`, `complex` → `minPoints: 8`). That keeps the operator's
   restriction instead of quietly widening metered-tap admission. `minPoints`
   is validated against `POINTS_VALUES`.
2. **Removal release:** `minComplexity` moves to a retired-key list that fails
   validation with a message naming `minPoints`.
3. After P3–P5, delete `COMPLEXITY_TIERS` (`model_tiers.rs`), `ComplexityTier`
   (`ceiling.rs`), and `extract_complexity_marker`/`normalize_complexity`
   (`sweep_experiment.rs`).

## 7. Phased plan

Each phase is one child issue of #11659, with its own tests. Dependencies:
P1 → P2. P1 → P3, P4 and P5. P2 + P5 → P7. P6 comes last.

| Phase | Scope | Key tests |
|---|---|---|
| **P1** Points resolver and gate in `curation-complete` | Harden `points_marker.rs` (§5a). Add comment-source resolution through `comment_trust` with the §5b precedence rule. `curation-complete` validates, projects `points:N` and applies `loom:curated`; it also gets a `--check` dry-run. Fold `forge_pr_congestion::points_from_labels` into `story_points`. Optional: the repair backstop pass. | Rejects `3`, `loom:complexity=3`, `loom:points=x`, `points=21` and conflicting duplicates. Accepts a valid marker in a trusted comment. Ignores an untrusted commenter's marker. Body-vs-comment precedence. Quoted and fenced syntax. Projection replaces a stale `points:*` label. No label or journal entry on failure. |
| **P2** Curator prompt and gate shim | `curator.md`, `defaults/.agents/skills/loom-curator/SKILL.md`, `curator-estimate.md` and its skill mirror: emit `<!-- loom:points=N -->` and drop the tier section. `story-points.md` is un-paused. Update `label-state-machine.md` and the `points:*` label descriptions. `require-complexity-marker.sh` becomes the shim (§5d). Update the allowlist, CI wiring and bypass baseline. | Shim test (exit codes, notice). Prompt-budget and markdown-token-budget gates. Codex mirror sync. |
| **P3** Retire tier 2.5 and move ceiling/work finder to points | Remove tier 2.5 from `sweep-execution-model.md`. `resolve-tier-model.sh` becomes an exit-3 shim, then is removed. Remove the `tierModels`/`optimization` paths in `model_tiers.rs` and the `--tier` flag. Update `model-selection.md`, `configuring-resources.md`, `runtime-adapters.md` and `sweep_md_doc_lint.rs`. Replace `minComplexity` with `minPoints` (§6.5). The work-finder carrier uses `story_points`. | Builder resolution is byte-identical without tier config. `minComplexity` translation plus warning. `minPoints` validation. The carrier reads labels with no body parse. |
| **P4** Experiment stratification v2 | Points-band stratum and versioned hash domains. A checkpoint-recorded arm wins on resume. `stratum_version` in stats records. `fetch_issue_complexity` becomes a label read. Update `sweep-experiment.sh`, `model-cost-experiment.md` and `model-selection-retune.md`. | Weighted frequencies converge within each band. Resume keeps the recorded arm across the cutover. v1 and v2 records both harvest. |
| **P5** Telemetry, write-back, estimate and Jev | `sweep.outcome.complexity` is deprecated (§6.3). Retire the marker read in `complexity_signal.rs`. Write-back reads the label. `issue.estimate` gains `points` and drops `difficulty` (§5c), with the OTLP mapping and `telemetry-schema.md` updated. Retire `jev_tier.rs`, its `daemon_service.rs` hook and its subcommand. | Schema round-trip with old and new records. A missing `difficulty` is accepted. No Jev call path remains. |
| **P6** Legacy cleanup | Re-anchor the dead cross-references (§3.1) to `loom:points`/`loom:capability`. Fix the dangling "Points estimate marker" reference in `daemon-reference.md`. Delete the tier vocabulary (§6.5 step 3) and the shims after their deprecation release. Leave existing markers and labels in place (§6.1, §6.2). | The inventory grep in §3 matches only historical docs. Shell budget shrinks. |
| **P7** Restart calibration (#9434) | Calibration and LSI KPI queries read journaled `issue.estimate` points with provenance, joined against landed size. Pre-restart label-only points are excluded by default. Update `story-points-calibration-queries.sql` and its questions doc. | Query fixtures. Label-only rows excluded. KPI never sums points. |

## 8. Out of scope

- Any code, prompt, label or config change. Those are the child issues.
- Reopening the ruling, including whether to keep the tier as a second axis.
- A points-driven model router (§5e, option 2), until P4 produces evidence.
- Retiring `loom:estimate` entirely (§5c, option 2).

## 9. Sources

- Ruling: [#11659 comment](https://github.com/rjwalters/loom/issues/11659#issuecomment-6131613162). Incident: rjwalters/repo#623.
- Tier: #3702 (two strata), #4238/#4278 (`mechanical`, `tierModels`, `optimization`), #4448 (closed vocabulary), #4827 (work-finder stratum), #4840 (marker anchoring).
- Experiments: #3718 (cheap-first retune), #3725 (model-cost A/B), #3731 (canary confirmation), #9122 (N arms, budget fraction).
- Jev shadow: #8543. Backstop ceiling: #8555.
- Story points: #9429 (epic), #9056 (marker and gate), #9431/#9432 (labels), #9434 (calibration), #9446, #9466 (LSI), #9674 (ready-queue points), #9867 (pause bypass; related).
- Trust: #9548, `defaults/docs/comment-trust.md`. Language policy: `.loom/docs/shell-language-policy.md`, #7810.
- Research: `2AMLogic/loom-experiments:story-points/RESULTS.md`, `REVIEW.md`, `PREREG.md`.
