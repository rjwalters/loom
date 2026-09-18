//! Judge/Doctor signals read off a PR's **forge label timeline** (Issue #8222).
//!
//! # Why the label timeline, and not the sampled phase history
//!
//! `sweep.outcome`'s `doctor_cycles` originally shipped (#8056) as a documented
//! *interim proxy*: a count of `doctor-*` markers in the phase-transition
//! history the reaper samples off the on-disk checkpoint on a ~30s tick. That
//! sampler is lossy by construction — a phase that opens and closes between two
//! ticks is invisible — so the count was a lower bound, and `judge_verdicts`
//! was deliberately NOT shipped on the same source at all: a first-pass
//! approval rate computed from a lossy sample is worse than no number, because
//! a silently-low verdict count reads as a silently-high approval rate.
//!
//! The forge's own label timeline has neither problem. Every Judge verdict is a
//! durable `labeled` event (`loom:pr` on approval, `loom:changes-requested` on
//! a change request), written by the Judge itself, retained by the forge, and
//! readable long after the sweep's checkpoint is deleted. It is also **not** an
//! agent-written stats file — the #4809 lesson that a skill-written file never
//! materializes in a headless child.
//!
//! # The label vocabulary this reads
//!
//! One Judge attempt is a `loom:review-requested` arrival (Builder opening the
//! PR, or Doctor handing a fixed PR back). Its verdict is the next
//! `loom:pr` (pass) or `loom:changes-requested` (fail) arrival. A Doctor cycle
//! is a `loom:changes-requested` arrival that is later followed by another
//! `loom:review-requested` arrival — i.e. a rejection that a Doctor actually
//! closed the loop on, which is the same event the retired `doctor-done`
//! checkpoint marker stood for. A terminal rejection (the sweep hit the
//! Doctor-cycle cap, or died) is therefore NOT counted as a cycle.
//!
//! # Which PR
//!
//! The record covers exactly the PR named by the same record's `pr_number` —
//! the latest PR this sweep's own checkpoint recorded. A sweep that opened more
//! than one PR (a re-dispatch after a park, a partial-increment slice) reports
//! its **latest** PR and never aggregates across PRs, so a consumer joining
//! `sweep.outcome` to a PR gets an exact, single-PR answer rather than a blend.
//! A sweep with no known PR has no timeline to read and reports neither field.
//!
//! # Cost and fail-open posture
//!
//! The read happens once per terminal transition (never per reaper tick) and is
//! a **REST** `gh api .../timeline` call — the independent, larger pool, not the
//! GraphQL one every agent-side `gh pr view` burns — gated behind the fleet
//! rate-limit breaker ([`crate::rate_limit_breaker`], epic #4432) and bounded by
//! the reaper's own `gh` timeout. An ETag conditional request (the
//! [`crate::forge_listing`] mechanism) is deliberately NOT used: each PR's
//! timeline is read exactly once, at that PR's sweep's terminal transition, so a
//! conditional cache would never serve a hit and would only add a round trip's
//! worth of bookkeeping.
//!
//! Every failure — breaker suppressed, spawn error, timeout, non-zero exit,
//! unparseable output — yields `None`, which the caller turns into **absent**
//! `judge_verdicts` / `doctor_cycles` keys. Never a fabricated `0`/`[]`, and
//! never a blocked or failed journal append.

use super::*;

/// The label a Builder (or a Doctor handing back a fixed PR) applies to open a
/// Judge attempt.
pub(crate) const REVIEW_REQUESTED_LABEL: &str = "loom:review-requested";

/// The label a Judge applies on approval — the `pass` verdict.
pub(crate) const APPROVED_LABEL: &str = "loom:pr";

/// The label a Judge applies when requesting changes — the `fail` verdict.
pub(crate) const CHANGES_REQUESTED_LABEL: &str = "loom:changes-requested";

/// [`telemetry::JudgeVerdict::verdict`] for an approval.
pub(crate) const VERDICT_PASS: &str = "pass";

/// [`telemetry::JudgeVerdict::verdict`] for a change request.
pub(crate) const VERDICT_FAIL: &str = "fail";

