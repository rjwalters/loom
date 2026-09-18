//! The operator-attention health section (Issue #8091): open PRs labeled
//! `loom:operator` (the first-class "the engine has stopped acting on this
//! artifact, a human is needed" hold, #5502) plus their `mergeable`/age
//! breakdown, and open issues labeled `loom:operator-only` (the hard park),
//! across every managed repo — answering "what is waiting on me?" without a
//! separate `gh` query.
//!
//! Split into its own file, mirroring [`super::holds`] (#7990): `health.rs`
//! sits at its `.loom/docs/file-size-policy.md` ratchet, so new assessment
//! logic goes in a sibling module and the parent keeps only the dispatch
//! line and re-export.

use super::{accumulate_observed, repo_label, HealthInputs, HealthSection, Verdict};

/// Assess the operator-attention section.
///
/// # Always `Verdict::Green` — deliberately, unconditionally
///
/// [`super::assess`]'s `overall` roll-up is `all(sections, is_green)`, and
/// [`super::HealthReport::exit_code`] maps `overall` straight to the process
/// exit code (see `health.rs`'s module-level "Busy vs degraded" doc section
/// for the full roll-up rule) — so **any** section that can ever render
/// non-Green changes `loom-daemon health`'s exit code. A fleet with held PRs
/// is normal steady state, not a fault: "a human is needed" is a routing
/// fact about work, not a statement that the fleet is unhealthy — the same
/// distinction `.github/labels.yml` already draws between `loom:operator` (a
/// first-class, re-evaluable hold) and an actual failure. This section's
/// verdict is therefore unconditionally [`Verdict::Green`], **never** gated
/// on a count or a threshold, so `exit_code()` is provably unchanged by its
/// presence — pinned by
/// `operator_attention_with_held_prs_does_not_change_the_exit_code` below, in
/// the style of `build_skew_alone_does_not_change_the_exit_code`
/// (`health/tests.rs`).
///
/// # Degrading on a forge failure
///
/// A failed forge read must never render as "0 held" (indistinguishable from
/// a genuinely clear queue) — so, mirroring `assess_queues`, a missing `gh`
/// binary or an unread pipeline snapshot renders `?` (via
/// [`crate::pipeline_snapshot::format_count`]) rather than a count, and a
/// per-repo query failure is named in the summary rather than silently
/// folded into the total. Because the verdict is unconditionally Green (see
/// above), none of these failure paths can themselves flip the exit code —
/// that would reintroduce the exact problem `EXIT_INDETERMINATE_BUSY`
/// (#6191) exists to keep a non-fault state off the fault exit code for.
#[must_use]
pub fn assess_operator_attention(inputs: &HealthInputs) -> HealthSection {
    if let Some(gh) = &inputs.gh_unavailable {
        return HealthSection::new(
            "operator_attention",
            Verdict::Green,
            format!("? held, ? operator-only — {}", gh.reason),
            serde_json::json!({
                "unavailable": "gh not found on PATH or not executable",
                "gh_bin": gh.gh_bin,
                "reason": gh.reason,
                "held": null,
                "held_conflicting": null,
                "held_oldest_days": null,
                "operator_only_issues": null,
            }),
        );
    }
    let Some(pipeline) = &inputs.pipeline else {
        return HealthSection::new(
            "operator_attention",
            Verdict::Green,
            "? held, ? operator-only — forge snapshot not collected",
            serde_json::json!({
                "unavailable": "forge snapshot not collected",
                "held": null,
                "held_conflicting": null,
                "held_oldest_days": null,
                "operator_only_issues": null,
            }),
        );
    };
    if pipeline.is_empty() {
        return HealthSection::new(
            "operator_attention",
            Verdict::Green,
            "0 held, 0 operator-only (no managed repos)",
            serde_json::json!({
                "held": 0,
                "held_conflicting": 0,
                "held_oldest_days": null,
                "operator_only_issues": 0,
                "repos": [],
            }),
        );
    }

    let mut held_total: Option<usize> = None;
    let mut conflicting_total: Option<usize> = None;
    let mut oldest_days: Option<i64> = None;
    let mut issues_total: Option<usize> = None;
    let mut failed: Vec<String> = Vec::new();

    for snap in pipeline {
        let name = repo_label(&snap.root);
        let mut repo_failed = false;
        match snap.operator_held {
            Some(n) => accumulate_observed(&mut held_total, Some(n)),
            None => repo_failed = true,
        }
        accumulate_observed(&mut conflicting_total, snap.operator_held_conflicting);
        if let Some(days) = snap.operator_held_oldest_days {
            oldest_days = Some(oldest_days.map_or(days, |cur: i64| cur.max(days)));
        }
        match snap.operator_only_issues {
            Some(n) => accumulate_observed(&mut issues_total, Some(n)),
            None => repo_failed = true,
        }
        if repo_failed {
            failed.push(name);
        }
    }

    let held_str = crate::pipeline_snapshot::format_count(held_total);
    let issues_str = crate::pipeline_snapshot::format_count(issues_total);

    let mut breakdown: Vec<String> = Vec::new();
    if held_total.unwrap_or(0) > 0 {
        if let Some(c) = conflicting_total.filter(|&c| c > 0) {
            breakdown.push(format!("{c} conflicting"));
        }
        if let Some(d) = oldest_days {
            breakdown.push(format!("oldest {d}d"));
        }
    }
    let breakdown_clause = if breakdown.is_empty() {
        String::new()
    } else {
        format!(" ({})", breakdown.join(", "))
    };

    let failed_clause = if failed.is_empty() {
        String::new()
    } else {
        format!("; forge query FAILED for: {}", failed.join(", "))
    };

    let summary =
        format!("{held_str} PR(s) held{breakdown_clause}, {issues_str} issue(s) operator-only{failed_clause}");

    HealthSection::new(
        "operator_attention",
        Verdict::Green,
        summary,
        serde_json::json!({
            "held": held_total,
            "held_conflicting": conflicting_total,
            "held_oldest_days": oldest_days,
            "operator_only_issues": issues_total,
            "repos": pipeline
                .iter()
                .map(|s| serde_json::json!({
                    "root": s.root,
                    "operator_held": s.operator_held,
                    "operator_held_conflicting": s.operator_held_conflicting,
                    "operator_held_oldest_days": s.operator_held_oldest_days,
                    "operator_only_issues": s.operator_only_issues,
                    "error": s.error,
                }))
                .collect::<Vec<_>>(),
        }),
    )
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::{assess_operator_attention, HealthInputs, Verdict};
    use crate::health::{assess, EXIT_DEGRADED, EXIT_HEALTHY};
    use crate::pipeline_snapshot::{GhUnavailable, RepoPipelineSnapshot};
    use std::path::PathBuf;

    fn gh_unavailable_fixture() -> GhUnavailable {
        GhUnavailable {
            gh_bin: "gh".to_string(),
            reason: "`gh` not found on PATH".to_string(),
            observed_path: Some("/usr/bin:/bin".to_string()),
        }
    }

    /// The core assertion: counts sum correctly across repos and the verdict
    /// is GREEN — the ordinary held-PRs-exist case, not a fault.
    #[test]
    fn sums_held_and_issue_counts_across_repos() {
        let inputs = HealthInputs {
            pipeline: Some(vec![
                RepoPipelineSnapshot {
                    root: PathBuf::from("/r/loom"),
                    operator_held: Some(4),
                    operator_held_conflicting: Some(1),
                    operator_held_oldest_days: Some(5),
                    operator_only_issues: Some(3),
                    ..Default::default()
                },
                RepoPipelineSnapshot {
                    root: PathBuf::from("/r/anvil"),
                    operator_held: Some(2),
                    operator_held_conflicting: Some(0),
                    operator_held_oldest_days: Some(1),
                    operator_only_issues: Some(5),
                    ..Default::default()
                },
            ]),
            ..HealthInputs::default()
        };
        let section = assess_operator_attention(&inputs);
        assert_eq!(section.verdict, Verdict::Green, "{}", section.summary);
        assert!(section.summary.contains("6 PR(s) held"), "{}", section.summary);
        assert!(section.summary.contains("1 conflicting"), "{}", section.summary);
        assert!(section.summary.contains("oldest 5d"), "{}", section.summary);
        assert!(section.summary.contains("8 issue(s) operator-only"), "{}", section.summary);
        assert_eq!(section.detail["held"], 6);
        assert_eq!(section.detail["held_conflicting"], 1);
        assert_eq!(section.detail["held_oldest_days"], 5);
        assert_eq!(section.detail["operator_only_issues"], 8);
    }

    /// AC: `--json` carries the counts as fields — `detail` IS the `--json`
    /// payload for this section (`HealthReport` serializes `sections`
    /// verbatim), so asserting the JSON shape here is asserting the `--json`
    /// contract.
    #[test]
    fn json_detail_carries_every_field() {
        let inputs = HealthInputs {
            pipeline: Some(vec![RepoPipelineSnapshot {
                root: PathBuf::from("/r/loom"),
                operator_held: Some(1),
                operator_held_conflicting: Some(0),
                operator_held_oldest_days: Some(0),
                operator_only_issues: Some(2),
                ..Default::default()
            }]),
            ..HealthInputs::default()
        };
        let report = assess(&inputs);
        let value = serde_json::to_value(&report).unwrap();
        let section = value["sections"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["key"] == "operator_attention")
            .expect("operator_attention section present in --json output");
        assert_eq!(section["verdict"], "green");
        assert_eq!(section["detail"]["held"], 1);
        assert_eq!(section["detail"]["held_conflicting"], 0);
        assert_eq!(section["detail"]["held_oldest_days"], 0);
        assert_eq!(section["detail"]["operator_only_issues"], 2);
    }

    /// THE mandatory regression pin (Issue #8091): a healthy fleet that also
    /// happens to have held PRs (the steady state on this repo) must still
    /// exit `0` — in the style of
    /// `build_skew_alone_does_not_change_the_exit_code`. A fleet with several
    /// `loom:operator` PRs is not a fault, and this section must never be
    /// the thing that flips `loom-daemon health`'s exit code.
    #[test]
    fn with_held_prs_does_not_change_the_exit_code() {
        // Every OTHER section must be Green for this to be a real assertion
        // (not a tautology on an already-degraded report), so this starts
        // from `health::tests`'s own known-good fixture rather than
        // `HealthInputs::default()`, which leaves `status: None` and every
        // status-derived section UNKNOWN.
        let mut inputs = crate::health::tests::healthy_inputs();
        inputs.pipeline = Some(vec![RepoPipelineSnapshot {
            root: PathBuf::from("/repos/loom"),
            queued: Some(3),
            merged_24h: Some(1),
            operator_held: Some(6),
            operator_held_conflicting: Some(1),
            operator_held_oldest_days: Some(0),
            operator_only_issues: Some(8),
            ..Default::default()
        }]);
        let report = assess(&inputs);
        assert_eq!(
            report.section("operator_attention").unwrap().verdict,
            Verdict::Green,
            "the section itself must be Green with held PRs present"
        );
        assert_eq!(
            report.exit_code(),
            EXIT_HEALTHY,
            "held PRs are steady state, not a fault: {}",
            report.render_human()
        );
    }

    /// A forge-read failure must not render as "0 held" — indistinguishable
    /// from a genuinely clear queue — and, since the verdict is
    /// unconditionally Green, must not itself change the exit code either.
    #[test]
    fn gh_unavailable_renders_unknown_not_zero() {
        let inputs = HealthInputs {
            pipeline: None,
            gh_unavailable: Some(gh_unavailable_fixture()),
            ..HealthInputs::default()
        };

        let section = assess_operator_attention(&inputs);
        assert_eq!(section.verdict, Verdict::Green);
        assert!(!section.summary.contains("0 held"), "{}", section.summary);
        assert!(section.summary.contains('?'), "{}", section.summary);
        assert!(section.detail["held"].is_null());

        // Nothing about a missing `gh` in THIS section changes the exit
        // code — the section is unconditionally Green either way. (Other
        // sections such as `queues`/`throughput` do still degrade on the
        // same input; that is their contract, not this one's.)
        assert_eq!(assess(&inputs).exit_code(), EXIT_DEGRADED, "queues/throughput still degrade");
    }

    /// The forge snapshot simply not having been collected (distinct from a
    /// known-missing `gh`) is the same "cannot tell" story, not a fabricated
    /// 0.
    #[test]
    fn missing_pipeline_renders_unknown_not_zero() {
        let inputs = HealthInputs {
            pipeline: None,
            ..HealthInputs::default()
        };

        let section = assess_operator_attention(&inputs);
        assert_eq!(section.verdict, Verdict::Green);
        assert!(!section.summary.contains("0 held"), "{}", section.summary);
        assert!(section.detail["held"].is_null());
    }

    /// One repo's query failing must not silently zero out the total, and
    /// must not suppress the counts that DID succeed for the other repo.
    #[test]
    fn partial_repo_failure_keeps_the_other_repos_counts() {
        // Same known-good baseline as `with_held_prs_does_not_change_the_exit_code`
        // above — needed so the final `exit_code()` assertion is a real
        // regression pin rather than one already-degraded by an unrelated
        // UNKNOWN section.
        let mut inputs = crate::health::tests::healthy_inputs();
        inputs.pipeline = Some(vec![
            RepoPipelineSnapshot {
                root: PathBuf::from("/repos/loom"),
                queued: Some(2),
                merged_24h: Some(0),
                operator_held: Some(3),
                operator_held_conflicting: Some(0),
                operator_only_issues: Some(1),
                ..Default::default()
            },
            RepoPipelineSnapshot {
                root: PathBuf::from("/repos/anvil"),
                // Only the operator-held axis failed for this repo; the
                // other metrics (`queued`/`merged_24h`) succeeded,
                // mirroring how `GhPipelineSource::fetch` records each
                // metric's failure independently.
                queued: Some(1),
                merged_24h: Some(0),
                operator_held: None,
                operator_only_issues: None,
                error: Some("rate limited".to_string()),
                ..Default::default()
            },
        ]);
        let section = assess_operator_attention(&inputs);
        assert_eq!(section.verdict, Verdict::Green, "{}", section.summary);
        assert_eq!(section.detail["held"], 3, "the successful repo's count must not be discarded");
        assert!(section.summary.contains("forge query FAILED for: anvil"), "{}", section.summary);
        // Every other section (queues/throughput) stayed Green above, and
        // this section is unconditionally Green (see its own doc) — so the
        // fleet-wide exit code is unaffected by this repo's
        // operator-attention read failure.
        assert_eq!(
            assess(&inputs).exit_code(),
            EXIT_HEALTHY,
            "an operator-attention read failure alone must not degrade the exit code"
        );
    }

    /// Zero managed repos is `0 held, 0 operator-only`, not a crash or `?`.
    #[test]
    fn no_managed_repos_is_zero_not_unknown() {
        let inputs = HealthInputs {
            pipeline: Some(vec![]),
            ..HealthInputs::default()
        };
        let section = assess_operator_attention(&inputs);
        assert_eq!(section.verdict, Verdict::Green);
        assert!(section.summary.contains("0 held"), "{}", section.summary);
        assert!(section.summary.contains("0 operator-only"), "{}", section.summary);
    }
}
