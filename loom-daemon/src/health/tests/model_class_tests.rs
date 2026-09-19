//! Per-model-class token counts in the `health` report's `tokens` section
//! (#8058 Phase 3).
//!
//! Split out of the parent `health/tests.rs` rather than appended to it: that
//! file is over the `.loom/docs/file-size-policy.md` threshold and therefore
//! frozen at its current size, so new cases go in a sibling module.
//!
//! A child module of `tests`, not of `health`, so it inherits the parent's
//! `healthy_inputs()` fixture and reads the same `assess_tokens` the rest of
//! the section's coverage does.

use super::*;
use crate::capacity::model_class::ClassCapacity;

/// A [`ClassCapacity`] with the given `class -> healthy` counts.
fn class_capacity(total: usize, healthy: usize, by_class: &[(&str, usize)]) -> ClassCapacity {
    ClassCapacity {
        total,
        healthy,
        by_class: by_class
            .iter()
            .map(|(c, n)| ((*c).to_string(), *n))
            .collect(),
    }
}

/// AC2's degradation clause: with no class-scoped state collected — either
/// no readable ranking (`None`) or a ranking with no class marks (an empty
/// `by_class`) — the summary line is byte-identical to its pre-#8058 form
/// and the JSON detail carries an empty object, never a fabricated count.
#[test]
fn tokens_summary_is_unchanged_without_class_scoped_state() {
    for collected in [None, Some(class_capacity(8, 6, &[]))] {
        let mut inputs = healthy_inputs();
        inputs.token_class_capacity = collected;
        let section = assess_tokens(&inputs);
        assert!(section.summary.starts_with("6/8 healthy (2 exhausted)"), "{}", section.summary);
        assert_eq!(section.detail["healthy_by_class"], serde_json::json!({}));
    }
}

/// The observability gap this phase closes: the account-wide count stays
/// exactly what it was, and the per-class breakdown printed beside it says
/// which classes still have the capacity that number is hiding.
#[test]
fn tokens_summary_reports_healthy_counts_per_class() {
    let mut inputs = healthy_inputs();
    inputs.token_class_capacity =
        Some(class_capacity(8, 6, &[("haiku", 8), ("opus", 6), ("sonnet", 8)]));
    let section = assess_tokens(&inputs);
    // Verdict and the account-wide numbers are untouched — this phase is
    // observability only, and a per-class count must never move a verdict.
    assert_eq!(section.verdict, Verdict::Green);
    assert!(
        section
            .summary
            .starts_with("6/8 healthy (per class: haiku 8/8, opus 6/8, sonnet 8/8) (2 exhausted)"),
        "{}",
        section.summary
    );
    assert_eq!(
        section.detail["healthy_by_class"],
        serde_json::json!({"haiku": 8, "opus": 6, "sonnet": 8})
    );
    // The pre-#8058 fields keep their exact meaning next to it.
    assert_eq!(section.detail["healthy"], serde_json::json!(6));
    assert_eq!(section.detail["total"], serde_json::json!(8));
}

/// End-to-end across the one seam the other cases in this file stub: a real
/// `.ranking` + `.bad_tokens` pair **on disk** is read by the same
/// [`read_class_capacity_at`] the collector calls, and its output — not a
/// hand-built [`ClassCapacity`] — is what `assess_tokens` renders.
///
/// Without this, every per-class assertion here would construct both sides of
/// the comparison from the same literal, and could not fail on a defect in the
/// collection step (a marker the reader does not normalize, a row shape it
/// skips). The `.bad_tokens` line is written in the exact format
/// `bad_tokens::scoped_reason` emits, timestamped now so its cooldown is live.
///
/// This is also the closest offline stand-in for the manual check in #8242's
/// test plan: the live CLI path additionally needs a reachable daemon whose
/// own pool carries class-scoped marks, which cannot be arranged from a test.
#[test]
fn a_pool_on_disk_renders_its_per_class_counts_end_to_end() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join(".ranking"),
        "a|available|0.1|2026-09-20T03:00:00Z\nb|available|0.2|2026-09-20T03:00:00Z\n\
         c|available|0.3|2026-09-20T03:00:00Z\nd|available|0.4|2026-09-20T03:00:00Z\n",
    )
    .unwrap();
    let now = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ");
    let marks: String = ["a", "b", "c"]
        .iter()
        .map(|n| format!("{now} {n} exhausted: out of usage credits [model-class:opus]\n"))
        .collect();
    std::fs::write(dir.path().join(".bad_tokens"), marks).unwrap();

    let collected = crate::capacity::model_class::read_class_capacity_at(dir.path())
        .expect("a readable ranking yields a snapshot");
    // The collector's own view of the pool: 1 healthy account-wide, but Opus
    // is the only class actually down.
    assert_eq!((collected.total, collected.healthy), (4, 1));

    let mut inputs = healthy_inputs();
    let cap = &mut inputs.status.as_mut().unwrap().capacity;
    cap.healthy_accounts = collected.healthy;
    cap.total_accounts = collected.total;
    cap.exhausted_accounts = collected.total - collected.healthy;
    inputs.token_class_capacity = Some(collected);

    let section = assess_tokens(&inputs);
    assert!(
        section
            .summary
            .starts_with("1/4 healthy (per class: fable 4/4, haiku 4/4, opus 1/4, sonnet 4/4)"),
        "{}",
        section.summary
    );
    assert_eq!(
        section.detail["healthy_by_class"],
        serde_json::json!({"fable": 4, "haiku": 4, "opus": 1, "sonnet": 4})
    );
}

/// A class-starved pool still reads as starved account-wide (the verdict is
/// driven by `healthy_accounts`, not by any class), while the breakdown
/// names the one class that is actually gone.
#[test]
fn a_class_breakdown_does_not_change_the_verdict() {
    let mut inputs = healthy_inputs();
    let cap = &mut inputs.status.as_mut().unwrap().capacity;
    cap.healthy_accounts = 0;
    cap.exhausted_accounts = 8;
    inputs.token_class_capacity = Some(class_capacity(8, 0, &[("opus", 0), ("sonnet", 8)]));
    let section = assess_tokens(&inputs);
    assert_eq!(section.verdict, Verdict::Degraded);
    assert!(section.summary.contains("token-starved"));
    assert!(section.summary.contains("per class: opus 0/8, sonnet 8/8"));
}