/// Defensive cap on retained verdicts per PR, mirroring
/// [`MAX_PHASE_OBSERVATIONS`]'s role for the sampled history: the Doctor-cycle
/// ladder bounds a normal PR to a handful of verdicts, so this only ever
/// truncates a pathological label-flapping timeline, never a real lifecycle.
pub(crate) const MAX_JUDGE_VERDICTS: usize = 32;

/// One `labeled` event from a PR's forge timeline, trimmed to what the verdict
/// reconstruction needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LabelEvent {
    /// RFC-3339 instant the label was applied. Kept as the parsed timestamp so
    /// events from separate `--paginate` pages can be merged in true
    /// chronological order rather than in page order.
    pub(crate) at: DateTime<Utc>,
    /// The label name (`loom:review-requested`, `loom:pr`, …).
    pub(crate) label: String,
}

/// What one PR's label timeline says about its Judge/Doctor history.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct TimelineSignals {
    /// Every Judge verdict on the PR, in lifecycle order. Empty (not absent —
    /// the absent case is the caller's `None`) when the timeline was read and
    /// carried no verdict at all, e.g. a PR whose sweep died before Judge.
    pub(crate) judge_verdicts: Vec<telemetry::JudgeVerdict>,
    /// Completed Doctor cycles — see the module doc's definition.
    pub(crate) doctor_cycles: u32,
}

/// The `--jq` program handed to `gh api`: one `<rfc3339>\t<label>` line per
/// `labeled` event. Projecting on the wire keeps the piped payload small on a
/// long timeline, and the tab-separated shape is unambiguous (a label name can
/// contain neither a tab nor a newline).
pub(crate) const TIMELINE_JQ: &str =
    r#".[] | select(.event == "labeled") | "\(.created_at)\t\(.label.name)""#;

/// Parse [`TIMELINE_JQ`]'s stdout into chronologically-ordered events.
///
/// `--paginate` re-runs the `--jq` program per page and concatenates the
/// results (#4637), so multi-page output is several independently-ordered
/// blocks; sorting by the parsed timestamp merges them correctly. Lines that do
/// not parse are skipped individually rather than failing the whole read — a
/// single malformed row must not erase an otherwise-complete history.
///
/// Empty output and output with no parseable line both yield an empty vec:
/// "this PR has no `labeled` events" is a legitimate observation, and the
/// caller's "the read failed" signal is its own `None`, never this one.
#[must_use]
pub(crate) fn parse_label_events(stdout: &[u8]) -> Vec<LabelEvent> {
    let raw = String::from_utf8_lossy(stdout);
    let mut events: Vec<LabelEvent> = raw
        .lines()
        .filter_map(|line| {
            let (at, label) = line.trim_end_matches('\r').split_once('\t')?;
            let at = DateTime::parse_from_rfc3339(at.trim().trim_matches('"'))
                .ok()?
                .with_timezone(&Utc);
            let label = label.trim();
            (!label.is_empty()).then(|| LabelEvent {
                at,
                label: label.to_string(),
            })
        })
        .collect();
    // Stable sort: two events sharing a timestamp (the forge stamps whole
    // seconds, and a Judge's remove+add pair lands inside one) keep the order
    // the forge returned them in, which is their true application order.
    events.sort_by_key(|e| e.at);
    events
}

/// Reconstruct the Judge/Doctor history from a PR's `labeled` events.
///
/// `attempt` is **1-based per PR**, counting `loom:review-requested` arrivals:
/// the verdict on the PR as first opened is attempt 1, the verdict after the
/// first Doctor hand-back is attempt 2, and so on. A verdict observed before
/// any `loom:review-requested` arrival (a PR whose opening label predates the
/// pipeline, or whose label was applied by an API call the timeline renders
/// differently) is attributed to attempt 1 rather than dropped — the verdict
/// itself is the load-bearing fact.
///
/// A verdict identical to the previous one *within the same attempt* is
/// ignored: a Judge that removes and re-applies its own label, or a Champion
/// pass that re-adds `loom:pr`, is one verdict, not two.
#[must_use]
pub(crate) fn signals_from_events(events: &[LabelEvent]) -> TimelineSignals {
    let mut out = TimelineSignals::default();
    let mut attempt: u32 = 0;
    // Whether a rejection is still waiting to be handed back, so a
    // `loom:review-requested` arrival after it can settle it as a COMPLETED
    // Doctor cycle.
    let mut open_rejection = false;
    for event in events {
        match event.label.as_str() {
            REVIEW_REQUESTED_LABEL => {
                attempt = attempt.saturating_add(1);
                if open_rejection {
                    // The rejection that preceded this hand-back was actually
                    // worked: one completed Doctor cycle.
                    out.doctor_cycles = out.doctor_cycles.saturating_add(1);
                    open_rejection = false;
                }
            }
            APPROVED_LABEL => {
                push_verdict(&mut out, attempt, VERDICT_PASS);
            }
            CHANGES_REQUESTED_LABEL => {
                // A deduped repeat of the same rejection does not re-open the
                // cycle — it is the same rejection, already pending.
                open_rejection |= push_verdict(&mut out, attempt, VERDICT_FAIL);
            }
            _ => {}
        }
    }
    out
}

