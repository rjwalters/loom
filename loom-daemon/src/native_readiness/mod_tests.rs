//! Boundary-model tests: the report must never turn an unobserved provider
//! boundary into a number, and never carry child output.
use super::*;

fn attempt(index: usize, mode: Mode, cache: CacheOutcome, millis: &[u64]) -> AttemptReport {
    let stages = Stage::ALL
        .iter()
        .enumerate()
        .map(|(i, &stage)| StageObservation {
            stage,
            observation: match (stage.unknown_reason(), millis.get(i)) {
                (Some(reason), _) => Observation::Unknown { reason },
                (None, Some(&m)) => Observation::Measured { millis: m },
                (None, None) => Observation::NotReached,
            },
        })
        .collect();
    AttemptReport {
        index,
        mode,
        cache,
        total_millis: millis.iter().sum(),
        plugin_load_observed: None,
        stages,
    }
}

fn host(before: Option<f64>, after: Option<f64>) -> HostConditions {
    HostConditions {
        os: "linux",
        arch: "x86_64",
        logical_cpus: 8,
        loadavg_1m_before: before,
        loadavg_1m_after: after,
    }
}

#[test]
fn provider_boundaries_are_always_unknown_with_a_reason() {
    for stage in Stage::ALL {
        match stage {
            Stage::FirstProviderEvent | Stage::FirstTool | Stage::Completion => {
                assert!(!stage.provider_free_observable(), "{}", stage.as_str());
                let reason = stage
                    .unknown_reason()
                    .unwrap_or_else(|| panic!("{}", stage.as_str()));
                assert!(!reason.is_empty());
            }
            _ => {
                assert!(stage.provider_free_observable(), "{}", stage.as_str());
                assert_eq!(stage.unknown_reason(), None, "{}", stage.as_str());
            }
        }
    }
}

#[test]
fn report_ledger_lists_every_unobservable_boundary_and_claims_nothing() {
    let attempts = vec![attempt(
        0,
        Mode::Cold,
        CacheOutcome::Bypassed,
        &[10, 20, 30, 40],
    )];
    let phase = PhaseReport::new(Mode::Cold, host(Some(1.0), Some(1.0)), attempts);
    let report = ReadinessReport::new(
        NetworkMode::Allowed,
        vec!["--version".into()],
        Some("1.18.31".into()),
        None,
        vec![phase],
    );
    let unknown: Vec<&str> = report
        .unknown_boundaries
        .iter()
        .map(|u| u.stage.as_str())
        .collect();
    assert_eq!(
        unknown,
        ["first_provider_event", "first_tool", "completion"],
        "every provider boundary must appear in the unknown ledger"
    );
    assert_eq!(report.model_calls, 0);
    assert!(!report.paid_retry);
    assert!(!report.forge_contact);
    assert!(!report.speedup_claimed);
    assert_eq!(
        report.plugin_load_proven, None,
        "plugin load must not be asserted without a live receipt"
    );
    // A single phase cannot be compared with anything.
    assert_eq!(report.host_conditions_comparable, None);
}

#[test]
fn serialized_report_has_no_duration_for_an_unknown_boundary() {
    let attempts = vec![attempt(0, Mode::Warm, CacheOutcome::Hit, &[1, 2, 3, 4])];
    let phase = PhaseReport::new(Mode::Warm, host(Some(0.5), Some(0.5)), attempts);
    let report = ReadinessReport::new(NetworkMode::DeniedByEnv, vec![], None, None, vec![phase]);
    let value = serde_json::to_value(&report).unwrap();
    let stages = value["phases"][0]["attempts"][0]["stages"]
        .as_array()
        .unwrap();
    for stage in stages {
        let name = stage["stage"].as_str().unwrap();
        let unknown = ["first_provider_event", "first_tool", "completion"].contains(&name);
        assert_eq!(stage["state"].as_str().unwrap() == "unknown", unknown, "{name} state");
        if unknown {
            assert!(stage.get("millis").is_none(), "{name} must carry no duration");
            assert!(stage["reason"].as_str().is_some_and(|r| !r.is_empty()));
        }
    }
    // No boundary is aggregated unless it was measured.
    let aggregated: Vec<&str> = value["phases"][0]["per_stage"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["stage"].as_str().unwrap())
        .collect();
    assert_eq!(
        aggregated,
        [
            "binary_probe",
            "binding_provision",
            "package_resolution",
            "server_session_ready"
        ]
    );
}

