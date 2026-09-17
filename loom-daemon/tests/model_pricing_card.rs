//! The pricing card has exactly one implementation and two entry points.
//! This test file holds them together (#8060).
//!
//! `activity::resource_usage::ModelPricing::for_model` is the table.
//! `script_helpers::sweep_experiment::model_pricing` is the experiment
//! harvest's view of it, and it *delegates* — but delegation alone was never
//! enough: until #8060 the harvest normalized its input to a family stem on
//! the way in (`Some("opus" | "fable") => "claude-opus-"`), so the two sites
//! disagreed for every Fable arm and for every pinned generation while both
//! sincerely "shared one table". A test that only checks the table cannot see
//! that; only a test that drives both entry points with the same inputs can.
//!
//! These live in `tests/` rather than inline because
//! `script_helpers/sweep_experiment.rs` is over the file-size ratchet's
//! threshold and may not grow (`.loom/docs/file-size-policy.md`).

use loom_daemon::activity::resource_usage::ModelPricing;
use loom_daemon::script_helpers::sweep_experiment::model_pricing;

/// Every model string the harvest can see, including bare arm aliases and a
/// deliberately unrecognized one.
const CASES: &[&str] = &[
    "sonnet",
    "opus",
    "haiku",
    "fable",
    "claude-sonnet-5",
    "claude-sonnet-4-6",
    "claude-opus-5",
    "claude-opus-4-8",
    "claude-opus-4-1",
    "claude-fable-5-1",
    "claude-fable-5",
    "claude-haiku-4-5",
    "claude-haiku-4-5-20251001",
    "mystery",
];

#[test]
fn both_pricing_entry_points_agree() {
    for id in CASES {
        let p = ModelPricing::for_model(id);
        assert_eq!(
            model_pricing(Some(id)),
            (
                p.input_cost_per_1k,
                p.output_cost_per_1k,
                p.cache_read_cost_per_1k,
                p.cache_write_cost_per_1k,
            ),
            "sweep_experiment::model_pricing disagrees with ModelPricing::for_model for '{id}'"
        );
    }
    // `None` takes the same path as an empty model string.
    assert_eq!(model_pricing(None), model_pricing(Some("")));
}

#[test]
fn harvest_sees_the_generation_keyed_rates_not_a_family_stem() {
    // A pinned ID must NOT be collapsed to a family stem on the way in.
    assert!((model_pricing(Some("claude-opus-4-8")).0 - 0.005).abs() < f64::EPSILON);
    // Retired Opus 4.1 keeps the retired rate — the generation matters.
    assert!((model_pricing(Some("claude-opus-4-1")).0 - 0.015).abs() < f64::EPSILON);
    // Unknown → the newest Sonnet row (never the oldest, never a retired one).
    assert!((model_pricing(Some("mystery")).0 - 0.002).abs() < f64::EPSILON);
}

#[test]
fn fable_is_priced_as_fable_not_as_opus() {
    // #3702's "no published per-token rate, price it as Opus" premise expired:
    // Fable is on the published card at 2x Opus, so the old collapse
    // over-reported rather than erring conservatively upward.
    let fable = model_pricing(Some("fable"));
    let opus = model_pricing(Some("opus"));
    assert!((fable.0 - 0.010).abs() < f64::EPSILON, "fable input {}", fable.0);
    assert!((fable.1 - 0.050).abs() < f64::EPSILON, "fable output {}", fable.1);
    assert!((fable.0 - 2.0 * opus.0).abs() < f64::EPSILON);
    assert!((fable.1 - 2.0 * opus.1).abs() < f64::EPSILON);
}
