//! Rendering for `loom-daemon pr-latency` (Issue #8923): the human report, the
//! `--json` document, and the `--advise` warning.
//!
//! One rule runs through all three: **an absence prints as `—`, never as 0.**
//! A zero in a latency table reads as "instant"; the thing this issue exists to
//! fix started as exactly that class of misreading.

use loom_daemon::pr_latency::report::LatencyReport;
use loom_daemon::pr_latency::segments::Treating;
use loom_daemon::pr_latency::stats::{hours, Distribution};

/// The full human report.
pub(crate) fn report_text(r: &LatencyReport, list_error: Option<&str>) -> String {
    let mut out = String::new();
    out.push_str("PR latency by segment (#8923)\n");
    if let Some(why) = list_error {
        out.push_str(&format!(
            "  WARNING: enumerating PRs did not fully answer: {why}\n  \
             The sample below is INCOMPLETE — this is unknown, not healthy.\n"
        ));
    }
    out.push_str(&format!(
        "  sample: {} PRs ({} merged, {} open, {} closed unmerged){}\n",
        r.prs_examined,
        r.prs_merged,
        r.prs_open,
        r.prs_closed_unmerged,
        if r.prs_incomplete.is_empty() {
            String::new()
        } else {
            format!(
                "; {} excluded from the distributions (unreadable timeline): {}",
                r.prs_incomplete.len(),
                join_numbers(&r.prs_incomplete)
            )
        }
    ));
    out.push_str(
        "  Every figure is DWELL between two observed events, never PR age.\n\
         \n  Segment                                    n     p50     p90     max\n",
    );
    push_row(&mut out, "PL1 review-requested -> verdict", &r.review_wait);
    push_row(&mut out, "PL2 first verdict -> loom:pr", &r.approval_path);
    push_row(&mut out, "PL3a loom:pr -> merged, OPERATOR-GATED", &r.merge_wait_gated);
    push_row(&mut out, "PL3b loom:pr -> merged, ungated", &r.merge_wait_ungated);
    push_row(&mut out, "PL4 changes-requested -> next push", &r.doctor_response);
    if r.merged_without_approval_label > 0 {
        out.push_str(&format!(
            "  ({} merged PR(s) carried no loom:pr labeling — PL3 unmeasured, not 0)\n",
            r.merged_without_approval_label
        ));
    }

    out.push_str(&format!(
        "\nPL5a approval invalidations (re-review of already-approved work): {}\n\
         PL5b repair laps (review re-requested after a rejection — healthy): {}\n",
        r.approval_invalidations, r.repair_laps
    ));
    if r.invalidated_prs.is_empty() {
        out.push_str("  (no approval was invalidated in this sample)\n");
    } else {
        for (pr, n) in r.invalidated_prs.iter().take(10) {
            out.push_str(&format!("  #{pr}: {n}\n"));
        }
    }

    out.push_str("\nPL6 live queues (open PRs, by dwell)\n");
    for label in LatencyReport::queue_labels() {
        let rows = r.queue(label);
        let d = r.queue_dwell(label);
        out.push_str(&format!(
            "  {label}: {} open, dwell p50 {}, max {}\n",
            rows.len(),
            hours(d.p50_secs),
            hours(d.max_secs)
        ));
        for row in rows {
            let mut flags: Vec<String> = Vec::new();
            if row.operator_gated {
                flags.push("OPERATOR-GATED".into());
            }
            if row.parked {
                flags.push("parked".into());
            }
            if row.treating == Treating::InProgress {
                flags.push("treating".into());
            }
            flags.extend(row.holds.iter().cloned());
            out.push_str(&format!(
                "    #{:<6} dwell {:>7}  age {:>7}  {}\n",
                row.pr,
                hours(row.dwell_secs),
                hours(Some(row.age_secs)),
                flags.join(", ")
            ));
        }
    }

    out.push_str(&format!(
        "\nDoctor backlog: open loom:changes-requested with NO push since the label: {}\n",
        r.doctor_backlog.len()
    ));
    for row in &r.doctor_backlog {
        out.push_str(&format!(
            "  #{:<6} dwell {:>7}  {}{}\n",
            row.pr,
            hours(row.dwell_secs),
            match row.treating {
                Treating::Queued => "queued for Doctor",
                Treating::InProgress => "Doctor is treating it now",
                Treating::Parked => "PARKED — out of Doctor's reach",
            },
            if row.holds.is_empty() {
                String::new()
            } else {
                format!(" [{}]", row.holds.join(", "))
            }
        ));
    }
    out.push_str("\nDefinitions and what this cannot answer: .loom/docs/pr-latency.md\n");
    out
}

fn push_row(out: &mut String, name: &str, d: &Distribution) {
    out.push_str(&format!(
        "  {name:<40} {:>3}  {:>6}  {:>6}  {:>6}\n",
        d.n,
        hours(d.p50_secs),
        hours(d.p90_secs),
        hours(d.max_secs)
    ));
}

