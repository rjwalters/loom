//! Outcome journal (`sweep.outcome` telemetry) + phase-transition sampling.

use super::*;

/// Judge/Doctor signals read off the sweep PR's forge label timeline (Issue
/// #8222) — the source of the record's `judge_verdicts` and `doctor_cycles`.
pub(crate) mod label_timeline;

/// The Curator complexity-tier signal read off the sweep's issue body (Issue
/// #8542) — the source of the record's `complexity`.
pub(crate) mod complexity_signal;

/// One observed lifecycle-phase transition for a live sweep (Issue #4704):
/// the checkpoint phase marker and the instant [`SweepRegistry::reap_once`]
/// first observed it.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct PhaseObservation {
    /// The raw checkpoint phase marker (`"curator-done"`, `"judge-rejected"`,
    /// …) exactly as `sweep-checkpoint.sh` wrote it. Normalized to a lifecycle
    /// phase name by [`phase_label`] only at record-build time, so the stored
    /// observation stays faithful to what was on disk.
    phase: String,
    /// When this transition was first observed (reaper tick time, not the
    /// checkpoint's own `updated_at` — see [`SweepRegistry::sample_phase_transition`]).
    at: DateTime<Utc>,
    /// The `pr_number` the checkpoint carried at this observation, when it
    /// carried one (the sweep skill records it from `builder-done` onward).
    /// Capturing it here is what lets a *successful* sweep's outcome record
    /// name its PR without a forge round trip — the checkpoint itself is
    /// deleted on success, before the record is written.
    pr_number: Option<u32>,
    /// The checkpoint's `(jev_tier, jev_confidence)` pair, when the sweep
    /// skill's Tier-2.5 dispatch step wrote one (Issue #8543) — a shadow-mode
    /// Jev complexity classification, present only when `TYPESAFE_API_KEY`
    /// was set for the run. Captured the same opportunistic, per-tick way as
    /// `pr_number` (see [`SweepRegistry::sample_phase_transition`]), since it
    /// too must survive the checkpoint's deletion on success.
    jev: Option<(String, f64)>,
}

/// Cap on retained [`PhaseObservation`]s per sweep (Issue #4704). A normal
/// lifecycle records at most ~6 (curator/builder/judge/doctor/judge/merge); the
/// Judge↔Doctor cycle is already bounded by the escalation ladder, so this cap
/// is a defensive backstop against an unbounded loop, not a normal-path limit.
/// Observations past the cap are dropped (the earliest are kept — they are the
/// ones the per-phase breakdown is built from).
pub(crate) const MAX_PHASE_OBSERVATIONS: usize = 32;

/// The normalized lifecycle phase name (see [`phase_label`]) whose completion
/// means the sweep merged — the `Success` signal for the durable
/// `sweep.outcome` record (Issue #4704).
pub(crate) const MERGE_PHASE_LABEL: &str = "merge";

/// Normalize a checkpoint phase marker to the lifecycle phase name the
/// telemetry schema documents (Issue #4704): `"curator-done"` → `"curator"`,
/// `"judge-rejected"` → `"judge"`, `"merge-done"` → `"merge"`. A marker with
/// neither known suffix is passed through unchanged rather than being guessed
/// at, so a future `VALID_PHASES` addition degrades to a readable raw label
/// instead of a truncated one.
#[must_use]
pub(crate) fn phase_label(checkpoint_phase: &str) -> &str {
    for suffix in ["-done", "-rejected"] {
        if let Some(head) = checkpoint_phase.strip_suffix(suffix) {
            return head;
        }
    }
    checkpoint_phase
}

/// The distinct model ids in a `sweep.outcome` record's per-model token
/// breakdown (Issue #8056), sorted and deduped — the top-level
/// [`telemetry::SweepOutcomeRecord::models_used`] signal that a sweep ran more
/// than the one model it was dispatched with (the Doctor escalation ladder's
/// opus rescue of a sonnet build, which the record's own `model` field reports
/// as plain `sonnet`).
///
/// Derived from [`telemetry::SweepOutcomeRecord::tokens_by_model`] rather than
/// sampled separately so the two can never disagree, and inherits its
/// "unknown != zero" contract: `None` — never `Some(vec![])` — when no
/// attributable transcript was found.
#[must_use]
pub(crate) fn models_used_from(
    tokens_by_model: Option<&[crate::script_helpers::sweep_experiment::ModelUsageTotals]>,
) -> Option<Vec<String>> {
    let rows = tokens_by_model?;
    let mut models: Vec<String> = rows.iter().map(|r| r.model.clone()).collect();
    models.sort_unstable();
    models.dedup();
    (!models.is_empty()).then_some(models)
}

/// Best-effort extraction of the `jev_tier`/`jev_confidence` pair from a
/// sweep checkpoint JSON file (Issue #8543), mirroring
/// [`read_checkpoint_pr_number`]'s opaque-file, one-shot-read discipline.
/// `None` unless BOTH fields are present and well-typed — a checkpoint
/// carrying a tier with no confidence (or vice versa) is not one this reads,
/// since the pair is always written together by the sweep skill's Tier-2.5
/// step.
#[must_use]
fn read_checkpoint_jev(path: &Path) -> Option<(String, f64)> {
    let s = std::fs::read_to_string(path).ok()?;
    let v: serde_json::Value = serde_json::from_str(&s).ok()?;
    let tier = v.get("jev_tier")?.as_str()?.to_string();
    let confidence = v.get("jev_confidence")?.as_f64()?;
    Some((tier, confidence))
}

impl SweepRegistry {
    /// Best-effort removal of this registry's sweep-journal entry for
    /// `issue` (Issue #3953). Never load-bearing — a missed removal is pruned
    /// the next time anything touches the journal — so failures are logged
    /// at `debug` and swallowed rather than propagated.
    pub(crate) fn journal_remove_best_effort(&self, issue: u32) {
        let journal_path = match self.config.resolve_journal_path() {
            Ok(p) => p,
            Err(e) => {
                log::debug!("sweep_journal: cannot resolve journal path for #{issue}: {e}");
                return;
            }
        };
        if let Err(e) = sweep_journal::remove_sweep_at(
            &journal_path,
            &self.config.workspace_root.display().to_string(),
            issue,
        ) {
            log::debug!("sweep_journal: failed to remove entry for #{issue}: {e}");
        }
    }

