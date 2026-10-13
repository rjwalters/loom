//! Pure tests for the weighted role draw (#11726).
use super::*;
use crate::runtime_preference::resolve::{resolve, SkipReason};
use serde_json::json;

fn layers(pairs: &[(Layer, Value)]) -> DrawConfig {
    DrawConfig::from_layers(pairs)
}

fn judge_config(draw: &Value) -> DrawConfig {
    layers(&[(Layer::RepoConfig, json!({"runtimes": {"roleDraw": {"judge": draw}}}))])
}

/// How often each key lands first among the candidates `available` admits,
/// over `n` seeds — the walk's "first available in the drawn order" pick.
fn pick_shares(
    table: &EffectiveTable,
    n: u64,
    available: impl Fn(&str) -> bool,
) -> BTreeMap<String, f64> {
    let mut counts: BTreeMap<String, u64> = BTreeMap::new();
    for seed in 0..n {
        let plan = plan("judge", 1, table, seed.wrapping_mul(0x9E37_79B9), None);
        let taps = plan.taps();
        let order = plan.order.clone();
        let resolution = resolve(
            &taps,
            |_| Ok::<(), SkipReason>(()),
            |tier, _, ()| {
                if available(&order[tier].key) {
                    Ok(())
                } else {
                    Err(SkipReason::Unavailable {
                        source: "test".into(),
                        detail: "down".into(),
                    })
                }
            },
        );
        let record = plan.finish(&resolution);
        *counts.entry(record.pick.unwrap_or_default()).or_default() += 1;
    }
    #[allow(clippy::cast_precision_loss)]
    counts
        .into_iter()
        .map(|(k, c)| (k, c as f64 / n as f64))
        .collect()
}

fn close(shares: &BTreeMap<String, f64>, key: &str, expected: f64) {
    let got = shares.get(key).copied().unwrap_or(0.0);
    assert!((got - expected).abs() < 0.02, "{key}: got {got}, expected {expected}");
}

#[test]
fn candidate_keys_parse_per_the_documented_grammar() {
    let c = Candidate::parse("claude").unwrap();
    assert_eq!((c.runtime.as_str(), c.model_profile, c.model), ("claude", None, None));
    let c = Candidate::parse(" claude/opus ").unwrap();
    assert_eq!((c.key.as_str(), c.model.as_deref()), ("claude/opus", Some("opus")));
    assert_eq!(c.tap(), Tap::runtime("claude"));
    // A native runtime's `/` segment is its model profile.
    let c = Candidate::parse("opencode/zai-flash").unwrap();
    assert_eq!(c.tap(), Tap::with_profile("opencode", "zai-flash"));
    assert_eq!(c.model, None);
    // `:` is the rolePreference tap rendering.
    let c = Candidate::parse("opencode:zai-metered").unwrap();
    assert_eq!(c.tap().to_string(), "opencode:zai-metered");
    let c = Candidate::parse("codex:seat/gpt-5").unwrap();
    assert_eq!((c.model_profile.as_deref(), c.model.as_deref()), (Some("seat"), Some("gpt-5")));
    for bad in ["", "claude/", ":p", "cla ude", "codex:"] {
        assert!(Candidate::parse(bad).is_err(), "{bad:?}");
    }
}

#[test]
fn draw_frequencies_track_the_weights() {
    let config = judge_config(&json!([{"attempt": 1, "draw":
        {"claude/opus": 80, "claude/fable": 10, "codex": 10}}]));
    let table = config.effective(config.table_for("judge", 1).unwrap());
    assert_eq!(table.total(), 100);
    let shares = pick_shares(&table, 20_000, |_| true);
    close(&shares, "claude/opus", 0.8);
    close(&shares, "claude/fable", 0.1);
    close(&shares, "codex", 0.1);
}

#[test]
fn an_unavailable_candidate_is_excluded_and_the_rest_renormalised() {
    let config = judge_config(&json!({"1": {"claude/opus": 60, "claude/fable": 20, "codex": 20}}));
    let table = config.effective(config.table_for("judge", 1).unwrap());
    let shares = pick_shares(&table, 20_000, |key| key != "codex");
    close(&shares, "codex", 0.0);
    close(&shares, "claude/opus", 0.75);
    close(&shares, "claude/fable", 0.25);
}

