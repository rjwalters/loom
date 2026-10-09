//! Agreement check (#10197): does `fleet_state(as_of)`, rebuilt from forge
//! history, reproduce the queue features the daemon logged on each
//! `eta.estimate`?
//!
//! Pure. The caller loads the logged [`Explanation`]s (a SigNoz export, one
//! JSON object per line, see [`parse_explanations`]) and the raw events; this
//! module replays each estimate's `as_of` and tallies, per feature, how often
//! the two agree and how they differ when they do not
//! (`reconstructed - logged`). Features the forge event log cannot supply
//! (PR size, model, host pool) are listed under `not_reconstructable`, never
//! scored.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::explanation::Explanation;
use super::fleet_events::{ItemKind, RawEvent};
use super::fleet_state::{fleet_state, FleetState, ItemStage};

/// Schema tag of an [`AgreementReport`].
pub const AGREEMENT_SCHEMA: &str = "eta-fleet-agreement/v1";

/// Seconds of slack for timestamp features: the logged value comes from the
/// forge's own `created_at`, the reconstructed one from an event row.
pub const TIME_TOLERANCE_SEC: i64 = 2;

/// Features the forge event log does not carry.
pub const NOT_RECONSTRUCTABLE: &[&str] = &[
    "pr_additions",
    "pr_deletions",
    "pr_changed_files",
    "pr_commits",
    "active_sweeps_host (host-local; only the `<= building` bound is checked)",
    "max_concurrent",
    "sweep_model",
    "sweep_runtime",
    "checks_pending",
    "checks_failed",
    "checks_all_pending",
    "checks_all_failed",
    "pool_usable_accounts (host-local)",
    "pool_exhausted (host-local)",
    "ratelimit_core_remaining (host-local)",
    "ratelimit_core_reset_at (host-local)",
    "ratelimit_graphql_remaining (host-local)",
    "ratelimit_graphql_reset_at (host-local)",
    "breaker_state (host-local)",
    "breaker_cooldown_until (host-local)",
    "ratelimit_writer_core_remaining (host-local)",
    "ratelimit_writer_graphql_remaining (host-local)",
    "ratelimit_min_remaining (host-local)",
    "ratelimit_exhausted (host-local)",
];

/// One feature's tally.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct FeatureAgreement {
    /// Estimates where both sides had a value.
    pub compared: u64,
    /// Of those, how many agreed.
    pub agreed: u64,
    /// `agreed / compared`, `null` when nothing was compared.
    pub rate: Option<f64>,
    /// Mean of `reconstructed - logged` over the numeric disagreements.
    pub mean_signed_diff: Option<f64>,
    /// Estimates where the logged side had a value and the reconstruction
    /// had none (item not in the cache's window).
    pub unreconstructed: u64,
    #[serde(skip)]
    diff_sum: f64,
    #[serde(skip)]
    diff_n: u64,
}

impl FeatureAgreement {
    fn record(&mut self, agreed: bool, diff: Option<f64>) {
        self.compared += 1;
        if agreed {
            self.agreed += 1;
        } else if let Some(d) = diff {
            self.diff_sum += d;
            self.diff_n += 1;
        }
    }

    fn finish(&mut self) {
        if self.compared > 0 {
            self.rate = Some(self.agreed as f64 / self.compared as f64);
        }
        if self.diff_n > 0 {
            self.mean_signed_diff = Some(self.diff_sum / self.diff_n as f64);
        }
    }
}

/// The report.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgreementReport {
    pub schema: String,
    pub repo: String,
    /// Estimates for `repo` scored.
    pub estimates: u64,
    /// Earliest and latest `as_of` scored.
    pub from: Option<DateTime<Utc>>,
    pub to: Option<DateTime<Utc>>,
    /// Per feature name.
    pub features: BTreeMap<String, FeatureAgreement>,
    pub not_reconstructable: Vec<String>,
}

/// Parse a JSONL export of `eta-explanation/v1` records. Blank and
/// unparseable lines are skipped and counted in the second return value.
#[must_use]
pub fn parse_explanations(text: &str) -> (Vec<Explanation>, usize) {
    let mut out = Vec::new();
    let mut skipped = 0;
    for line in text.lines().filter(|l| !l.trim().is_empty()) {
        match serde_json::from_str::<Explanation>(line) {
            Ok(e) => out.push(e),
            Err(_) => skipped += 1,
        }
    }
    (out, skipped)
}