/// Append one verdict for `attempt`, deduping a repeat of the same verdict
/// within the same attempt. Returns whether a new verdict was recorded.
fn push_verdict(out: &mut TimelineSignals, attempt: u32, verdict: &str) -> bool {
    let attempt = attempt.max(1);
    if out
        .judge_verdicts
        .last()
        .is_some_and(|v| v.attempt == attempt && v.verdict == verdict)
    {
        return false;
    }
    if out.judge_verdicts.len() >= MAX_JUDGE_VERDICTS {
        return false;
    }
    out.judge_verdicts.push(telemetry::JudgeVerdict {
        attempt,
        verdict: verdict.to_string(),
    });
    true
}

impl SweepRegistry {
    /// Best-effort [`TimelineSignals`] for `pr_number` (Issue #8222), or `None`
    /// when the forge was not successfully consulted — see the module doc for
    /// the full fail-open contract. `None` is what makes the record's
    /// `judge_verdicts`/`doctor_cycles` keys **absent** rather than a
    /// fabricated `[]`/`0`.
    ///
    /// Skipped outright (never shelling to `gh`) when `skip_label_flip` is set,
    /// matching every other real-forge probe on this path.
    pub(crate) fn fetch_timeline_signals(&self, pr_number: u32) -> Option<TimelineSignals> {
        if self.config.skip_label_flip {
            return None;
        }
        // Fleet rate-limit breaker (epic #4432): while the breaker is
        // suppressing forge polling, a best-effort telemetry read is exactly
        // the kind of call that must stand down — the journal line is still
        // written, just without these two fields.
        if crate::rate_limit_breaker::global_is_suppressed() {
            log::debug!(
                "sweep_outcomes: skipping the PR #{pr_number} label-timeline read — the \
                 rate-limit breaker is suppressing forge polling (#8222)"
            );
            return None;
        }
        let gh = self
            .config
            .gh_bin
            .clone()
            .unwrap_or_else(|| PathBuf::from("gh"));
        let mut cmd = Command::new(&gh);
        cmd.arg("api")
            .arg(format!("repos/{{owner}}/{{repo}}/issues/{pr_number}/timeline"))
            .arg("--paginate")
            .arg("--jq")
            .arg(TIMELINE_JQ);
        // Resolve the PR against THIS registry's repo, not the daemon's cwd
        // repo (#3937), and honor a cross-owner managed repo's own
        // installation-token config dir (#5401).
        cmd.current_dir(&self.config.workspace_root);
        crate::credential_preflight::apply_gh_config_for_root(
            &mut cmd,
            &self.config.workspace_root,
        );
        if let Ok(repo) = std::env::var("LOOM_REPO") {
            cmd.arg("--repo").arg(repo);
        }
        let output = match output_with_timeout(cmd, reap_gh_timeout()) {
            Ok(Some(o)) if o.status.success() => o,
            Ok(Some(o)) => {
                let stderr = String::from_utf8_lossy(&o.stderr).trim().to_string();
                // Feed the breaker the raw stderr so a rate-limit signature
                // trips it for every other forge consumer too, exactly as the
                // polling loops' own error arms do.
                crate::rate_limit_breaker::global_observe_failure(
                    &stderr,
                    "sweep_outcome_label_timeline",
                );
                // `warn`, not `debug`: an *unexpected* failure is the case a
                // consumer needs to correlate a silently-absent field with, and
                // it fires at most once per sweep (never per reaper tick). The
                // EXPECTED omissions — no PR, breaker suppressed,
                // `skip_label_flip` — stay quiet or `debug`.
                log::warn!(
                    "sweep_outcomes: PR #{pr_number} label-timeline read failed ({}) — omitting \
                     judge_verdicts/doctor_cycles, record still written (#8222): {stderr}",
                    o.status
                );
                return None;
            }
            Ok(None) => {
                log::warn!(
                    "sweep_outcomes: PR #{pr_number} label-timeline read timed out — omitting \
                     judge_verdicts/doctor_cycles, record still written (#8222)"
                );
                return None;
            }
            Err(e) => {
                log::warn!(
                    "sweep_outcomes: could not invoke {} for PR #{pr_number}'s label timeline: \
                     {e} — omitting judge_verdicts/doctor_cycles, record still written (#8222)",
                    gh.display()
                );
                return None;
            }
        };
        Some(signals_from_events(&parse_label_events(&output.stdout)))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic, clippy::expect_used)]