#[test]
fn exclude_providers_removes_a_runtime_and_renormalises() {
    let config = layers(&[
        (
            Layer::MachineDefaults,
            json!({"runtimes": {"roleDraw": {"judge": [{"attempt": 1, "draw":
                {"claude/opus": 80, "claude/fable": 10, "codex": 10}}]}}}),
        ),
        (Layer::Local, json!({"runtimes": {"excludeProviders": ["codex"]}})),
    ]);
    assert_eq!(config.exclude_layer, Some(Layer::Local));
    let table = config.effective(config.table_for("judge", 1).unwrap());
    assert_eq!(table.total(), 90);
    assert_eq!(table.off.len(), 1);
    assert!(table.off[0].1.contains("excludeProviders (local)"), "{:?}", table.off);
    let shares = pick_shares(&table, 20_000, |_| true);
    close(&shares, "codex", 0.0);
    close(&shares, "claude/opus", 80.0 / 90.0);
}

#[test]
fn weight_zero_is_off() {
    let config = judge_config(&json!([{"attempt": 1, "draw": {"claude": 1, "codex": 0}}]));
    let table = config.effective(config.table_for("judge", 1).unwrap());
    assert_eq!(table.active.len(), 1);
    assert_eq!(table.off, vec![("codex".to_string(), "weight 0 (off)".to_string())]);
    let shares = pick_shares(&table, 500, |_| true);
    close(&shares, "claude", 1.0);
}

#[test]
fn a_table_empty_after_exclusions_draws_nothing_and_says_so() {
    let config = layers(&[(
        Layer::RepoConfig,
        json!({"runtimes": {"excludeProviders": ["codex"],
            "roleDraw": {"judge": [{"attempt": 1, "draw": {"codex": 100}}]}}}),
    )]);
    let table = config.effective(config.table_for("judge", 1).unwrap());
    assert!(table.active.is_empty());
    let plan = plan("judge", 1, &table, 1, None);
    assert!(plan.order.is_empty());
    let record = plan.finish(&resolve::<(), _, _>(&[], |_| Ok(()), |_, _, ()| Ok(())));
    assert_eq!(record.pick, None);
    assert!(record.diagnostic_suffix().contains("EMPTY"), "{}", record.diagnostic_suffix());
    assert!(record.marker_fragment().ends_with("pick:none"));
}