fn within(a: DateTime<Utc>, b: DateTime<Utc>) -> bool {
    (a - b).num_seconds().abs() <= TIME_TOLERANCE_SEC
}

/// Score `explanations` for `repo` against `events`.
#[must_use]
pub fn agreement(events: &[RawEvent], repo: &str, explanations: &[Explanation]) -> AgreementReport {
    let mut report = AgreementReport {
        schema: AGREEMENT_SCHEMA.to_string(),
        repo: repo.to_string(),
        estimates: 0,
        from: None,
        to: None,
        features: BTreeMap::new(),
        not_reconstructable: NOT_RECONSTRUCTABLE
            .iter()
            .map(|s| (*s).to_string())
            .collect(),
    };
    let mut states: BTreeMap<DateTime<Utc>, FleetState> = BTreeMap::new();
    for e in explanations
        .iter()
        .filter(|e| e.subject.repo.eq_ignore_ascii_case(repo))
    {
        let Some(f) = &e.features else { continue };
        report.estimates += 1;
        report.from = Some(report.from.map_or(e.as_of, |t| t.min(e.as_of)));
        report.to = Some(report.to.map_or(e.as_of, |t| t.max(e.as_of)));
        let state = states
            .entry(e.as_of)
            .or_insert_with(|| fleet_state(events, repo, e.as_of));
        let issue = state
            .items
            .iter()
            .find(|i| i.kind == ItemKind::Issue && i.number == e.subject.issue);
        let pr = e.subject.pr_number.and_then(|n| {
            state
                .items
                .iter()
                .find(|i| i.kind == ItemKind::Pr && i.number == n)
        });
        let ready = state
            .stage_counts
            .get(ItemStage::ReadyWait.as_str())
            .copied()
            .unwrap_or(0) as i64;
        let mut tally = |name: &str, logged: bool, rec: Option<(bool, Option<f64>)>| {
            if !logged {
                return;
            }
            let entry = report.features.entry(name.to_string()).or_default();
            match rec {
                Some((ok, diff)) => entry.record(ok, diff),
                None => entry.unreconstructed += 1,
            }
        };
        let num = |rec: i64, logged: i64| Some((rec == logged, Some((rec - logged) as f64)));
        tally(
            "queue_ready",
            f.queue_ready.is_some(),
            f.queue_ready.and_then(|l| num(ready, i64::from(l))),
        );
        tally(
            "queue_running",
            f.queue_running.is_some(),
            f.queue_running
                .and_then(|l| num(state.building as i64, i64::from(l))),
        );
        // A rank is logged only for a ready item: agree when the forge also
        // had it ready.
        tally(
            "queue_rank_present",
            f.queue_rank.is_some(),
            issue.map(|i| (i.stage == ItemStage::ReadyWait, None)),
        );
        tally(
            "active_sweeps_host_le_building",
            f.active_sweeps_host.is_some(),
            f.active_sweeps_host
                .map(|l| (i64::from(l) <= state.building as i64, None)),
        );
        tally(
            "labels",
            f.labels.is_some(),
            f.labels.as_ref().and_then(|l| {
                issue.map(|i| {
                    let mut logged = l.clone();
                    logged.sort();
                    (logged == i.labels, None)
                })
            }),
        );
        tally(
            "issue_age_sec",
            f.issue_age_sec.is_some(),
            f.issue_age_sec
                .and_then(|l| issue.map(|i| (i.opened_at, l)))
                .map(|(opened, l)| {
                    let rec = (e.as_of - opened).num_seconds();
                    ((rec - l).abs() <= TIME_TOLERANCE_SEC, Some((rec - l) as f64))
                }),
        );
        tally(
            "repo_pr_open_skip",
            f.repo_pr_open_skip.is_some(),
            f.repo_pr_open_skip
                .and_then(|l| state.pr_open_skip_lockout.map(|r| (r == l, None))),
        );
        // The logged value was read at `pr_friction_observed_at` (at or
        // before `as_of`); the reconstruction is at `as_of`. A logged `none`
        // has no reconstructed counterpart: the cache cannot tell "no runs"
        // from "an earlier head's runs", so it disagrees or is unreconstructed.
        tally(
            "pr_ci_status",
            f.pr_ci_status.is_some(),
            f.pr_ci_status
                .as_deref()
                .and_then(|l| pr.and_then(|p| p.ci.as_deref()).map(|r| (r == l, None))),
        );
        tally(
            "pr_created_at",
            f.pr_created_at.is_some(),
            f.pr_created_at.and_then(|l| {
                pr.map(|p| (within(p.opened_at, l), Some((p.opened_at - l).num_seconds() as f64)))
            }),
        );
    }
    for f in report.features.values_mut() {
        f.finish();
    }
    report
}