fn join_numbers(ns: &[u32]) -> String {
    ns.iter()
        .map(|n| format!("#{n}"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// The `--json` document. Shapes that a caller would otherwise have to
/// recompute (the gate split, the advisory populations) are materialized, so
/// nobody re-derives "which of these is gated" with a second label query.
pub(crate) fn report_json(r: &LatencyReport, list_error: Option<&str>) -> serde_json::Value {
    serde_json::json!({
        "questions_doc": ".loom/docs/pr-latency.md",
        "list_error": list_error,
        "complete": list_error.is_none() && r.prs_incomplete.is_empty(),
        "report": r,
    })
}

/// Advisory mode: warn on stderr about anything past `threshold_secs`, and
/// print the suppressible one-line stdout confirmation when clear.
///
/// Never exits non-zero and never writes a label — same contract as the four
/// sibling pre-wave checks. It reports the *silence*, not the hold: an
/// operator gate on a finished PR is a legitimate state, and this exists only
/// so that state cannot be invisible for days.
pub(crate) fn advise(
    r: &LatencyReport,
    threshold_secs: i64,
    list_error: Option<&str>,
    quiet: bool,
    json: bool,
) {
    if json {
        println!(
            "{}",
            serde_json::json!({
                "threshold_secs": threshold_secs,
                "clear": r.advisory_is_clear(threshold_secs),
                "list_error": list_error,
                "operator_holds": r.stalled_operator_holds(threshold_secs),
                "ungated_approvals": r.stalled_plain_approvals(threshold_secs),
                "awaiting_review": r.stalled_reviews(threshold_secs),
                "awaiting_doctor": r.stalled_doctor_backlog(threshold_secs),
                "queues": r.queues,
            })
        );
        return;
    }

    let t = hours(Some(threshold_secs));
    let mut w = String::new();

    if let Some(why) = list_error {
        eprintln!("[pr-latency] could not enumerate open PRs: {why}");
        eprintln!(
            "[pr-latency] reporting nothing — this is UNKNOWN, not clear (advisory; exit 0)."
        );
    }

    let gated = r.stalled_operator_holds(threshold_secs);
    let plain = r.stalled_plain_approvals(threshold_secs);
    let reviews = r.stalled_reviews(threshold_secs);
    let doctor = r.stalled_doctor_backlog(threshold_secs);
    let total = gated.len() + plain.len() + reviews.len() + doctor.len();

    if total > 0 {
        w.push('\n');
        w.push_str(&"=".repeat(72));
        w.push('\n');
        w.push_str(&format!(
            "  WARNING: {total} open PR(s) have sat in one queue for over {t} (#8923)\n"
        ));
        w.push_str(&"=".repeat(72));
        w.push('\n');
    }

    if !gated.is_empty() {
        w.push_str(&format!(
            "\nWAITING ON A PERSON ({}) — the hold is legitimate; the silence is not:\n",
            gated.len()
        ));
        for row in &gated {
            w.push_str(&format!(
                "  #{:<6} {:>7} in {} under {}\n",
                row.pr,
                hours(row.dwell_secs),
                row.queue,
                row.holds.join(", ")
            ));
        }
        w.push_str(
            "  The engine has stopped on these (loom:operator is in SKIP_LABELS), so no\n  \
             role will move them. Merge, clear the hold, or say why it stands:\n\
             \x20     ./.loom/scripts/merge-pr.sh <PR>\n",
        );
    }

    if !plain.is_empty() {
        w.push_str(&format!(
            "\nAPPROVED, NOTHING HOLDING IT ({}) — no gate, no park; the merge lane stalled:\n",
            plain.len()
        ));
        for row in &plain {
            w.push_str(&format!("  #{:<6} {:>7}\n", row.pr, hours(row.dwell_secs)));
        }
    }

    if !reviews.is_empty() {
        w.push_str(&format!(
            "\nAWAITING A JUDGE VERDICT ({}) — no gate, no park; Judge's queue:\n",
            reviews.len()
        ));
        for row in &reviews {
            w.push_str(&format!("  #{:<6} {:>7}\n", row.pr, hours(row.dwell_secs)));
        }
    }

    if !doctor.is_empty() {
        w.push_str(&format!(
            "\nREJECTED, NO DOCTOR PUSH ({}) — excludes loom:treating, parked and gated rows:\n",
            doctor.len()
        ));
        for row in &doctor {
            w.push_str(&format!("  #{:<6} {:>7}\n", row.pr, hours(row.dwell_secs)));
        }
    }

    if total > 0 {
        w.push_str(
            "\nEvery figure above is dwell in the named queue, not PR age.\n\
             Full decomposition: loom-daemon pr-latency\n",
        );
        w.push_str(&"=".repeat(72));
        w.push('\n');
        eprint!("{w}");
    }

    if quiet {
        return;
    }
    if total == 0 {
        if list_error.is_some() {
            println!("[pr-latency] could not enumerate open PRs; see stderr.");
        } else {
            println!(
                "[pr-latency] no open PR has dwelt over {t} in review, approval or repair ({} open in a queue).",
                r.queues.len()
            );
        }
    } else {
        println!(
            "[pr-latency] WARNING: {total} PR(s) over {t} in one queue ({} waiting on a person). See stderr.",
            gated.len()
        );
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use chrono::{DateTime, Utc};
    use loom_daemon::pr_latency::history::PrHistory;
    use loom_daemon::pr_latency::{
        PrEvent, PrState, APPROVED, CHANGES_REQUESTED, REVIEW_REQUESTED,
    };

    const HOUR: i64 = 3600;

    fn t(secs: i64) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-09-01T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc)
            + chrono::Duration::seconds(secs)
    }

    fn open(number: u32, labels: &[&str], events: Vec<PrEvent>) -> PrHistory {
        PrHistory::new(
            number,
            t(0),
            PrState::Open,
            None,
            labels.iter().map(|s| (*s).to_string()).collect(),
            events,
            true,
        )
    }

    fn labeled(label: &str, secs: i64) -> PrEvent {
        PrEvent::Labeled {
            label: label.into(),
            at: t(secs),
        }
    }

    #[test]
    fn an_unmeasured_segment_renders_as_a_dash_never_zero() {
        let r = LatencyReport::build(&[], t(0));
        let text = report_text(&r, None);
        assert!(text.contains("PL1 review-requested -> verdict"));
        assert!(text.contains('—'), "{text}");
        assert!(!text.contains("0.0h"), "{text}");
    }

    #[test]
    fn the_report_names_both_halves_of_the_gate_split() {
        let r = LatencyReport::build(&[], t(0));
        let text = report_text(&r, None);
        assert!(text.contains("PL3a loom:pr -> merged, OPERATOR-GATED"));
        assert!(text.contains("PL3b loom:pr -> merged, ungated"));
    }

    #[test]
    fn a_queue_row_prints_dwell_and_age_side_by_side() {
        let h = open(8531, &[APPROVED, "loom:operator"], vec![labeled(APPROVED, 80 * HOUR)]);
        let r = LatencyReport::build(&[h], t(100 * HOUR));
        let text = report_text(&r, None);
        assert!(text.contains("#8531"), "{text}");
        assert!(text.contains("dwell   20.0h"), "{text}");
        assert!(text.contains("age  100.0h"), "{text}");
        assert!(text.contains("OPERATOR-GATED"), "{text}");
    }

    #[test]
    fn a_failed_listing_is_reported_as_incomplete_not_healthy() {
        let r = LatencyReport::build(&[], t(0));
        let text = report_text(&r, Some("gh pr list exited 1"));
        assert!(text.contains("INCOMPLETE"), "{text}");
        let j = report_json(&r, Some("gh pr list exited 1"));
        assert_eq!(j["complete"], false);
    }

    #[test]
    fn advisory_json_separates_gated_from_ungated_populations() {
        let gated = open(1, &[APPROVED, "loom:operator"], vec![labeled(APPROVED, 0)]);
        let plain = open(2, &[APPROVED], vec![labeled(APPROVED, 0)]);
        let r = LatencyReport::build(&[gated, plain], t(90 * HOUR));
        let j = serde_json::json!({
            "gated": r.stalled_operator_holds(24 * HOUR).len(),
            "ungated": r.stalled_plain_approvals(24 * HOUR).len(),
        });
        assert_eq!(j["gated"], 1);
        assert_eq!(j["ungated"], 1);
        assert!(!r.advisory_is_clear(24 * HOUR));
    }

    #[test]
    fn report_json_carries_the_questions_doc_pointer() {
        let r = LatencyReport::build(&[], t(0));
        let j = report_json(&r, None);
        assert_eq!(j["questions_doc"], ".loom/docs/pr-latency.md");
        assert_eq!(j["complete"], true);
    }

    #[test]
    fn review_and_changes_queues_are_both_rendered() {
        let waiting = open(3, &[REVIEW_REQUESTED], vec![labeled(REVIEW_REQUESTED, 0)]);
        let rejected = open(4, &[CHANGES_REQUESTED], vec![labeled(CHANGES_REQUESTED, 0)]);
        let r = LatencyReport::build(&[waiting, rejected], t(5 * HOUR));
        let text = report_text(&r, None);
        assert!(text.contains("loom:review-requested: 1 open"), "{text}");
        assert!(text.contains("loom:changes-requested: 1 open"), "{text}");
        assert!(text.contains("queued for Doctor"), "{text}");
    }
}