    /// Append one line to the durable terminal-outcomes journal (Issue #4644)
    /// for a single-issue sweep's terminal transition. Called at every site
    /// that already emits a terminal `SweepExited`/`SweepCrashed` bus event —
    /// the reaper's dead-child handling in [`reap_once`](Self::reap_once) and
    /// the operator/watchdog-initiated [`finish_cancel`](Self::finish_cancel)
    /// — so the record outlives both the in-memory registry's ~1h GC window
    /// and the in-memory-only event bus.
    ///
    /// Best-effort by contract, matching [`journal_remove_best_effort`](Self::journal_remove_best_effort):
    /// a write failure is logged and swallowed, never allowed to block
    /// reaping. Deliberately independent of the bus emission's own success —
    /// the two are separate side effects of the same terminal transition (see
    /// the `sweep_outcomes` module doc).
    // One parameter per journaled field; they come from four different points
    // in the reaper's terminal branches, so bundling them into a struct would
    // only move the same argument list to the construction site.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn append_outcome_journal(
        &self,
        issue: u32,
        sweep_id: &str,
        outcome: &str,
        exit_code: Option<i32>,
        death_class: Option<String>,
        crash_classification: Option<String>,
        duration_sec: i64,
        result: telemetry::SweepResult,
    ) {
        let path = self.config.resolve_outcomes_journal_path();
        let token_name = self.resolve_token_account(sweep_id, issue);
        // Issue #8447: the API-key pool's equivalent attribution, read off the
        // child's own `# LOOM_LAUNCH` record. Resolved once here and handed to
        // the paired telemetry record below so the two journals can never
        // disagree about which account a sweep ran on.
        // Issue #8556 adds tap-attributed usage accounting for the same launch,
        // resolved in the SAME single pass over the log so the two attributions
        // cannot disagree and a terminal transition still costs exactly one
        // `read_to_string`.
        // Issue #8659: that accounting is now the region folded per tap —
        // `outcome()` is the launch this outcome belongs to (what both journals'
        // one-row fields keep naming), and `breakdown()` is non-empty only when
        // one row cannot represent the region.
        let (credential, tap_region) = self.resolve_launch_attribution(sweep_id, issue);
        // Issue #8056: the single most-specific failure label for this
        // terminal transition, copied into the PAIRED `sweep.outcome`
        // telemetry record below so "real failure vs. <60s spawn death" is
        // answerable without joining this journal by `sweep_id`. Computed
        // before the two `Option`s move into the record below; both survive
        // separately there, exactly as before.
        //
        // Precedence: `death_class` first. It is the pre-flight classifier —
        // it answers "did this sweep ever start work?", which is the question
        // behind #8056's measurement that 1,285 of 1,963 `failure` records
        // were <60s spawn deaths. `crash_classification` fills in for the
        // deaths the pre-flight classifier deliberately declines to label
        // (account/credit exhaustion is excluded from it by design), so in
        // practice the two rarely compete; where they do — a token-selection
        // death that also resolved an empty pool — the pre-flight label is the
        // canonical one for that shape.
        let failure_class = death_class
            .clone()
            .or_else(|| crash_classification.clone())
            .filter(|class| !class.is_empty());
        // Issue #8543: whatever Tier-2.5 sampled for this sweep, or `(None,
        // None)` when `TYPESAFE_API_KEY` was never set / the call never
        // completed — the keyless case this keeps byte-identical to before
        // #8543 (no fields serialized; see `OutcomeRecord::jev_tier`).
        let (jev_tier, jev_confidence) = self
            .sampled_jev(sweep_id)
            .map_or((None, None), |(tier, confidence)| (Some(tier), Some(confidence)));
        let record = sweep_outcomes::OutcomeRecord {
            timestamp: Utc::now(),
            repo: self.config.workspace_root.display().to_string(),
            issue,
            sweep_id: sweep_id.to_string(),
            outcome: outcome.to_string(),
            exit_code,
            death_class,
            crash_classification,
            token_name,
            credential: credential.clone(),
            jev_tier,
            jev_confidence,
            tap_usage: tap_region.outcome().cloned(),
            // Empty — and so absent from the line entirely — whenever
            // `tap_usage` above already accounts for the whole region, which is
            // every single-record region and every re-dispatch/re-exec that
            // re-announced the same tap (Issue #8659).
            tap_usage_all: tap_region.breakdown().to_vec(),
            duration_sec,
        };
        if let Err(e) = sweep_outcomes::append_outcome(&path, &record) {
            log::warn!(
                "sweep_outcomes: failed to append terminal-outcome journal entry for issue \
                 #{issue} ({sweep_id}) at {}: {e} — best-effort, not blocking reap (#4644)",
                path.display()
            );
        }
        // Durable `sweep.outcome` telemetry record (Issue #4704, absorbs
        // #4137) — a SEPARATE journal from the one above (see the
        // `sweep_outcomes` module doc for why), carrying model/config/result
        // detail the #4644 journal was never meant to hold. Independent
        // best-effort side effect: never allowed to block reaping.
        self.append_outcome_telemetry_journal(
            issue,
            sweep_id,
            duration_sec,
            result,
            failure_class,
            credential,
            tap_region,
        );
    }

    /// Both launch attributions off **one** read of the sweep's own log: the
    /// API-key pool account this sweep's native harness ran on (Issue #8447,
    /// the provider-neutral counterpart of
    /// [`resolve_token_account`](Self::resolve_token_account)) and the #8556
    /// tap-attributed usage accounting.
    ///
    /// Unlike the OAuth path there is no dispatch-time capture to prefer: the
    /// `# LOOM_LAUNCH` record is written by the child itself (see
    /// [`crate::launch_record`]), so the sweep's own log is the single source.
    /// One bounded `read_to_string` at the terminal transition, never a forge
    /// call. Both halves are `None` — never a fabricated `"unknown"` account —
    /// for a Claude/legacy-adapter spawn that writes no launch record, an
    /// unreadable/rotated log, or a pre-#8401 binary.
    ///
    /// They are resolved together rather than by two calls because they are two
    /// readings of the same `# LOOM_LAUNCH` record in the same anchored region.
    /// One read keeps the terminal transition's cost exactly where #8447 left it
    /// (one `read_to_string`, never a forge call) and makes it impossible for
    /// the two journals to disagree about which credential a sweep ran on.
    ///
    /// The credential half is returned even when the tap half declines — a
    /// record carrying credential fields but no runtime to key a tap on is
    /// "no tap opinion", never a reason to lose the attribution #8447 already
    /// reported.
    ///
    /// The tap half is the region folded **per tap** (Issue #8659), not one row:
    /// [`RegionAccounting::outcome`](crate::tap_usage::RegionAccounting::outcome)
    /// is the launch this outcome belongs to — carrying that tap's whole share
    /// of the region rather than only its final block, and `None` under exactly
    /// the conditions the single row was `None` under before — while
    /// [`RegionAccounting::breakdown`](crate::tap_usage::RegionAccounting::breakdown)
    /// carries every tap when one row cannot represent the region. Because the
    /// outcome row keeps the last record's own attribution (account included),
    /// the #8447 credential half below is unchanged by the fold.
    pub(crate) fn resolve_launch_attribution(
        &self,
        sweep_id: &str,
        issue: u32,
    ) -> (
        Option<crate::launch_record::CredentialAttribution>,
        crate::tap_usage::RegionAccounting,
    ) {
        let log_path = self
            .entries
            .get(sweep_id)
            .map_or_else(|| self.compute_log_path(issue), |i| i.log_path.clone());
        let Ok(contents) = std::fs::read_to_string(log_path) else {
            return (None, crate::tap_usage::RegionAccounting::default());
        };
        let anchor = format!("sweep_id={sweep_id}");
        let tap_region = crate::tap_usage::account_region_by_tap(&contents, &anchor);
        let credential = tap_region
            .outcome()
            .map(|accounting| accounting.tap.credential.clone())
            .or_else(|| crate::launch_record::parse_launch_credential_after(&contents, &anchor));
        (credential, tap_region)
    }

    /// The runtime/provider/profile this sweep's native harness actually
    /// launched on (Issue #8507), for `sweep.outcome`'s top-level
    /// `runtime`/`provider`/`profile` fields — the runtime-neutral counterpart
    /// of [`resolve_launch_attribution`](Self::resolve_launch_attribution),
    /// reading the SAME `# LOOM_LAUNCH` record for a second, independent set
    /// of keys. `None` under the identical conditions
    /// `resolve_launch_attribution` documents: a Claude/legacy-adapter
    /// spawn writes no launch record at all.
    pub(crate) fn resolve_runtime_attribution(
        &self,
        sweep_id: &str,
        issue: u32,
    ) -> Option<crate::launch_record::RuntimeAttribution> {
        let log_path = self
            .entries
            .get(sweep_id)
            .map_or_else(|| self.compute_log_path(issue), |i| i.log_path.clone());
        let contents = std::fs::read_to_string(log_path).ok()?;
        crate::launch_record::parse_launch_runtime_after(&contents, &format!("sweep_id={sweep_id}"))
    }

    /// The OAuth/token account this sweep actually ran on (Issue #8056), for
    /// both terminal journals' account attribution.
    ///
    /// Three sources, strongest first:
    ///
    /// 1. the live registry entry's `token_name`, when it holds a real account;
    /// 2. a re-parse of the sweep's own per-sweep log, anchored to its
    ///    `sweep_id=<id>` dispatch header — the same parser restart adoption
    ///    uses ([`recover_adopted_token_name`], #4173);
    /// 3. [`UNKNOWN_TOKEN_NAME`], for a genuinely unknowable account.
    ///
    /// Step 2 is what #8056 measured as missing: `config.token_account` was
    /// `"unknown"` on every inspected record, from two different causes that
    /// both leave the on-disk log intact. Either the entry is **gone** (a sweep
    /// reconstructed after a daemon restart, or one already reaped), or the
    /// entry is present but its `token_name` is literally `"unknown"` because
    /// the dispatch-time account-selection poll gave up after
    /// `TOKEN_NAME_CAPTURE_TIMEOUT` (5s) while `spawn-claude.sh` logged its
    /// selection a moment later. The log line is durable in both cases, so one
    /// bounded read at the terminal transition recovers the attribution.
    ///
    /// Cost: a single `read_to_string` of the per-sweep log, and only on the
    /// fallback path — a sweep whose entry already names its account never
    /// touches the filesystem here. Terminal transitions are rare (once per
    /// sweep), so this is not a per-tick cost. Never a forge call.
    pub(crate) fn resolve_token_account(&self, sweep_id: &str, issue: u32) -> String {
        let info = self.entries.get(sweep_id);
        if let Some(name) = info
            .map(|i| i.token_name.clone())
            .filter(|name| !name.is_empty() && name != UNKNOWN_TOKEN_NAME)
        {
            return name;
        }
        let log_path = info.map_or_else(|| self.compute_log_path(issue), |i| i.log_path.clone());
        recover_adopted_token_name(&log_path, sweep_id)
    }

    /// Append one `sweep.outcome` telemetry record (Issue #4704, absorbs
    /// #4137) for `sweep_id`'s terminal transition, wrapped in a
    /// [`telemetry::TelemetryEnvelope`] and appended to the journal
    /// [`sweep_outcomes::append_outcome_telemetry`] manages. Called only from
    /// [`append_outcome_journal`](Self::append_outcome_journal) so every
    /// #4644 terminal-outcome line has a paired telemetry line.
    ///
    /// Best-effort like its sibling: a write failure is logged and swallowed.
    /// Everything except the repo slug + visibility tag is derived from state
    /// this registry already holds (the entry, plus the phase/PR observations
    /// it sampled while the sweep ran), so a terminal transition costs no forge
    /// round trip beyond the one cached `owner/repo` + visibility lookup — and
    /// even that is skipped entirely (never shelling to `gh`) when
    /// `skip_label_flip` is set, matching every other real-forge probe in this
    /// file. A skipped lookup still writes the record, with best-effort
    /// (workspace-path `repo`, private `visibility`) values.
    ///
    /// `failure_class` is the caller's already-computed classification for this
    /// same terminal transition (Issue #8056) — passed in rather than re-derived
    /// so the telemetry record and the sibling `sweep-outcomes.jsonl` record can
    /// never disagree about why a sweep died.
    ///
    /// `credential` is likewise the caller's already-resolved API-key-pool
    /// attribution (Issue #8447), passed in for the same reason: one log read
    /// per terminal transition, and two journals that cannot disagree.
    ///
    /// `tap_region` is the tap-attributed accounting off that same read (Issue
    /// #8556), which is what makes "how much went to the metered backstop vs.
    /// the subscriptions" answerable from this journal — see
    /// [`crate::tap_usage`]. Its outcome row is the launch this outcome belongs
    /// to and is the only one this record's flat `config` map spells out (Issue
    /// #8659); a region one row cannot represent is *flagged* here and broken
    /// out in full on the sibling `sweep-outcomes.jsonl` line.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn append_outcome_telemetry_journal(
        &self,
        issue: u32,
        sweep_id: &str,
        duration_sec: i64,
        result: telemetry::SweepResult,
        failure_class: Option<String>,
        credential: Option<crate::launch_record::CredentialAttribution>,
        tap_region: crate::tap_usage::RegionAccounting,
    ) {
        let info = self.entries.get(sweep_id);
        let model = info.and_then(|i| i.model.clone());
        let effort = info.and_then(|i| i.effort.clone());
        let runtime = info.map(|i| i.runtime.clone());
        // Issue #8056: survives both a missing entry and an entry whose
        // dispatch-time account capture timed out — see `resolve_token_account`.
        let token_name = self.resolve_token_account(sweep_id, issue);
        let latest_phase = info.and_then(|i| i.latest_phase.clone());
        let started_at = info.map(|i| i.started_at);
        // Issue #8507: the launch's own runtime/provider/profile, read off the
        // SAME `# LOOM_LAUNCH` record `credential` above already reads — the
        // ground truth of what actually ran, independent of the dispatch-time
        // `config["runtime"]` string above (which can only ever hold what was
        // *requested*, e.g. absent for an entry reconstructed after a daemon
        // restart). `None` for a Claude/legacy-adapter spawn, same as `credential`.
        let runtime_attribution = self.resolve_runtime_attribution(sweep_id, issue);

        let mut config = BTreeMap::new();
        if let Some(runtime) = runtime {
            config.insert("runtime".to_string(), runtime);
        }
        config.insert("token_account".to_string(), token_name);
        // Issue #8447: the API-key pool's attribution beside the OAuth pool's,
        // in the same free-form map (additive per #4703 — no schema bump).
        // Names only, never key material. Keys are omitted rather than set to
        // a placeholder when the spawn had no launch record or no pooled
        // account, so "not a native pool spawn" stays distinguishable from
        // "a pool spawn whose account is unknown".
        if let Some(credential) = &credential {
            config.insert("credential_source".to_string(), credential.source.clone());
            if let Some(provider) = &credential.provider {
                config.insert("credential_provider".to_string(), provider.clone());
            }
            if let Some(account) = &credential.account {
                config.insert("credential_account".to_string(), account.clone());
            }
        }
        // Issue #8556: the tap this sweep's spend belongs to, plus whatever its
        // native stream reported consuming. Additive in the same free-form map
        // (per #4703 — no schema bump), and `config["tap"]` is what
        // `sweep_outcome_summary`'s `--group-by tap` folds on.
        //
        // Counters are written ONLY when the harness actually reported them: a
        // missing counter means unmeasured, not zero, and a key absent from the
        // map is how that stays distinguishable from a reported `0`. The cost
        // key is spelled `tap_cost_estimate` so no reader can mistake a harness
        // estimate for a measured charge.
        //
        // Issue #8659 fixes the multi-record decision here explicitly rather
        // than by default: `config["tap"]` stays ONE tap — the launch this
        // outcome belongs to, now carrying that tap's whole share of the region
        // — because this map is flat strings and `--group-by tap` puts a record
        // in exactly one bucket, so a second tap could only be spelled here by
        // changing what a grouped row means. A region one row cannot represent
        // is instead FLAGGED (`tap_region_keys`) and broken out per tap on the
        // sibling `sweep-outcomes.jsonl` line (`tap_usage_all`), so a
        // telemetry-only reader can never mistake one tap's counters for the
        // region's total.
        if let Some(accounting) = tap_region.outcome() {
            config.insert("tap".to_string(), accounting.key());
            let usage = &accounting.usage;
            for (key, value) in [
                ("tap_input_tokens", usage.input),
                ("tap_output_tokens", usage.output),
                ("tap_reasoning_tokens", usage.reasoning),
                ("tap_cache_read_tokens", usage.cache_read),
                ("tap_cache_write_tokens", usage.cache_write),
            ] {
                if let Some(value) = value {
                    config.insert(key.to_string(), value.to_string());
                }
            }
            if let Some(cost) = usage.cost_estimate {
                config.insert("tap_cost_estimate".to_string(), cost.to_string());
            }
            if usage.is_measured() {
                config.insert("tap_usage_events".to_string(), usage.usage_events.to_string());
            }
        }
        // Every tap the region named, outcome's first — written only when the
        // single `tap` key above does not account for the whole region, so a
        // record whose region held one tap (every record written before #8659)
        // keeps its exact key set. Present without a `tap` key at all when the
        // region's last record was unattributable but an earlier one was: that
        // is the one shape where the flag is the only telemetry-side signal
        // that the region carried measurable spend.
        if !tap_region.breakdown().is_empty() {
            config.insert(
                "tap_region_keys".to_string(),
                tap_region
                    .breakdown()
                    .iter()
                    .map(crate::tap_usage::TapAccounting::key)
                    .collect::<Vec<_>>()
                    .join(","),
            );
        }
        // Issue #4809: attribute this sweep to the model-cost A/B experiment's
        // arm from its OWN dispatched model — the same inference the #3725
        // harvest already applies to `observe`-mode records
        // (`sweep_experiment::infer_arm_from_model`), reused here so a
        // `experiment`-mode dispatch (which forces exactly an opus/sonnet-family
        // model — see `resolve_autonomous_dispatch_model`) is attributed
        // identically without needing a separate stored "explicit arm" field.
        // Kept in the free-form `config` map (not a new top-level schema field)
        // so this is additive per #4703 with no schema-version bump. Inference
        // stays faithful under #4827's per-issue complexity stratification: the
        // stratum only shifts WHICH arm an issue lands in, never the arm →
        // model mapping this reads back.
        if let Some(arm) =
            crate::script_helpers::sweep_experiment::infer_arm_from_model(model.as_deref())
        {
            config.insert("arm".to_string(), arm.to_string());
        }

        // Per-phase breakdown from the transition history this registry
        // sampled while the sweep was alive (see `sample_phase_transition`).
        // Fallback (history empty — e.g. a sweep reconstructed after a daemon
        // restart, or one that died before the first reaper tick): a single
        // best-effort entry naming the last known phase, attributed the whole
        // duration. Empty when neither is available, never a fabricated phase.
        let phase_durations = started_at
            .map(|started_at| self.phase_durations_for(sweep_id, started_at))
            .unwrap_or_default();
        let phase_durations = if phase_durations.is_empty() {
            latest_phase
                .map(|phase| {
                    vec![telemetry::PhaseDuration {
                        phase: phase_label(&phase).to_string(),
                        duration_sec,
                    }]
                })
                .unwrap_or_default()
        } else {
            phase_durations
        };

        // The sweep's own PR, taken from the checkpoint values sampled while it
        // ran (free), falling back to the checkpoint still on disk for a sweep
        // that died before any sampling tick. Deliberately NOT a
        // `probe_open_linked_pr` GraphQL round trip: that would add one forge
        // call per terminal transition on the reaper's hot path (the exact
        // pressure the fleet rate-limit work exists to relieve), and it answers
        // a weaker question — "does the issue have any open linked PR" rather
        // than "which PR did this sweep produce".
        let pr_number = self.sampled_pr_number(sweep_id).or_else(|| {
            let started_at = started_at?;
            let checkpoint = self
                .config
                .checkpoint_dir()
                .join(format!("issue-{issue}.json"));
            checkpoint_written_by_run(&checkpoint, started_at)
                .then(|| read_checkpoint_pr_number(&checkpoint))
                .flatten()
        });

        let (repo, visibility) = if self.config.skip_label_flip {
            (
                self.config.workspace_root.display().to_string(),
                telemetry::RepoVisibility::Private,
            )
        } else {
            let repo_slug = self
                .resolve_owner_repo()
                .map(|(owner, repo)| format!("{owner}/{repo}"));
            let visibility = repo_slug
                .as_deref()
                .map(telemetry::visibility::derive_visibility)
                .unwrap_or(telemetry::RepoVisibility::Private);
            let repo =
                repo_slug.unwrap_or_else(|| self.config.workspace_root.display().to_string());
            (repo, visibility)
        };

        // Lines added/deleted (Issue #5357): prefer the value this registry
        // already sampled while the sweep was alive (see
        // `sample_phase_transition`'s opportunistic snapshot) — it is the
        // only source that survives a `--merge`-mode sweep's own synchronous
        // `merge-pr.sh` worktree cleanup, which can run and complete inside
        // the sweep's process *before* this reaper-side terminal transition
        // ever fires. Falls back to a live probe for a sweep that died
        // before any sampling tick but whose worktree still exists now (the
        // common Builder-only-dispatch case). Never a forge call either way
        // — both paths are local `git` subprocess invocations. Omitted
        // entirely (not `Some((0, 0))`) when neither source has anything.
        let (lines_added, lines_deleted) = self
            .sampled_loc(sweep_id)
            .or_else(|| self.probe_worktree_loc(issue))
            .map_or((None, None), |(added, deleted)| (Some(added), Some(deleted)));

        // Tokens in/out (Issue #5357): summed straight from the sweep's own
        // Claude Code transcripts, split into input/output axes (raw, NOT
        // cost-weighted — see the schema doc and `tokens_in`'s own doc for
        // why). Bounded to this run's own wall-clock window so a
        // re-dispatched issue's earlier runs are never folded in. Best
        // effort: any failure (no project dir, no matching session, pruned
        // logs) degrades to `None`, never a fabricated `0`.
        let (tokens_in, tokens_out) = started_at
            .and_then(|started_at| {
                let projects_dir = crate::transcript_tokens::claude_projects_dir()?;
                crate::transcript_tokens::sum_sweep_tokens_split(
                    &projects_dir,
                    &self.config.workspace_root,
                    issue,
                    Some((started_at, Utc::now())),
                )
            })
            .map_or((None, None), |(tin, tout)| (Some(tin), Some(tout)));

        // Per-model token breakdown (Issue #6384): same wall-clock window as
        // `tokens_in`/`tokens_out` above, but grouped by
        // `(model, speed, service_tier)` instead of flattened — see
        // `ModelUsageTotals`'s own doc for why a flat sum cannot be priced.
        // Same best-effort/never-fabricated-zero contract.
        //
        // Issue #8507: the SOURCE is runtime-dispatched through
        // `crate::usage_source`. The Claude on-disk JSONL transcripts stay the
        // default for every runtime that seam cannot identify (including the
        // `None` a Claude/legacy spawn yields), so this is byte-identical to
        // the pre-#8507 behavior for every Claude sweep; an `opencode` launch
        // instead reads OpenCode's own session store, scoped to this sweep's
        // directories and the same wall-clock window.
        //
        // Issue #8594: a legacy-adapter runtime that keeps a usage store of its
        // own (Codex) writes no `# LOOM_LAUNCH` record, so its runtime id is
        // recovered from the `# LOOM_RUNTIME_RESOLVED` marker every runtime
        // writes — `sweep_usage_runtime`, which deliberately yields `None` for
        // `claude` and is therefore payload-preserving. The usage SOURCE only:
        // this record's own `runtime`/`provider`/`profile` fields stay
        // launch-record-sourced (`apply_runtime_attribution` below), because
        // the marker carries no provider or profile to publish.
        let usage_runtime = crate::usage_source::sweep_usage_runtime(
            runtime_attribution.as_ref().map(|r| r.runtime.as_str()),
            &self.config.workspace_root,
            issue,
        );
        let tokens_by_model = started_at.and_then(|started_at| {
            crate::usage_source::sweep_tokens_by_model(
                usage_runtime.as_deref(),
                &self.config.workspace_root,
                issue,
                Some((started_at, Utc::now())),
            )
        });

        // Distinct model ids actually observed in this sweep's transcripts
        // (Issue #8056) — see `models_used_from`.
        let models_used = models_used_from(tokens_by_model.as_deref());

        // Judge verdicts + completed Doctor cycles (Issue #8222), read off the
        // forge label timeline of the PR resolved just above — NOT off the
        // sampled phase history the `doctor_cycles` proxy used through #8056.
        // See `label_timeline`'s module doc for the label vocabulary, the
        // which-PR rule (this record's own `pr_number`, i.e. the sweep's
        // latest PR, never a blend across PRs), the 1-based-per-PR `attempt`
        // numbering, and why the read is REST + breaker-gated rather than a
        // bare call on this terminal-transition path.
        //
        // Strictly best-effort: `None` — no PR, breaker suppressed, failed or
        // timed-out fetch, `skip_label_flip` — omits BOTH keys and never
        // blocks or fails the journal append below. Never a fabricated `0`/
        // `[]`: `Some(0)`/`Some([])` mean "the timeline was read and there was
        // nothing", which #8057's summary must be able to tell apart from "not
        // observed".
        let timeline = pr_number.and_then(|pr| self.fetch_timeline_signals(pr));
        let (doctor_cycles, judge_verdicts) = timeline.map_or((None, None), |signals| {
            (Some(signals.doctor_cycles), Some(signals.judge_verdicts))
        });

        // Curator complexity tier (Issue #8542), read off the sweep's own
        // issue body — see `complexity_signal`'s module doc for why this is a
        // separate forge read from the PR timeline above rather than a
        // dispatch-time plumb, and its identical fail-open contract.
        let complexity = self.fetch_complexity_signal(issue);

        let outcome_record = telemetry::SweepOutcomeRecord {
            repo,
            visibility,
            issue,
            sweep_id: sweep_id.to_string(),
            model,
            effort,
            config,
            phase_durations,
            total_duration_sec: duration_sec,
            result,
            pr_number,
            tokens_in,
            tokens_out,
            lines_added,
            lines_deleted,
            tokens_by_model,
            failure_class,
            models_used,
            doctor_cycles,
            judge_verdicts,
            runtime: runtime_attribution.as_ref().map(|r| r.runtime.clone()),
            provider: runtime_attribution
                .as_ref()
                .and_then(|r| r.provider.clone()),
            profile: runtime_attribution.as_ref().and_then(|r| r.profile.clone()),
            complexity,
        };
        let result_name = serde_json::to_value(result)
            .ok()
            .and_then(|v| v.as_str().map(str::to_owned))
            .unwrap_or_else(|| "unknown".into());
        let mut metadata = crate::observability::lifecycle::attributes(&[
            ("loom.repo", &outcome_record.repo),
            ("loom.issue", &issue.to_string()),
            (
                "loom.repo.visibility",
                if visibility == telemetry::RepoVisibility::Public {
                    "public"
                } else {
                    "private"
                },
            ),
        ]);
        if let Some(pr) = pr_number {
            metadata.insert("loom.pr_number".into(), pr.to_string());
        }
        for (key, value) in [
            ("loom.failure_class", outcome_record.failure_class.as_ref()),
            ("loom.configured_model", outcome_record.model.as_ref()),
            ("loom.effort", outcome_record.effort.as_ref()),
            ("loom.runtime", outcome_record.config.get("runtime")),
            ("loom.provider", outcome_record.config.get("provider")),
        ] {
            if let Some(value) = value {
                metadata.insert(key.into(), value.clone());
            }
        }
        if let Some(cycles) = outcome_record.doctor_cycles {
            metadata.insert("loom.doctor_cycles".into(), cycles.to_string());
        }
        // #8908: this execution's exact token usage, joined to its trace.
        crate::observability::runtime_usage::finish_sweep(
            &self.config.workspace_root,
            sweep_id,
            started_at,
            outcome_record.tokens_by_model.as_deref(),
            outcome_record.runtime.as_deref(),
        );
        let trace_context = crate::observability::lifecycle::finish_execution(
            &self.config.workspace_root,
            sweep_id,
            &result_name,
            metadata,
        );
        let mut envelope = telemetry::TelemetryEnvelope::new(
            host_identity(),
            telemetry::TelemetryRecord::SweepOutcome(outcome_record),
        );
        envelope.trace_context = trace_context;
        let path = self.config.resolve_outcome_telemetry_path();
        if let Err(e) = sweep_outcomes::append_outcome_telemetry(&path, &envelope) {
            log::warn!(
                "sweep_outcomes: failed to append sweep.outcome telemetry record for issue \
                 #{issue} ({sweep_id}) at {}: {e} — best-effort, not blocking reap (#4704)",
                path.display()
            );
        }
    }

    // ------------------------------------------------------------------------
    // Lifecycle-phase transition sampling (Issue #4704)
    // ------------------------------------------------------------------------

    /// Record a lifecycle-phase transition for a **live** sweep when the
    /// on-disk checkpoint has advanced since the last observation (Issue
    /// #4704). Called once per live entry per [`reap_once`](Self::reap_once)
    /// tick — which includes the read-path reaps (`ListSweeps` /
    /// `GetSweepStatus` → [`reap_liveness`](Self::reap_liveness)), so the
    /// effective sampling resolution is at worst the 30s reaper timer and
    /// usually finer.
    ///
    /// Sampling — rather than reading a history off disk — is required because
    /// `sweep-checkpoint.sh` *overwrites* the checkpoint at every phase
    /// boundary and the sweep skill **deletes** it on success: the file is a
    /// point-in-time value that is gone precisely when a successful sweep's
    /// record is written. The #4009 freshness guard
    /// ([`checkpoint_written_by_run`]) still applies, so a checkpoint left by
    /// an earlier dispatch of the same issue is never misattributed to this
    /// run.
    ///
    /// The timestamp recorded is the *observation* instant, not the
    /// checkpoint's own `updated_at`: the daemon only ever reads the phase
    /// string (`read_checkpoint_phase`), and an observation time is the honest
    /// upper bound on when the transition happened given a polled source.
    ///
    /// The checkpoint's `pr_number` is captured on the same read (it is written
    /// by the sweep skill from `builder-done` onward), which is what lets the
    /// terminal record carry the sweep's own PR **without** a forge round trip
    /// — including for a successful sweep, whose checkpoint is deleted before
    /// the record is written.
    ///
    /// # Bus emission (Issue #4863)
    ///
    /// Every *newly observed* transition also publishes [`Event::SweepPhase`]
    /// on the attached bus — the only production emit site for that variant, and
    /// therefore the only source of the `sweep.phase` telemetry record the
    /// observability collector maps it to. It lives **here**, rather than in
    /// [`overlay_live_phase`](Self::overlay_live_phase), precisely because this
    /// function already dedupes against `phase_history`: the checkpoint is
    /// re-read on every tick, so an unguarded emit would republish the same
    /// phase for the whole time a sweep sits in it and flood the bus (and D1).
    /// The published `phase` is the **normalized** lifecycle name
    /// ([`phase_label`] — `"curator-done"` → `"curator"`), matching the
    /// `sweep.phase` schema's documented `curator|builder|judge|doctor|merge`
    /// vocabulary, while the stored [`PhaseObservation`] keeps the raw marker.
    pub(crate) fn sample_phase_transition(
        &mut self,
        sweep_id: &str,
        kind: &SweepKind,
        started_at: DateTime<Utc>,
    ) {
        let SweepKind::Issue(issue) = kind else {
            return;
        };
        let checkpoint = self
            .config
            .checkpoint_dir()
            .join(format!("issue-{issue}.json"));
        if !checkpoint_written_by_run(&checkpoint, started_at) {
            return;
        }
        let Some(phase) = read_checkpoint_phase(&checkpoint) else {
            return;
        };

        // Opportunistic LOC snapshot (Issue #5357): runs on EVERY tick the
        // checkpoint is valid, not just on a phase transition, so the most
        // recent diffstat is always cached before a `--merge`-mode sweep's
        // own synchronous worktree cleanup can remove it out from under the
        // terminal-transition write. A failed probe (worktree gone, no
        // mainline ref resolves yet) simply leaves the last successfully
        // sampled value in place rather than clearing it.
        if let Some(loc) = self.probe_worktree_loc(*issue) {
            self.sampled_loc.insert(sweep_id.to_string(), loc);
        }

        let pr_number = read_checkpoint_pr_number(&checkpoint);
        // Issue #8543: sampled on the same per-tick read as `pr_number`, for
        // the identical reason — the checkpoint is gone by the time a
        // successful sweep's terminal record is written.
        let jev = read_checkpoint_jev(&checkpoint);
        let history = self.phase_history.entry(sweep_id.to_string()).or_default();
        if history.last().map(|o| o.phase.as_str()) == Some(phase.as_str()) {
            // Unchanged phase since the previous tick — the common case. Still
            // absorb a `pr_number`/`jev` pair that appeared after the
            // transition was first observed, so a PR opened (or a Tier-2.5
            // Jev call completed) mid-phase is not lost.
            if let Some(last) = history.last_mut() {
                if last.pr_number.is_none() {
                    last.pr_number = pr_number;
                }
                if last.jev.is_none() {
                    last.jev = jev;
                }
            }
            return;
        }
        if history.len() >= MAX_PHASE_OBSERVATIONS {
            return;
        }
        crate::observability::lifecycle::phase_transition(
            &self.config.workspace_root,
            sweep_id,
            &phase,
            *issue,
            pr_number,
        );
        history.push(PhaseObservation {
            phase: phase.clone(),
            at: Utc::now(),
            pr_number,
            jev,
        });
        // Genuine transition — publish it (see the "Bus emission" doc section
        // above). Ordered after the history push so the dedupe state is already
        // committed even if a subscriber panics on delivery.
        self.emit_event(Event::SweepPhase {
            issue: *issue,
            phase: phase_label(&phase).to_string(),
            pr_number: pr_number.and_then(|n| i32::try_from(n).ok()),
            repo: None, // stamped by emit_event (#3929)
        });
    }

    /// The most recent `pr_number` observed on this sweep's checkpoint (Issue
    /// #4704), or `None` when the sweep never reached a phase that records one.
    /// Free (no forge call) — see [`sample_phase_transition`](Self::sample_phase_transition).
    pub(crate) fn sampled_pr_number(&self, sweep_id: &str) -> Option<u32> {
        self.phase_history
            .get(sweep_id)?
            .iter()
            .rev()
            .find_map(|o| o.pr_number)
    }

    /// The most recent `(jev_tier, jev_confidence)` pair observed on this
    /// sweep's checkpoint (Issue #8543), or `None` when no Tier-2.5 Jev call
    /// was ever sampled (no `TYPESAFE_API_KEY`, or the call failed/never
    /// completed). Free (no forge/network call), mirrors
    /// [`Self::sampled_pr_number`] exactly.
    pub(crate) fn sampled_jev(&self, sweep_id: &str) -> Option<(String, f64)> {
        self.phase_history
            .get(sweep_id)?
            .iter()
            .rev()
            .find_map(|o| o.jev.clone())
    }

    /// The most recently opportunistically-sampled `(lines_added,
    /// lines_deleted)` for `sweep_id` (Issue #5357), or `None` when it was
    /// never successfully sampled — see [`sample_phase_transition`](Self::sample_phase_transition)'s
    /// per-tick snapshot and [`probe_worktree_loc`](Self::probe_worktree_loc)'s
    /// live-fallback sibling in [`append_outcome_telemetry_journal`](Self::append_outcome_telemetry_journal).
    pub(crate) fn sampled_loc(&self, sweep_id: &str) -> Option<(i64, i64)> {
        self.sampled_loc.get(sweep_id).copied()
    }

    /// Best-effort local diffstat for issue `N`'s worktree against its
    /// mainline merge base (Issue #5357) — never a forge call, and `None`
    /// (not a fabricated `0`) when the worktree is absent or the probe
    /// fails for any reason (no mainline ref resolves, not a git repo,
    /// `git` unavailable). Delegates to [`crate::git_utils::diff_stat_against_mainline`];
    /// this wrapper only resolves the worktree path from the issue number.
    pub(crate) fn probe_worktree_loc(&self, issue: u32) -> Option<(i64, i64)> {
        let wt = self.worktree_path(issue);
        if !wt.exists() {
            return None;
        }
        crate::git_utils::diff_stat_against_mainline(&wt)
    }

    /// Whether this sweep was ever observed completing the Merge phase (Issue
    /// #4704) — the strongest available "this sweep merged" signal, and the
    /// [`telemetry::SweepResult::Success`] the schema documents ("merged, or
    /// otherwise reached its successful terminal state").
    ///
    /// Load-bearing because exit codes are not always available: an entry with
    /// no retained `Child` handle (reconstructed after a daemon restart) is
    /// reaped via the `kill(pid, 0)` probe, which yields no code at all — so a
    /// merged sweep would otherwise be recorded as a failure purely because its
    /// exit status was unobservable.
    pub(crate) fn sampled_reached_merge(&self, sweep_id: &str) -> bool {
        self.phase_history.get(sweep_id).is_some_and(|history| {
            history
                .iter()
                .any(|o| phase_label(&o.phase) == MERGE_PHASE_LABEL)
        })
    }

    /// Build the `sweep.outcome` record's per-phase breakdown from the
    /// transition history sampled for `sweep_id` (Issue #4704).
    ///
    /// The checkpoint markers are phase *completions* (`curator-done` means the
    /// Curator phase finished), so the duration attributed to a phase is the
    /// interval ending at its own observation and beginning at the previous
    /// observation — or at `started_at` for the first. A phase that appears
    /// twice in one lifecycle (the Judge↔Doctor cycle) yields two entries, in
    /// lifecycle order, which is more faithful than collapsing them.
    ///
    /// The trailing in-flight segment — from the last observed completion to
    /// the terminal transition — is deliberately **not** emitted: the daemon
    /// does not know which phase the sweep was in, and inventing one would be a
    /// fabricated label. So the entries sum to at most `total_duration_sec`,
    /// never more.
    pub(crate) fn phase_durations_for(
        &self,
        sweep_id: &str,
        started_at: DateTime<Utc>,
    ) -> Vec<telemetry::PhaseDuration> {
        let Some(history) = self.phase_history.get(sweep_id) else {
            return Vec::new();
        };
        let mut out = Vec::with_capacity(history.len());
        let mut prev = started_at;
        for observation in history {
            // `max(0)` guards against a clock step between observations; a
            // negative duration on the wire would be nonsense to any consumer.
            let duration_sec = (observation.at - prev).num_seconds().max(0);
            out.push(telemetry::PhaseDuration {
                phase: phase_label(&observation.phase).to_string(),
                duration_sec,
            });
            prev = observation.at;
        }
        out
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::panic,
    clippy::expect_used,
    unused_imports
)]
mod tests;