/// Plain-text rendering of a report.
#[must_use]
pub fn render(report: &AgreementReport) -> String {
    use std::fmt::Write;
    let mut out = String::new();
    let _ = writeln!(out, "fleet-state agreement {} ({} estimates)", report.repo, report.estimates);
    if let (Some(a), Some(b)) = (report.from, report.to) {
        let _ = writeln!(out, "  window: {} .. {}", a.to_rfc3339(), b.to_rfc3339());
    }
    for (name, f) in &report.features {
        let rate = f
            .rate
            .map_or("n/a".to_string(), |r| format!("{:.1}%", r * 100.0));
        let diff = f
            .mean_signed_diff
            .map_or(String::new(), |d| format!("  mean(reconstructed-logged)={d:+.2}"));
        let _ = writeln!(
            out,
            "  {name:<32} {rate:>7} ({}/{}){diff}  unreconstructed={}",
            f.agreed, f.compared, f.unreconstructed
        );
    }
    let _ = writeln!(
        out,
        "  not reconstructable from forge events: {}",
        report.not_reconstructable.join(", ")
    );
    out
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::eta::explanation::Features;
    use crate::eta::fleet_events::EventKind;

    #[test]
    fn no_explanations_is_an_empty_report() {
        let r = agreement(&[], "o/r", &[]);
        assert_eq!(r.estimates, 0);
        assert!(r.features.is_empty());
        assert!(render(&r).contains("0 estimates"));
    }

    #[test]
    fn junk_lines_are_counted_not_fatal() {
        let (v, skipped) = parse_explanations("not json\n\n{\"a\":1}\n");
        assert!(v.is_empty());
        assert_eq!(skipped, 2);
    }

    fn explanation(as_of: &str, features: Features) -> Explanation {
        let mut e: Explanation = serde_json::from_value(serde_json::json!({
            "schema": "eta-explanation/v1",
            "estimate_id": "x",
            "heuristic": "start-v1",
            "kind": "start",
            "loom": {"version": "0", "revision": "unknown", "tree_state": "unknown", "complete": false},
            "as_of": as_of,
            "subject": {"repo": "o/r", "repo_id": null, "issue": 7, "pr_number": null,
                        "story": "o/r#7", "sweep_id": null},
            "stages": [],
            "features_omitted": [],
            "truncated": []
        }))
        .unwrap();
        e.features = Some(features);
        e
    }

    #[test]
    fn a_ready_issue_with_retained_curation_labels_agrees_on_queue_features() {
        use crate::eta::fleet_events::SOURCE_FORGE;
        let at = chrono::DateTime::parse_from_rfc3339("2026-09-01T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let ev = |what, label: Option<&str>, secs: i64, seq| {
            RawEvent::new(
                "o/r",
                7,
                ItemKind::Issue,
                what,
                label.map(str::to_string),
                at + chrono::Duration::seconds(secs),
                SOURCE_FORGE,
                seq,
                at,
            )
        };
        let events = vec![
            ev(EventKind::Opened, None, 0, 0),
            ev(EventKind::LabelAdded, Some("loom:curated"), 10, 1),
            ev(EventKind::LabelAdded, Some("loom:issue"), 20, 2),
        ];
        let f = Features {
            queue_ready: Some(1),
            queue_rank: Some(1),
            ..Features::default()
        };
        let ex = explanation("2026-09-01T00:10:00Z", f);
        let r = agreement(&events, "o/r", &[ex]);
        assert_eq!(r.features["queue_ready"].agreed, 1);
        assert_eq!(r.features["queue_rank_present"].agreed, 1);
        // No closing references cached: the lockout is not scored, only
        // counted as unreconstructed.
        let ex = explanation(
            "2026-09-01T00:10:00Z",
            Features {
                repo_pr_open_skip: Some(true),
                ..Features::default()
            },
        );
        let r = agreement(&events, "o/r", &[ex]);
        assert_eq!(r.features["repo_pr_open_skip"].compared, 0);
        assert_eq!(r.features["repo_pr_open_skip"].unreconstructed, 1);
    }

    #[test]
    fn the_open_pr_lockout_is_scored_once_closing_refs_are_cached() {
        use crate::eta::fleet_events::SOURCE_FORGE;
        let at = chrono::DateTime::parse_from_rfc3339("2026-09-01T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let ev = |item, kind, what, label: Option<&str>, secs: i64, seq| {
            RawEvent::new(
                "o/r",
                item,
                kind,
                what,
                label.map(str::to_string),
                at + chrono::Duration::seconds(secs),
                SOURCE_FORGE,
                seq,
                at,
            )
        };
        let events = vec![
            ev(7, ItemKind::Issue, EventKind::Opened, None, 0, 0),
            ev(7, ItemKind::Issue, EventKind::LabelAdded, Some("loom:issue"), 20, 2),
            ev(9, ItemKind::Pr, EventKind::Opened, None, 30, 0),
            ev(9, ItemKind::Pr, EventKind::ClosingRef, None, 30, 900).with_target(Some(7)),
        ];
        let logged = |skip| {
            explanation(
                "2026-09-01T00:10:00Z",
                Features {
                    repo_pr_open_skip: Some(skip),
                    ..Features::default()
                },
            )
        };
        let r = agreement(&events, "o/r", &[logged(true), logged(false)]);
        let f = &r.features["repo_pr_open_skip"];
        assert_eq!((f.compared, f.agreed, f.unreconstructed), (2, 1, 0));
    }

    #[test]
    fn the_pr_ci_status_is_scored_against_cached_check_runs() {
        use crate::eta::fleet_events::SOURCE_FORGE;
        let at = chrono::DateTime::parse_from_rfc3339("2026-09-01T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let ev = |item, kind, what, label: Option<&str>, secs: i64, seq| {
            RawEvent::new(
                "o/r",
                item,
                kind,
                what,
                label.map(str::to_string),
                at + chrono::Duration::seconds(secs),
                SOURCE_FORGE,
                seq,
                at,
            )
        };
        let events = vec![
            ev(7, ItemKind::Issue, EventKind::Opened, None, 0, 0),
            ev(9, ItemKind::Pr, EventKind::Opened, None, 30, 0),
            ev(9, ItemKind::Pr, EventKind::HeadCommit, Some("h"), 35, 0),
            ev(9, ItemKind::Pr, EventKind::CheckRun, Some("started:test"), 40, 55)
                .with_commit(Some("h".to_string())),
            ev(9, ItemKind::Pr, EventKind::CheckRun, Some("failure:test"), 900, 55)
                .with_commit(Some("h".to_string())),
        ];
        let logged = |as_of: &str, ci: &str| {
            let mut e = explanation(
                as_of,
                Features {
                    pr_ci_status: Some(ci.to_string()),
                    ..Features::default()
                },
            );
            e.subject.pr_number = Some(9);
            e
        };
        let r = agreement(
            &events,
            "o/r",
            &[
                // Running at 00:10, failed by 00:20.
                logged("2026-09-01T00:10:00Z", "pending"),
                logged("2026-09-01T00:20:00Z", "failing"),
                logged("2026-09-01T00:20:00Z", "passing"),
                // Before the run started: unknown, not scored.
                logged("2026-09-01T00:00:35Z", "none"),
            ],
        );
        let f = &r.features["pr_ci_status"];
        assert_eq!((f.compared, f.agreed, f.unreconstructed), (3, 2, 1));
    }
}