#[test]
#[serial_test::serial]
fn the_explain_view_flags_an_empty_table_without_failing() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join(".loom")).unwrap();
    std::fs::write(
        dir.path().join(".loom/config.json"),
        json!({"runtimes": {"excludeProviders": ["codex"], "rolePreference": {"curator": ["claude"]},
            "roleDraw": {"judge": [{"attempt": 1, "draw": {"codex": 100}},
                                   {"attempt": 2, "draw": {"claude/fable": 100}}]}}})
        .to_string(),
    )
    .unwrap();
    let doc = explain::explain_json(dir.path());
    let judge = doc["stages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["stage"] == "judge")
        .unwrap();
    assert_eq!(judge["roleDraw"][0]["empty"], true);
    assert_eq!(judge["roleDraw"][1]["candidates"][0]["p"], 1.0);
    assert_eq!(judge["roleDraw"][1]["layer"], "repo-config");
    let curator = doc["stages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["stage"] == "curator")
        .unwrap();
    assert_eq!(curator["fallback"]["source"], "role-preference");
    let text = explain::explain_text(dir.path());
    assert!(text.contains("WARNING: empty after exclusions"), "{text}");
    assert!(text.contains("excludeProviders: codex (set by repo-config)"), "{text}");
}

#[test]
#[serial_test::serial]
fn attempt_two_draws_from_the_attempt_two_table_after_a_blocked_attempt_one() {
    attempts::reset_for_tests();
    let root = std::path::Path::new("/tmp/role-draw-attempt-test");
    let config = judge_config(&json!([
        {"attempt": 1, "draw": {"codex": 100}},
        {"attempt": 2, "draw": {"claude/fable": 100}},
    ]));
    assert_eq!(attempts::current(root, "judge"), 1);
    assert_eq!(config.table_for("judge", 1).unwrap().entries[0].0, "codex");
    // A blocked (hook-denied) attempt 1 counts as a failed attempt.
    assert_eq!(attempts::record_failure(root, "judge"), 2);
    let attempt = attempts::current(root, "judge");
    assert_eq!(config.table_for("judge", attempt).unwrap().attempt, 2);
    // Attempt N past the last table keeps using the highest one <= N.
    attempts::record_failure(root, "judge");
    assert_eq!(
        config
            .table_for("judge", attempts::current(root, "judge"))
            .unwrap()
            .attempt,
        2
    );
    // Aliases share the counter; another role and another root do not.
    assert_eq!(attempts::current(root, "code-review-specialist"), 3);
    assert_eq!(attempts::current(root, "curator"), 1);
    assert_eq!(attempts::current(std::path::Path::new("/tmp/other"), "judge"), 1);
    attempts::record_success(root, "judge");
    assert_eq!(attempts::current(root, "judge"), 1);
    // A role whose first table starts at attempt 2 has no draw on attempt 1.
    let late = judge_config(&json!({"2": {"claude": 1}}));
    assert!(late.table_for("judge", 1).is_none());
    attempts::reset_for_tests();
}

#[test]
fn a_host_overrides_one_stage_attempt_without_restating_the_others() {
    let fleet = json!({"runtimes": {"roleDraw": {
        "judge": [{"attempt": 1, "draw": {"claude/opus": 80, "codex": 20}},
                  {"attempt": 2, "draw": {"claude/fable": 100}}],
        "builder": [{"attempt": 1, "draw": {"claude": 100}}]}}});
    let host = json!({"runtimes": {"roleDraw": {"judge": {"1": {"claude/sonnet": 100}}}}});
    let config = layers(&[(Layer::MachineDefaults, fleet), (Layer::Local, host)]);
    let a1 = config.table_for("judge", 1).unwrap();
    assert_eq!(
        (a1.layer, a1.entries.clone()),
        (Layer::Local, vec![("claude/sonnet".into(), 100)])
    );
    let a2 = config.table_for("judge", 2).unwrap();
    assert_eq!(a2.layer, Layer::MachineDefaults);
    assert_eq!(a2.entries, vec![("claude/fable".to_string(), 100)]);
    assert_eq!(config.table_for("builder", 1).unwrap().layer, Layer::MachineDefaults);
    // `null` at a higher tier clears the role; absent leaves it.
    let cleared = layers(&[
        (
            Layer::MachineDefaults,
            json!({"runtimes": {"roleDraw": {"judge": {"1": {"a": 1}}}}}),
        ),
        (Layer::RepoConfig, json!({"runtimes": {"roleDraw": {"judge": null}}})),
    ]);
    assert!(cleared.table_for("judge", 1).is_none());
}

#[test]
fn malformed_entries_are_warnings_never_failures() {
    let config = layers(&[(
        Layer::RepoConfig,
        json!({"runtimes": {"_note": "x", "excludeProviders": "codex", "roleDraw": {
            "_note": "comment",
            "buidler": [{"attempt": 1, "draw": {"claude": 1}}],
            "judge": [{"draw": {"claude": 1}},
                      {"attempt": 1, "draw": {"claude": 2, "codex": -1, "x": "9", "_c": 1}}],
            "curator": "claude"}}}),
    )]);
    assert_eq!(config.table_for("judge", 1).unwrap().entries, vec![("claude".to_string(), 2)]);
    assert!(config.table_for("curator", 1).is_none());
    assert!(config.exclude.is_empty());
    let joined = config.warnings.join("\n");
    for needle in [
        "buidler: unknown role",
        "no attempt",
        "\"codex\"",
        "\"x\"",
        "curator must",
        "excludeProviders must",
    ] {
        assert!(joined.contains(needle), "{needle} not in {joined}");
    }
    // An invalid key is off, not fatal.
    let bad = judge_config(&json!({"1": {"cla ude": 5, "claude": 5}}));
    let table = bad.effective(bad.table_for("judge", 1).unwrap());
    assert_eq!(table.active.len(), 1);
    assert!(table.off[0].1.starts_with("invalid"));
}

#[test]
fn a_draw_replays_from_its_record() {
    let config = judge_config(&json!({"1": {"claude/opus": 80, "claude/fable": 10, "codex": 10}}));
    let table = config.effective(config.table_for("judge", 1).unwrap());
    let s = seed(29_000_000, "host-a", "loom", "judge", 1);
    assert_eq!(s, seed(29_000_000, "host-a", "loom", "judge", 1));
    assert_ne!(s, seed(29_000_000, "host-a", "loom", "judge", 2));
    assert_ne!(s, seed(29_000_000, "host-a", "loom-ui", "judge", 1));
    let run = || {
        let plan = plan("judge", 1, &table, s, None);
        let taps = plan.taps();
        plan.finish(&resolve(&taps, |_| Ok::<(), SkipReason>(()), |_, _, ()| Ok(())))
    };
    let record = run();
    assert_eq!(record, run());
    assert_eq!(record.seed, format!("{s:016x}"));
    let step = record.winning_step().unwrap();
    assert_eq!(step.total, 100);
    // The pick is the cumulative-weight bucket the roll lands in.
    let mut acc = 0;
    let bucket = record
        .table
        .iter()
        .find(|w| {
            acc += w.weight;
            step.roll < acc
        })
        .unwrap();
    assert_eq!(Some(&bucket.candidate), record.pick.as_ref());
    let json = serde_json::to_value(&record).unwrap();
    assert_eq!(json["layer"], "repo-config");
    assert!(json.get("passed_over").is_none());
}
