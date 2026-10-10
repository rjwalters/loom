//! Outcome journal (`sweep.outcome` telemetry) + phase-transition sampling.

use super::*;

/// Judge/Doctor signals read off the sweep PR's forge label timeline (Issue
/// #8222) — the source of the record's `judge_verdicts` and `doctor_cycles`.
pub(crate) mod label_timeline;

/// The Curator complexity-tier signal read off the sweep's issue body (Issue
/// #8542) — the source of the record's `complexity`.
pub(crate) mod complexity_signal;

/// The Curator points-estimate signal read off the sweep's issue body (Issue
/// #9056), consumed only by [`writeback`] — see its own module doc for why
/// this is a separate, conditionally-fetched read rather than a permanent
/// `SweepOutcomeRecord` field like `complexity`.
pub(crate) mod points_signal;

/// The opt-in, post-`Success` issue write-back comment (Issue #9056): renders
/// [`telemetry::SweepOutcomeRecord`] + [`points_signal`]'s estimate to
/// Markdown and posts it, once, to the sweep's originating issue.
pub(crate) mod writeback;

/// Attempt lineage (Issue #9444): `attempt_index`, `previous_sweep_id` and
/// the dispatch `trigger`, derived from this host's durable outcome journal.
pub(crate) mod lineage;

/// In-sweep rework events (Issue #9444): the marker-file protocol the
/// performing paths write and the terminal outcome samples.
pub(crate) mod rework;

/// Generated-path classification and the landing-diff hand-written size
/// facts (Issue #9466): `hw_lines_*`, `hw_files`, `generated_lines`,
/// `test_lines`.
pub(crate) mod landing_size;