mod tests {
    use super::*;

    fn events(rows: &[(&str, &str)]) -> Vec<LabelEvent> {
        let joined: String = rows
            .iter()
            .map(|(at, label)| format!("{at}\t{label}\n"))
            .collect();
        parse_label_events(joined.as_bytes())
    }

    /// The `--jq` projection parses into chronological events, and rows from
    /// separate `--paginate` pages (out of order on the wire) are merged by
    /// timestamp rather than left in page order.
    #[test]
    fn parses_and_chronologically_merges_paginated_rows() {
        let stdout = "2026-09-18T12:05:00Z\tloom:pr\n\
                      2026-09-18T12:00:00Z\tloom:review-requested\n";
        let parsed = parse_label_events(stdout.as_bytes());
        assert_eq!(
            parsed.iter().map(|e| e.label.as_str()).collect::<Vec<_>>(),
            vec![REVIEW_REQUESTED_LABEL, APPROVED_LABEL],
        );
    }

    /// A malformed row is skipped individually; it never erases the rest of an
    /// otherwise-complete history.
    #[test]
    fn skips_unparseable_rows_without_discarding_the_history() {
        let stdout = "not-a-timestamp\tloom:pr\n\
                      \n\
                      2026-09-18T12:00:00Z\tloom:review-requested\n\
                      2026-09-18T12:01:00Z\t\n";
        let parsed = parse_label_events(stdout.as_bytes());
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].label, REVIEW_REQUESTED_LABEL);
    }

    /// AC: a first-pass approval is `[{attempt: 1, verdict: "pass"}]` — the
    /// shape "first-pass judge approval rate" is computed from.
    #[test]
    fn first_pass_approval_is_attempt_one_pass() {
        let signals = signals_from_events(&events(&[
            ("2026-09-18T12:00:00Z", REVIEW_REQUESTED_LABEL),
            ("2026-09-18T12:30:00Z", APPROVED_LABEL),
        ]));
        assert_eq!(
            signals.judge_verdicts,
            vec![telemetry::JudgeVerdict {
                attempt: 1,
                verdict: VERDICT_PASS.to_string(),
            }]
        );
        assert_eq!(signals.doctor_cycles, 0);
    }

    /// A rejection, a Doctor hand-back, then an approval: two verdicts with
    /// 1-based per-PR attempt numbers, and exactly one COMPLETED Doctor cycle.
    #[test]
    fn rejection_then_handback_then_approval_counts_one_doctor_cycle() {
        let signals = signals_from_events(&events(&[
            ("2026-09-18T12:00:00Z", REVIEW_REQUESTED_LABEL),
            ("2026-09-18T12:30:00Z", CHANGES_REQUESTED_LABEL),
            ("2026-09-18T13:00:00Z", REVIEW_REQUESTED_LABEL),
            ("2026-09-18T13:30:00Z", APPROVED_LABEL),
        ]));
        assert_eq!(
            signals.judge_verdicts,
            vec![
                telemetry::JudgeVerdict {
                    attempt: 1,
                    verdict: VERDICT_FAIL.to_string(),
                },
                telemetry::JudgeVerdict {
                    attempt: 2,
                    verdict: VERDICT_PASS.to_string(),
                },
            ]
        );
        assert_eq!(signals.doctor_cycles, 1);
    }

    /// A rejection nobody ever handed back (the Doctor-cycle cap, or a dead
    /// sweep) is a verdict but NOT a Doctor cycle — the cycle count means
    /// "a Doctor closed the loop", the same fact the retired `doctor-done`
    /// marker stood for.
    #[test]
    fn a_terminal_rejection_is_not_a_doctor_cycle() {
        let signals = signals_from_events(&events(&[
            ("2026-09-18T12:00:00Z", REVIEW_REQUESTED_LABEL),
            ("2026-09-18T12:30:00Z", CHANGES_REQUESTED_LABEL),
        ]));
        assert_eq!(signals.judge_verdicts.len(), 1);
        assert_eq!(signals.judge_verdicts[0].verdict, VERDICT_FAIL);
        assert_eq!(signals.doctor_cycles, 0);
    }

    /// A re-applied identical label inside one attempt is one verdict, not two
    /// — a Judge (or Champion) re-adding its own label must not inflate the
    /// verdict list or the first-pass denominator.
    #[test]
    fn a_repeated_identical_verdict_within_one_attempt_is_deduped() {
        let signals = signals_from_events(&events(&[
            ("2026-09-18T12:00:00Z", REVIEW_REQUESTED_LABEL),
            ("2026-09-18T12:30:00Z", APPROVED_LABEL),
            ("2026-09-18T12:31:00Z", APPROVED_LABEL),
        ]));
        assert_eq!(signals.judge_verdicts.len(), 1);
    }

    /// A verdict with no preceding `loom:review-requested` arrival is still
    /// attributed to attempt 1 — the verdict is the load-bearing fact, and
    /// dropping it would silently deflate the approval-rate numerator.
    #[test]
    fn a_verdict_without_an_observed_attempt_opens_attempt_one() {
        let signals = signals_from_events(&events(&[("2026-09-18T12:30:00Z", APPROVED_LABEL)]));
        assert_eq!(signals.judge_verdicts[0].attempt, 1);
    }

    /// A timeline that was READ but carries no verdict is an empty list, not a
    /// failure: the caller distinguishes it from an unreadable timeline by its
    /// own `Option`, which is what keeps "observed, none" apart from "unknown".
    #[test]
    fn an_observed_timeline_with_no_verdict_is_empty_not_absent() {
        let signals =
            signals_from_events(&events(&[("2026-09-18T12:00:00Z", REVIEW_REQUESTED_LABEL)]));
        assert!(signals.judge_verdicts.is_empty());
        assert_eq!(signals.doctor_cycles, 0);
    }

    /// Unrelated labels on the same PR (`loom:treating`, `tier:*`, …) never
    /// open an attempt or record a verdict.
    #[test]
    fn unrelated_labels_are_ignored() {
        let signals = signals_from_events(&events(&[
            ("2026-09-18T12:00:00Z", "loom:treating"),
            ("2026-09-18T12:01:00Z", "tier:goal-supporting"),
        ]));
        assert_eq!(signals, TimelineSignals::default());
    }

    /// The verdict list is capped like the sampled phase history is — a
    /// pathologically flapping timeline cannot grow the record without bound.
    #[test]
    fn verdicts_are_capped() {
        let base = DateTime::parse_from_rfc3339("2026-09-18T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let mut flapping = Vec::new();
        for i in 0..(MAX_JUDGE_VERDICTS as i64 + 10) {
            flapping.push(LabelEvent {
                at: base + chrono::Duration::minutes(i * 2),
                label: REVIEW_REQUESTED_LABEL.to_string(),
            });
            flapping.push(LabelEvent {
                at: base + chrono::Duration::minutes(i * 2 + 1),
                label: APPROVED_LABEL.to_string(),
            });
        }
        let signals = signals_from_events(&flapping);
        assert_eq!(signals.judge_verdicts.len(), MAX_JUDGE_VERDICTS);
    }
}