#[test]
fn failed_observations_carry_counts_and_a_closed_classification_only() {
    let observation = Observation::Failed {
        classification: Classification::Timeout,
        elapsed_millis: 60_000,
        stdout_bytes: 4096,
        stderr_bytes: 12,
    };
    let value = serde_json::to_value(&observation).unwrap();
    assert_eq!(value["state"], "failed");
    assert_eq!(value["classification"], "timeout");
    assert_eq!(value["stdout_bytes"], 4096);
    // The only string in the payload is the closed-set classification.
    let strings: Vec<&str> = value
        .as_object()
        .unwrap()
        .values()
        .filter_map(serde_json::Value::as_str)
        .collect();
    assert_eq!(strings, ["failed", "timeout"]);
}

#[test]
fn aggregate_reports_min_median_max_without_a_mean_or_ratio() {
    let attempts = vec![
        attempt(0, Mode::Cold, CacheOutcome::Bypassed, &[10, 0, 0, 0]),
        attempt(1, Mode::Cold, CacheOutcome::Bypassed, &[30, 0, 0, 0]),
        attempt(2, Mode::Cold, CacheOutcome::Bypassed, &[20, 0, 0, 0]),
    ];
    let probed = aggregate(&attempts, Stage::BinaryProbe).unwrap();
    assert_eq!(
        probed,
        Aggregate {
            measured: 3,
            min_millis: 10,
            median_millis: 20,
            max_millis: 30
        }
    );
    let value = serde_json::to_value(probed).unwrap();
    for forbidden in ["mean", "average", "speedup", "ratio", "improvement"] {
        assert!(value.get(forbidden).is_none(), "{forbidden}");
    }
    // An unobserved boundary aggregates to nothing rather than to zero.
    assert_eq!(aggregate(&attempts, Stage::FirstProviderEvent), None);
    assert_eq!(aggregate(&[], Stage::BinaryProbe), None);
}

#[test]
fn median_takes_a_real_sample_rather_than_averaging() {
    assert_eq!(median(&mut []), None);
    assert_eq!(median(&mut [7]), Some(7));
    // Lower central sample: 10 was actually observed, 15 never was.
    assert_eq!(median(&mut [20, 10]), Some(10));
    assert_eq!(median(&mut [5, 1, 9]), Some(5));
}

#[test]
fn comparability_is_measured_not_assumed() {
    let phase = |before, after| {
        PhaseReport::new(
            Mode::Cold,
            host(before, after),
            vec![attempt(
                0,
                Mode::Cold,
                CacheOutcome::Bypassed,
                &[1, 1, 1, 1],
            )],
        )
    };
    // Peaks 1.0 vs 1.1 -> within the drift band.
    assert_eq!(
        comparable_load(&[phase(Some(1.0), Some(0.9)), phase(Some(1.1), Some(1.0))]),
        Some(true)
    );
    // Peaks 1.0 vs 8.0 -> the phases are not comparable.
    assert_eq!(
        comparable_load(&[phase(Some(1.0), Some(0.9)), phase(Some(8.0), Some(7.0))]),
        Some(false)
    );
    // Unreadable load average -> unknown, never an optimistic default.
    assert_eq!(comparable_load(&[phase(None, None), phase(None, None)]), None);
    assert_eq!(comparable_load(&[phase(Some(1.0), None)]), None);
}