/// The cause attached to an `unclassified:no-phase-signal` record (Issue
/// #10642): exit status, last step reached, bounded reason.
pub(crate) mod no_phase;

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
        self.append_outcome_telemetry_journal_observed(
            issue,
            sweep_id,
            duration_sec,
            result,
            failure_class,
            credential,
            tap_region,
            exit_code,
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
    ///
    /// Called only from tests since #10642: the reaper's production path goes
    /// through [`Self::append_outcome_telemetry_journal_observed`] with the
    /// exit status it observed; this keeps the many fixture call sites
    /// unchanged.
    #[cfg_attr(not(test), allow(dead_code))]
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
        self.append_outcome_telemetry_journal_observed(
            issue,
            sweep_id,
            duration_sec,
            result,
            failure_class,
            credential,
            tap_region,
            None,
        );
    }

    /// [`Self::append_outcome_telemetry_journal`] plus the exit status the
    /// reaper observed (Issue #10642), which the `no_phase_cause` of an
    /// `unclassified:no-phase-signal` record reports. `None` means no exit
    /// status was observed.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn append_outcome_telemetry_journal_observed(
        &self,
        issue: u32,
        sweep_id: &str,
        duration_sec: i64,
        result: telemetry::SweepResult,
        failure_class: Option<String>,
        credential: Option<crate::launch_record::CredentialAttribution>,
        tap_region: crate::tap_usage::RegionAccounting,
        exit_code: Option<i32>,
    ) {
        let info = self.entries.get(sweep_id);
        let runtime = info.map(|i| i.runtime.clone());
        // Issue #11370: the resolved facts `sweep.started` reported, not the
        // literal launch arguments: a full model id (absent, with
        // `model_source=unknown`, when none can be named) and the effort that
        // applied with where it came from. Only an entry we hold has either.
        let model = info
            .and_then(|i| i.model.as_deref())
            .and_then(normalize_model_id);
        let resolved_effort = info
            .and_then(|i| resolve_effort_from_env(i.effort.as_deref(), Some(i.runtime.as_str())));
        let effort = resolved_effort.as_ref().map(|r| r.0.clone());
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
        if let Some((_, source)) = &resolved_effort {
            config.insert("effort_source".to_string(), (*source).to_string());
        }
        if info.is_some() && model.is_none() {
            config.insert(
                "model_source".to_string(),
                crate::telemetry::model_source::UNKNOWN.to_string(),
            );
        }
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
        //
        // Issue #9443 keeps the sampled *windows*, not just their lengths, so
        // the per-phase token fold below covers exactly the intervals these
        // durations measure. The fallback entry has no sampled window at all, so
        // it stays usage-less: attributing the whole sweep's tokens to the one
        // phase that happened to be observed last would be a fabrication, and
        // the sweep ran the earlier phases too. That shortfall is reported in
        // `tokens_unattributed` instead.
        let phase_windows = started_at
            .map(|started_at| self.phase_windows_for(sweep_id, started_at))
            .unwrap_or_default();
        let mut phase_durations: Vec<telemetry::PhaseDuration> =
            phase_windows.iter().map(PhaseWindow::to_duration).collect();
        if phase_durations.is_empty() {
            phase_durations = latest_phase
                .map(|phase| {
                    vec![telemetry::PhaseDuration {
                        attempt: Some(1),
                        ..telemetry::PhaseDuration::new(phase_label(&phase), duration_sec)
                    }]
                })
                .unwrap_or_default();
        }

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

        // Issue #9442: `repo` is ALWAYS the `owner/name` forge slug or absent.
        // The pre-#9442 fallback wrote `workspace_root`'s display path on any
        // resolution failure — leaking host usernames/layout into fleet-wide
        // stores and forcing every per-repo query to carry a basename
        // normalization step. When the slug cannot be resolved, the record
        // omits `repo` and sets `repo_unresolved` instead.
        //
        // The two branches differ ONLY in how far resolution may reach:
        // `skip_label_flip` gates every forge read at this site (timeline,
        // complexity), so it keeps that property and resolves from the local
        // `LOOM_REPO` override alone; the normal branch also asks `gh repo
        // view` (breaker-gated, fail-open). Visibility stays Private under
        // `skip_label_flip`, exactly as before.
        let (repo, visibility, repo_unresolved) = if self.config.skip_label_flip {
            match repo_slug_from_env() {
                Some(slug) => (Some(slug), telemetry::RepoVisibility::Private, false),
                None => (None, telemetry::RepoVisibility::Private, true),
            }
        } else {
            match self
                .resolve_owner_repo()
                .map(|(owner, repo)| format!("{owner}/{repo}"))
            {
                Some(slug) => {
                    let visibility = telemetry::visibility::derive_visibility(&slug);
                    (Some(slug), visibility, false)
                }
                None => (None, telemetry::RepoVisibility::Private, true),
            }
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

        // Tokens in/out (Issue #5357) and the per-model breakdown (#6384),
        // resolved together with the `tokens_status` that explains them
        // (Issue #9440) — see `crate::sweep_usage`, which owns the whole
        // decision so this path and the live collector path cannot disagree
        // about what an empty read means.
        //
        // Summed straight from the sweep's own transcripts, split into
        // input/output axes (raw, NOT cost-weighted — see the schema doc and
        // `tokens_in`'s own doc for why), and grouped by `(model, speed,
        // service_tier)` rather than flattened for `tokens_by_model` — see
        // `ModelUsageTotals`'s own doc for why a flat sum cannot be priced.
        // Bounded to this run's own wall-clock window so a re-dispatched
        // issue's earlier runs are never folded in; a sweep whose registry
        // entry is already gone reconstructs that window from its measured
        // duration rather than declining to measure at all (#9440).
        //
        // Issue #9440 is what makes this run for a FAILED sweep too: a
        // pre-flight death publishes a measured zero, a cancelled or
        // watchdog-killed sweep publishes the partial usage it really burned,
        // and only a sweep whose usage genuinely could not be read publishes
        // absent counters — now with a reason attached instead of looking
        // identical to "never spawned".
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
        let usage = crate::sweep_usage::apply_plausibility_guard(
            crate::sweep_usage::resolve(
                usage_runtime.as_deref(),
                &self.config.workspace_root,
                issue,
                crate::sweep_usage::window(started_at, duration_sec),
                failure_class.as_deref(),
            ),
            duration_sec,
        );
        let crate::sweep_usage::SweepUsage {
            tokens_in,
            tokens_out,
            tokens_by_model,
            status: tokens_status,
            reason: tokens_status_reason,
        } = usage;

        // ── Issues #9444/#9465/#9466: the terminal facts that are neither
        // disposition nor usage-status — attempt lineage, marked rework,
        // every sampled PR, the landing's hand-written size, and the model
        // that actually ran. All but the rework markers are free (local
        // journal/phase-history/git reads); none can fail the append.

        // Attempt lineage (Issue #9444): where this sweep sits in the issue's
        // local attempt chain and what triggered it. Read off this host's
        // durable outcome journal — no forge calls, no new state. Absent
        // (never fabricated) when the journal could not be read or the repo
        // slug never resolved (the journal matches on it).
        //
        // Issue #11280: a sweep dispatched by this daemon already published
        // its lineage on `sweep.started` / `fleet.state`; reuse that captured
        // value (including "unknown") so the start and outcome rows cannot
        // disagree when the journal rotated or became unreadable in between.
        // Only a sweep with no captured facts (adopted, PR-set) derives it
        // here.
        let (attempt_index, previous_sweep_id, trigger) =
            if let Some(facts) = self.start_facts.get(sweep_id) {
                (facts.attempt_index, facts.previous_sweep_id.clone(), facts.trigger.clone())
            } else {
                let lineage = repo.as_deref().and_then(|repo_slug| {
                    let journal_path = self.config.resolve_outcome_telemetry_path();
                    let prior = sweep_outcomes::read_all_sweep_outcomes(&journal_path);
                    lineage::derive_lineage(&prior, repo_slug, issue, sweep_id)
                });
                lineage.map_or((None, None, None), |(index, previous, trigger)| {
                    (Some(index), previous, Some(trigger.to_string()))
                })
            };

        // In-sweep rework events (Issue #9444), two sources, markers first:
        // (a) events the performing paths explicitly marked in the
        // `sweep-rework-events.jsonl` protocol, and (b) events read off the
        // worktree's own HEAD reflog — the mechanical writer that needs no
        // role compliance, since a Doctor's conflict rebase or a Builder's
        // merge-from-main records itself there with a timestamp. Both are
        // scoped to this sweep's own window. Absent (never `[]`) when neither
        // saw anything.
        let rework_events = started_at
            .map(|started_at| {
                rework::collect_rework_events(
                    &self.config.workspace_root,
                    &self.worktree_path(issue),
                    issue,
                    started_at,
                )
            })
            .filter(|events| !events.is_empty());

        // Every PR this sweep's lifecycle was observed to carry (Issue
        // #9465), in first-seen order — the multi-PR slice shape the single
        // latest `pr_number` cannot represent.
        let pr_numbers = self.sampled_pr_numbers(sweep_id);

        // Landing-size facts (Issue #9466): the hand-written vs generated
        // split of this sweep's own diff, classified by the repo-owned
        // `generatedPaths` globs over the shipped default. Best-effort and
        // local only — omitted (never 0) whenever the worktree's numstat
        // could not be read, the same contract `lines_added` keeps.
        let landing_size = {
            let worktree = self.worktree_path(issue);
            if worktree.exists() {
                crate::git_utils::diff_rows_against_mainline(&worktree).map(|rows| {
                    let matcher = landing_size::GeneratedMatcher::new(
                        &landing_size::generated_patterns(&self.config.workspace_root),
                    );
                    landing_size::classify_numstat(&rows, &matcher)
                })
            } else {
                None
            }
        };

        // Issue #9465: `model` names what actually ran — the dominant model
        // in the per-model breakdown — with the dispatched model as the
        // fallback when nothing was attributed. The dispatch-time arm stays
        // under `config["arm"]` (#4809).
        let model = tokens_by_model
            .as_ref()
            .and_then(|rows| {
                rows.iter()
                    .max_by_key(|row| row.input.saturating_add(row.output))
                    .map(|row| row.model.clone())
            })
            .or(model);

        // Distinct model ids actually observed in this sweep's transcripts
        // (Issue #8056) — see `models_used_from`.
        let models_used = models_used_from(tokens_by_model.as_deref());

        // Per-phase-attempt usage (Issue #9443): the same read, the same
        // runtime-dispatched source, and the same overall window as
        // `tokens_by_model` above — folded over each sampled phase window
        // instead of the sweep as a whole, so clean-landing cost (curator +
        // builder + FIRST judge) is separable from Doctor/re-judge rework cost.
        //
        // Deliberately derived from the per-model rows rather than measured on a
        // second axis: `TokenUsage::split` is `tokens_in`/`tokens_out`'s own
        // definition, so Σ phases + `tokens_unattributed` reconciles against the
        // sweep totals by construction.
        let phase_slices: Vec<(DateTime<Utc>, DateTime<Utc>)> =
            phase_windows.iter().map(|w| (w.start, w.end)).collect();
        let phase_usage = started_at
            .map(|started_at| {
                crate::usage_source::sweep_tokens_by_window(
                    usage_runtime.as_deref(),
                    &self.config.workspace_root,
                    issue,
                    Some((started_at, Utc::now())),
                    &phase_slices,
                )
            })
            .unwrap_or_default();
        for (entry, rows) in phase_durations.iter_mut().zip(phase_usage) {
            let Some(rows) = rows.filter(|rows| !rows.is_empty()) else {
                continue;
            };
            let (tokens_in, tokens_out) =
                crate::observability::runtime_usage::TokenUsage::from_models(&rows).split();
            entry.tokens_in = Some(tokens_in);
            entry.tokens_out = Some(tokens_out);
            entry.tokens_by_model = Some(rows);
        }

        // The remainder no phase entry accounts for (Issue #9443). Present only
        // when the sweep's own totals are known — with nothing to take a
        // remainder of, `0` would falsely claim the phases cover everything.
        // Saturating: the two sides are folded from the same file set with the
        // same dedupe, so the subtraction cannot legitimately go negative, and
        // clamping is preferable to wrapping if a future source breaks that.
        let tokens_unattributed = tokens_in.zip(tokens_out).map(|(total_in, total_out)| {
            let (attributed_in, attributed_out) = phase_durations
                .iter()
                .filter_map(telemetry::PhaseDuration::token_split)
                .fold((0u64, 0u64), |(sum_in, sum_out), (tin, tout)| {
                    (sum_in.saturating_add(tin), sum_out.saturating_add(tout))
                });
            telemetry::TokenTotals {
                tokens_in: total_in.saturating_sub(attributed_in),
                tokens_out: total_out.saturating_sub(attributed_out),
            }
        });

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

        // Curator complexity tier (Issue #8542) plus the issue's own end state
        // (Issue #9441), read off the sweep's own issue in ONE REST call — see
        // `complexity_signal`'s module doc for why this is a separate forge
        // read from the PR timeline above rather than a dispatch-time plumb,
        // why the two signals share one call, and their identical fail-open
        // contract.
        let issue_signals = self.fetch_issue_signals(issue);
        let complexity = issue_signals.complexity.clone();
        let issue_end_state = issue_signals.end_state(started_at);
        // The Curator's story-point size (Issue #9432, epic #9429), folded out
        // of the SAME read's label list — no extra forge round trip, and the
        // estimate can never describe a different issue than `complexity` and
        // the end state do. Pure label folding with the one-label-per-issue
        // guard: absent, out-of-vocabulary and stacked points labels all yield
        // `None` (the last two logged loudly), never a guessed or zero size.
        let story_points = issue_signals.story_points(issue);

        // What this sweep actually DID (Issue #9441) — a pure derivation over
        // the signals already assembled above, NOT new instrumentation. The
        // disposition and its mandatory `failure_class` come back as one value
        // so the "env_failure/substantive_failure/unknown must say why"
        // invariant cannot be half-applied; see `telemetry::disposition`.
        let (disposition, failure_class) =
            telemetry::classify_disposition(&telemetry::DispositionSignals {
                result,
                pr_number,
                failure_class: failure_class.as_deref(),
                phase_durations: &phase_durations,
                total_duration_sec: duration_sec,
                judge_verdicts: judge_verdicts.as_deref(),
                doctor_cycles,
                issue_end_state,
            });
        // Issue #10642: a synthesized `no-phase-signal` class says nothing on
        // its own, so attach what is known about how the run ended.
        let no_phase_cause = (failure_class.as_deref() == Some(telemetry::NO_PHASE_SIGNAL_CLASS))
            .then(|| {
                self.no_phase_cause_for(
                    sweep_id,
                    issue,
                    exit_code,
                    phase_durations.last().map(|p| p.phase.as_str()),
                )
            });

        let outcome_record = telemetry::SweepOutcomeRecord {
            repo,
            repo_unresolved,
            visibility,
            issue,
            sweep_id: sweep_id.to_string(),
            model,
            effort,
            config,
            phase_durations,
            total_duration_sec: duration_sec,
            result,
            disposition,
            pr_number,
            tokens_in,
            tokens_out,
            lines_added,
            lines_deleted,
            tokens_by_model,
            tokens_unattributed,
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
            story_points,
            tokens_status: Some(tokens_status),
            attempt_index,
            previous_sweep_id,
            trigger,
            rework_events,
            pr_numbers,
            hw_lines_added: landing_size.as_ref().map(|size| size.hw_lines_added),
            hw_lines_deleted: landing_size.as_ref().map(|size| size.hw_lines_deleted),
            hw_files: landing_size.as_ref().map(|size| size.hw_files),
            generated_lines: landing_size.as_ref().map(|size| size.generated_lines),
            test_lines: landing_size.as_ref().map(|size| size.test_lines),
            tokens_status_reason,
            no_phase_cause,
        };
        // Issue #9441: both disposition invariants hold on every record this
        // daemon writes. A debug assertion rather than a runtime guard — the
        // classifier makes them true by construction (and proves it in its own
        // contract tests), so a violation here is a code defect to catch in
        // test/CI, never a reason to drop a record in production.
        debug_assert!(
            outcome_record.disposition_invariants_hold(),
            "#9441: sweep.outcome for issue #{issue} ({sweep_id}) violates a disposition \
             invariant: disposition={}, pr_number={:?}, failure_class={:?}",
            outcome_record.disposition.as_str(),
            outcome_record.pr_number,
            outcome_record.failure_class,
        );
        // Issue #9056: opt-in, post-`Success`-only issue write-back comment.
        // Only a clone of the assembled record is taken HERE (`outcome_record`
        // itself moves into the telemetry envelope below); every forge call —
        // the idempotency read, the points read, and the post — runs strictly
        // AFTER the durable local writes at the end of this function, so a
        // slow or hung `gh` can never delay (or, via a daemon restart in that
        // window, lose) the `sweep.outcome` record. See `writeback`'s module
        // doc for the full fail-open contract.
        let writeback_record =
            (result == telemetry::SweepResult::Success).then(|| outcome_record.clone());
        let result_name = serde_json::to_value(result)
            .ok()
            .and_then(|v| v.as_str().map(str::to_owned))
            .unwrap_or_else(|| "unknown".into());
        let mut metadata = crate::observability::lifecycle::attributes(&[
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
        // Issue #9442: `loom.repo` only when the slug resolved; the unresolved
        // case is stamped explicitly so fleet queries can count it instead of
        // guessing from an absent attribute.
        if let Some(slug) = outcome_record.repo.as_deref() {
            metadata.insert("loom.repo".into(), slug.to_string());
        }
        if outcome_record.repo_unresolved {
            metadata.insert("loom.repo_unresolved".into(), "true".into());
        }
        if let Some(pr) = pr_number {
            metadata.insert("loom.pr_number".into(), pr.to_string());
        }
        // #9441: the "what did this sweep DO" axis on the span too, so a trace
        // query can separate a landing from a no-op re-dispatch without
        // joining the journal.
        metadata.insert("loom.disposition".into(), outcome_record.disposition.as_str().to_string());
        // Issues #9444/#9465/#9466: the new terminal facts ride the trace
        // metadata too, so a span-side reader sees the same verdict the
        // record carries.
        if let Some(index) = outcome_record.attempt_index {
            metadata.insert("loom.attempt_index".into(), index.to_string());
        }
        if let Some(previous) = outcome_record.previous_sweep_id.as_deref() {
            metadata.insert("loom.previous_sweep_id".into(), previous.to_string());
        }
        if let Some(trigger) = outcome_record.trigger.as_deref() {
            metadata.insert("loom.trigger".into(), trigger.to_string());
        }
        for (key, value) in [
            ("loom.failure_class", outcome_record.failure_class.as_ref()),
            ("loom.configured_model", outcome_record.model.as_ref()),
            ("loom.effort", outcome_record.effort.as_ref()),
            ("loom.effort_source", outcome_record.config.get("effort_source")),
            ("loom.model_source", outcome_record.config.get("model_source")),
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
        if let Some(cause) = &outcome_record.no_phase_cause {
            no_phase::insert_span_attributes(&mut metadata, cause);
        }
        // #9443: the same per-phase numbers `phase_durations` carries, on the
        // execution's own `loom.role_attempt` spans, so the D1/JSONL path and
        // the OTLP path report one set of per-phase costs. Before
        // `finish_sweep`/`finish_execution`, which retire the journal.
        let phase_usage_spans: Vec<crate::observability::runtime_usage::PhaseUsage<'_>> =
            phase_windows
                .iter()
                .zip(&outcome_record.phase_durations)
                .filter_map(|(window, entry)| {
                    Some(crate::observability::runtime_usage::PhaseUsage {
                        role: entry.phase.as_str(),
                        attempt: entry.attempt.unwrap_or(1),
                        window: (window.start, window.end),
                        rows: entry.tokens_by_model.as_deref()?,
                    })
                })
                .collect();
        crate::observability::runtime_usage::record_phase_usage(
            &self.config.workspace_root,
            sweep_id,
            &phase_usage_spans,
            outcome_record.runtime.as_deref(),
        );
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
        // Issue #9056: forge work only after every durable write above.
        if let Some(record) = writeback_record {
            self.maybe_post_sweep_outcome_writeback(issue, &record);
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

    /// Every PR number this sweep's lifecycle was observed to carry (Issue
    /// #9465), in first-seen order — the multi-PR slice shape a single
    /// latest-`pr_number` cannot represent. Free (no forge call): read off
    /// the same phase history [`Self::sampled_pr_number`] reads.
    pub(crate) fn sampled_pr_numbers(&self, sweep_id: &str) -> Option<Vec<u32>> {
        let history = self.phase_history.get(sweep_id)?;
        let mut seen = Vec::new();
        for observation in history {
            if let Some(pr) = observation.pr_number {
                if !seen.contains(&pr) {
                    seen.push(pr);
                }
            }
        }
        (!seen.is_empty()).then_some(seen)
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
    /// transition history sampled for `sweep_id` (Issue #4704), as attribution
    /// *windows* rather than bare lengths (Issue #9443) so the durations and the
    /// per-phase token fold can never describe different intervals.
    ///
    /// The checkpoint markers are phase *completions* (`curator-done` means the
    /// Curator phase finished), so the interval attributed to a phase ends at its
    /// own observation and begins at the previous observation — or at
    /// `started_at` for the first. A phase that appears twice in one lifecycle
    /// (the Judge↔Doctor cycle) yields two windows, in lifecycle order and
    /// distinguished by a 1-based-per-phase [`PhaseWindow::attempt`], which is
    /// more faithful than collapsing them and is what makes "the *first* judge"
    /// addressable.
    ///
    /// The trailing in-flight segment — from the last observed completion to
    /// the terminal transition — is deliberately **not** emitted: the daemon
    /// does not know which phase the sweep was in, and inventing one would be a
    /// fabricated label. So the windows cover at most `total_duration_sec`, never
    /// more, and the tokens spent in that segment land in
    /// [`telemetry::SweepOutcomeRecord::tokens_unattributed`].
    ///
    /// Empty when this sweep's transition history was never sampled (a daemon
    /// restart mid-sweep, a sweep that died before the first reaper tick) — the
    /// caller then has no window to attribute anything to, which is why such a
    /// record's fallback single entry carries no usage.
    pub(crate) fn phase_windows_for(
        &self,
        sweep_id: &str,
        started_at: DateTime<Utc>,
    ) -> Vec<PhaseWindow> {
        let Some(history) = self.phase_history.get(sweep_id) else {
            return Vec::new();
        };
        let mut out: Vec<PhaseWindow> = Vec::with_capacity(history.len());
        let mut prev = started_at;
        for observation in history {
            let phase = phase_label(&observation.phase).to_string();
            let attempt = u32::try_from(out.iter().filter(|w| w.phase == phase).count())
                .unwrap_or(u32::MAX - 1)
                .saturating_add(1);
            out.push(PhaseWindow {
                phase,
                attempt,
                // `max(prev)` guards against a clock step between observations;
                // an end before its own start would be nonsense to a consumer
                // and would make the window fold nothing.
                start: prev,
                end: observation.at.max(prev),
            });
            prev = observation.at;
        }
        out
    }
}

/// One sampled phase attempt's attribution window (Issue #9443).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PhaseWindow {
    /// The normalized lifecycle phase name ([`phase_label`]).
    pub(crate) phase: String,
    /// 1-based index among this record's entries for the same `phase`.
    pub(crate) attempt: u32,
    /// The previous observation's instant (or the sweep's start, for the first).
    pub(crate) start: DateTime<Utc>,
    /// This observation's instant. Never before `start`.
    pub(crate) end: DateTime<Utc>,
}

impl PhaseWindow {
    /// The duration-only `phase_durations` entry for this window; usage fields
    /// are filled in afterwards, once the usage source is known.
    pub(crate) fn to_duration(&self) -> telemetry::PhaseDuration {
        telemetry::PhaseDuration {
            attempt: Some(self.attempt),
            ..telemetry::PhaseDuration::new(
                self.phase.clone(),
                (self.end - self.start).num_seconds().max(0),
            )
        }
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

// Per-phase token attribution + `tokens_unattributed` (#9443), in its own
// sibling module for the same file-size reason as `runtime_tests` above.
#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::panic,
    clippy::expect_used,
    unused_imports
)]
mod phase_usage_tests;

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

// End-to-end tests for the #9056 issue write-back (posting, idempotency, the
// `Success`-only gate), in their own sibling file for the same file-size
// reason as `timeline_tests` above. `points_signal` and `writeback`'s own
// pure/unit tests live inline in those modules instead.
#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::panic,
    clippy::expect_used,
    unused_imports
)]
mod writeback_tests;

// End-to-end tests for the #9441 `disposition` field as it lands on a real
// journal record, in their own sibling file for the same file-size reason as
// `timeline_tests` above. The classifier's own exhaustive contract tests are
// pure and live with it, in `telemetry::disposition`.
#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::panic,
    clippy::expect_used,
    unused_imports
)]
mod disposition_tests;

// End-to-end tests for the #9440 `tokens_status` axis (a failed/cancelled
// sweep's token counters and the absence-case discriminator), in their own
// sibling file for the same file-size reason as `timeline_tests` above.
#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::panic,
    clippy::expect_used,
    unused_imports
)]
mod tokens_status_tests;

// Contract tests for the #9465 PR-linkage and actually-ran-model derivation
// (`pr_numbers`, `model` vs. `config["arm"]`), in their own sibling file for
// the same file-size reason as `timeline_tests` above.
#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::panic,
    clippy::expect_used,
    unused_imports
)]
mod pr_link_tests;