// End-to-end tests for the #8222 label-timeline sourcing, in their own sibling
// file rather than appended to `tests` above: that module is already near the
// file-size ratchet's threshold (`scripts/check-file-size-budget.sh`), and the
// rule there is to add a new sibling module rather than grow a big one.
#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::panic,
    clippy::expect_used,
    unused_imports
)]
mod timeline_tests;

// API-key-pool credential attribution (#8447), in its own sibling module for
// the same file-size reason as `timeline_tests` above.
#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::panic,
    clippy::expect_used,
    unused_imports
)]
mod credential_tests;

// Runtime/provider/profile attribution + the OpenCode `tokens_by_model`
// dispatch seam (Issue #8507), in its own sibling module for the same
// file-size reason as `credential_tests` above.
#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::panic,
    clippy::expect_used,
    unused_imports
)]
mod runtime_tests;

// Tap-attributed usage accounting (#8556) end-to-end across both terminal
// journals, in its own sibling module for the same file-size reason.
#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::panic,
    clippy::expect_used,
    unused_imports
)]
mod tap_usage_tests;

// End-to-end tests for the #8542 complexity-marker sourcing, in their own
// sibling file for the same file-size reason as `timeline_tests` above.
#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::panic,
    clippy::expect_used,
    unused_imports
)]
mod complexity_tests;