#[test]
fn plugin_load_verdict_is_none_when_no_attempt_observed_it() {
    let attempts = vec![attempt(0, Mode::Cold, CacheOutcome::Bypassed, &[10])];
    let phase = PhaseReport::new(Mode::Cold, host(Some(1.0), Some(1.0)), attempts);
    assert_eq!(
        plugin_load_verdict(&[phase]),
        None,
        "a run whose readiness boundary never completed has no opinion on plugin load"
    );
}

#[test]
fn plugin_load_verdict_is_true_when_every_observed_attempt_loaded() {
    let mut a0 = attempt(0, Mode::Cold, CacheOutcome::Bypassed, &[10]);
    a0.plugin_load_observed = Some(true);
    let mut a1 = attempt(1, Mode::Cold, CacheOutcome::Bypassed, &[10]);
    a1.plugin_load_observed = Some(true);
    let phase = PhaseReport::new(Mode::Cold, host(Some(1.0), Some(1.0)), vec![a0, a1]);
    assert_eq!(plugin_load_verdict(&[phase]), Some(true));
}

#[test]
fn plugin_load_verdict_is_false_on_one_counterexample() {
    let mut a0 = attempt(0, Mode::Cold, CacheOutcome::Bypassed, &[10]);
    a0.plugin_load_observed = Some(true);
    let mut a1 = attempt(1, Mode::Cold, CacheOutcome::Bypassed, &[10]);
    a1.plugin_load_observed = Some(false);
    let phase = PhaseReport::new(Mode::Cold, host(Some(1.0), Some(1.0)), vec![a0, a1]);
    assert_eq!(
        plugin_load_verdict(&[phase]),
        Some(false),
        "this probe shape loads the guarded plugin is a claim about every invocation \
         of it, and one counterexample refutes it"
    );
}

#[test]
fn plugin_load_verdict_ignores_attempts_that_never_reached_the_boundary() {
    let mut a0 = attempt(0, Mode::Cold, CacheOutcome::Bypassed, &[10]);
    a0.plugin_load_observed = Some(true);
    // Default plugin_load_observed is None: this attempt never got far enough
    // to have an opinion, and must not be treated as a counterexample.
    let a1 = attempt(1, Mode::Cold, CacheOutcome::Bypassed, &[]);
    let phase = PhaseReport::new(Mode::Cold, host(Some(1.0), Some(1.0)), vec![a0, a1]);
    assert_eq!(plugin_load_verdict(&[phase]), Some(true));
}

#[test]
fn readiness_report_propagates_the_plugin_load_verdict() {
    let mut a0 = attempt(0, Mode::Cold, CacheOutcome::Bypassed, &[10]);
    a0.plugin_load_observed = Some(true);
    let phase = PhaseReport::new(Mode::Cold, host(Some(1.0), Some(1.0)), vec![a0]);
    let report = ReadinessReport::new(
        NetworkMode::Allowed,
        vec!["debug".into(), "config".into()],
        Some("1.18.31".into()),
        None,
        vec![phase],
    );
    assert_eq!(
        report.plugin_load_proven,
        Some(true),
        "must be derived from the attempts' own receipts, never asserted by construction"
    );
}

#[test]
fn version_line_is_bounded_printable_and_first_line_only() {
    assert_eq!(sanitize_version_line("1.18.31\n"), Some("1.18.31".into()));
    assert_eq!(
        sanitize_version_line("\n  opencode v1.18.31 \nsecond line\n"),
        Some("opencode v1.18.31".into())
    );
    let noisy = format!("1.0.0\u{1b}[31m{}", "x".repeat(200));
    let line = sanitize_version_line(&noisy).unwrap();
    assert!(!line.contains('\u{1b}'));
    assert_eq!(line.chars().count(), 40);
    assert_eq!(sanitize_version_line(""), None);
    assert_eq!(sanitize_version_line("\u{1b}\u{7f}\n"), None);
}
