//! Pure tests for the weighted role draw (#11726).
use super::*;
use crate::runtime_admission::{AdmissionContext, ContainmentProof, RuntimeRejection};
use crate::runtime_preference::resolve::{resolve, SkipReason};
use crate::runtime_preference::{ContainmentPreparer, DispatchContext};
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
    // The tap carries the pick's model, so containment selects an account
    // for the model the launch will use (#12748).
    assert_eq!(
        c.tap(),
        Tap {
            model: Some("opus".into()),
            ..Tap::runtime("claude")
        }
    );
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

/// #11738: the per-repo fleet tier sits between `.loom-project` and
/// `.loom-local/local.json` in the draw's own tier list, exactly as in the
/// config resolver, so a fleet-set `roleDraw` / `excludeProviders` beats the
/// repo's committed value and a host-local override still beats it.
#[test]
fn the_per_repo_fleet_tier_beats_the_committed_config_and_loses_to_local() {
    let dir = tempfile::tempdir().unwrap();
    let write = |rel: &str, v: &Value| {
        let p = dir.path().join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, v.to_string()).unwrap();
    };
    write(
        LEGACY_CONFIG_REL,
        &json!({"runtimes": {"excludeProviders": ["opencode"],
            "roleDraw": {"judge": [{"attempt": 1, "draw": {"claude": 1}}]}}}),
    );
    write(
        FLEET_REPO_CONFIG_REL,
        &json!({"runtimes": {"excludeProviders": ["codex"],
            "roleDraw": {"judge": [{"attempt": 1, "draw": {"codex": 1}}]}}}),
    );
    let cfg = DrawConfig::load(dir.path());
    assert_eq!(cfg.roles["judge"][&1].layer, Layer::FleetRepo);
    assert_eq!(cfg.exclude, vec!["codex".to_string()]);
    assert_eq!(cfg.exclude_layer, Some(Layer::FleetRepo));

    write(LOCAL_CONFIG_REL, &json!({"runtimes": {"excludeProviders": []}}));
    let cfg = DrawConfig::load(dir.path());
    assert_eq!(cfg.exclude_layer, Some(Layer::Local));
    assert_eq!(cfg.roles["judge"][&1].layer, Layer::FleetRepo);
    assert_eq!(Layer::FleetRepo.as_str(), "fleet-repo");
}

#[test]
fn an_empty_attempt_one_table_falls_through_to_attempt_two() {
    // #11782: `excludeProviders: ["codex"]` empties the attempt-1 table; a skip
    // before spawn never advances the counter, so the draw must move on itself.
    let config = layers(&[(
        Layer::RepoConfig,
        json!({"runtimes": {"excludeProviders": ["codex"], "roleDraw": {"judge": [
            {"attempt": 1, "draw": {"codex": 100}},
            {"attempt": 2, "draw": {"claude": 100}},
        ]}}}),
    )]);
    let table = config.effective_for("judge", 1).unwrap();
    assert_eq!(table.attempt, 2);
    assert_eq!(table.active[0].0.key, "claude");
    let drawn = plan("judge", 1, &table, 1, None);
    assert_eq!(drawn.order.len(), 1);
    assert_eq!(drawn.record.attempt, 1);
    assert_eq!(drawn.record.table_attempt, 2);

    // Every table empty: the named table comes back empty, so the skip still
    // reports EMPTY.
    let config = layers(&[(
        Layer::RepoConfig,
        json!({"runtimes": {"excludeProviders": ["codex"], "roleDraw": {"judge": [
            {"attempt": 1, "draw": {"codex": 100}},
            {"attempt": 2, "draw": {"codex": 5, "claude": 0}},
        ]}}}),
    )]);
    let table = config.effective_for("judge", 1).unwrap();
    assert_eq!(table.attempt, 1);
    assert!(table.active.is_empty());
}

#[test]
fn overflowing_weights_saturate_and_are_not_reported_empty() {
    let config = judge_config(&json!([{"attempt": 1, "draw":
        {"claude": u64::MAX, "codex": 1}}]));
    let table = config.effective_for("judge", 1).unwrap();
    assert_eq!(table.total(), u64::MAX);
    let drawn = plan("judge", 1, &table, 9, None);
    assert!(!drawn.record.diagnostic_suffix().contains("EMPTY"));
    assert_eq!(drawn.order.len(), 2);
}

#[test]
fn an_empty_table_is_reported_once_per_root_and_again_when_it_changes() {
    let empty = |exclude: &[&str]| {
        let config = layers(&[(
            Layer::RepoConfig,
            json!({"runtimes": {"excludeProviders": exclude,
                "roleDraw": {"judge": [{"attempt": 1, "draw": {"codex": 100, "claude": 0}}]}}}),
        )]);
        config.effective(config.table_for("judge", 1).unwrap())
    };
    let root_a = Path::new("/tmp/role-draw-dedup-a-13463");
    let root_b = Path::new("/tmp/role-draw-dedup-b-13463");
    let table = empty(&["codex"]);
    assert!(table.active.is_empty());
    assert!(empty_table_first_sighting(root_a, "judge", 1, &table));
    assert!(!empty_table_first_sighting(root_a, "judge", 1, &table));
    // A different root with the identical table logs separately.
    assert!(empty_table_first_sighting(root_b, "judge", 1, &table));
    // A changed exclusion set changes the table hash and re-logs once.
    let changed = empty(&["codex", "claude"]);
    assert!(changed.active.is_empty());
    assert!(empty_table_first_sighting(root_a, "judge", 1, &changed));
    assert!(!empty_table_first_sighting(root_a, "judge", 1, &changed));
}

// ---------------------------------------------------------------------------
// The explain view's per-candidate admission verdict (#14027). The
// production probe (`explain_json` -> `probe_on_this_host`) runs the real
// spawn path's `probe_candidate` with the real containment preparer; these
// tests inject the probe exactly as production wires it, with a recording
// fake preparer standing in for Docker-backed private-clone containment.
// ---------------------------------------------------------------------------

/// Pins the env the admission probe reads, so the test host's real pools and
/// pins cannot leak into a verdict (the containment_tests::Env pattern).
struct ProbeEnv(
    Vec<(&'static str, Option<String>)>,
    tempfile::TempDir,
    crate::tokens_pool::profile_root_env::ProfileRootLock,
);
impl ProbeEnv {
    fn new() -> Self {
        let lock = crate::tokens_pool::profile_root_env::lock();
        let keys = [
            "LOOM_RUNTIME",
            "LOOM_RUNTIME_BUILDER",
            "LOOM_RUNTIME_JUDGE",
            "LOOM_CODEX_PROFILE_ROOT",
            "LOOM_CODEX_PROFILE",
            "LOOM_CODEX_HOME",
            "CODEX_HOME",
            "LOOM_SPAWN_NO_EXPORT",
            "LOOM_CODEX_NO_EXEC",
            crate::runtime_preference::ceiling::MAX_CONCURRENT_ENV,
            "ZAI_API_KEY",
            "LOOM_NATIVE_AUTH_FILE",
            crate::api_keys_pool::paths::SHARED_API_KEYS_DIR_ENV,
        ];
        let prior = keys.iter().map(|k| (*k, std::env::var(k).ok())).collect();
        for key in keys {
            std::env::remove_var(key);
        }
        let profiles = tempfile::tempdir().unwrap();
        std::env::set_var("LOOM_CODEX_PROFILE_ROOT", profiles.path());
        Self(prior, profiles, lock)
    }

    fn provision_codex_seat(&self) {
        std::fs::create_dir_all(self.1.path().join("seat")).unwrap();
    }
}
impl Drop for ProbeEnv {
    fn drop(&mut self) {
        for (key, value) in self.0.drain(..) {
            match value {
                Some(value) => std::env::set_var(key, value),
                None => std::env::remove_var(key),
            }
        }
    }
}

/// A workspace admission can resolve against: the shipped role/runtime
/// bindings, spawn adapters, an enabled zai seat, and the roleDraw config.
fn explain_workspace(config: &Value) -> tempfile::TempDir {
    let repo = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let dir = tempfile::tempdir().unwrap();
    for (from, to) in [
        ("defaults/roles", ".loom/roles"),
        ("defaults/runtimes", ".loom/runtimes"),
    ] {
        std::fs::create_dir_all(dir.path().join(to)).unwrap();
        for entry in std::fs::read_dir(repo.join(from)).unwrap().flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) == Some("json") {
                std::fs::copy(&path, dir.path().join(to).join(path.file_name().unwrap())).unwrap();
            }
        }
    }
    std::fs::create_dir_all(dir.path().join(".loom/scripts")).unwrap();
    for runtime in ["claude", "codex"] {
        let adapter = dir.path().join(format!(".loom/scripts/spawn-{runtime}.sh"));
        std::fs::write(&adapter, "#!/bin/sh\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(adapter, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    std::fs::write(dir.path().join(".loom/config.json"), config.to_string()).unwrap();
    crate::runtime_preference::unprovisioned_tests::provision_zai_pool(dir.path(), true);
    dir
}

/// Admits against a synthetic proof when `verified`; records every call
/// (the containment_tests::FakePreparer contract).
struct FakePreparer {
    root: std::path::PathBuf,
    verified: bool,
    proof: ContainmentProof,
    contained: Vec<String>,
    drawn_models: Vec<Option<String>>,
    held: bool,
    releases: usize,
}
impl FakePreparer {
    fn new(root: &Path, verified: bool) -> Self {
        Self {
            root: root.to_owned(),
            verified,
            proof: ContainmentProof::fixture("seat"),
            contained: Vec::new(),
            drawn_models: Vec::new(),
            held: false,
            releases: 0,
        }
    }
}
impl ContainmentPreparer for FakePreparer {
    fn contain(
        &mut self,
        role: &str,
        explicit: Option<&str>,
        drawn_model: Option<&str>,
        rejection: RuntimeRejection,
    ) -> Result<crate::runtime_admission::ResolvedRuntime, RuntimeRejection> {
        self.drawn_models.push(drawn_model.map(str::to_owned));
        self.contained
            .push(format!("{role}:{}", explicit.unwrap_or("-")));
        if !self.verified {
            return Err(rejection.with_containment_failure("fixture: hook trust unproven"));
        }
        let admitted = crate::runtime_admission::resolve_and_admit_in(
            &self.root,
            role,
            explicit,
            AdmissionContext::PrivateClone(&self.proof),
        )?;
        self.held = true;
        Ok(admitted)
    }
    fn release(&mut self) {
        self.held = false;
        self.releases += 1;
    }
}

/// The production probe shape over a shared fake: `probe_candidate` with the
/// injected preparer — the same admission path a spawn walk asks each tap.
fn fake_probe<'a>(
    root: &'a Path,
    fake: &'a std::cell::RefCell<FakePreparer>,
) -> impl FnMut(&str, &Tap, usize) -> Result<(), SkipReason> + 'a {
    move |role: &str, tap: &Tap, tier: usize| {
        let mut fake = fake.borrow_mut();
        crate::runtime_preference::probe_candidate(
            root,
            role,
            tap,
            tier,
            0,
            DispatchContext::default(),
            &mut *fake,
        )
        .map(|_| ())
    }
}

/// The explain view (JSON and text), probed through the production shape
/// with a fresh fake per pass. Returns the doc-pass fake's record — the
/// always-release assertion reads that one.
fn explain_via_fake(root: &Path, verified: bool) -> (Value, String, FakePreparer) {
    let doc_fake = std::cell::RefCell::new(FakePreparer::new(root, verified));
    let doc = explain::explain_json_with(root, &mut fake_probe(root, &doc_fake));
    let text_fake = std::cell::RefCell::new(FakePreparer::new(root, verified));
    let text = explain::explain_text_with(root, &mut fake_probe(root, &text_fake));
    (doc, text, doc_fake.into_inner())
}

fn stage_of<'a>(doc: &'a Value, name: &str) -> &'a Value {
    doc["stages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["stage"] == name)
        .unwrap()
}

#[test]
#[serial_test::serial]
fn the_explain_view_marks_each_candidate_admitted_or_not_and_renormalises() {
    let _env = ProbeEnv::new(); // no codex seat: the codex pool is empty
                                // Builder, not judge: the shipped zai-flash profile excludes judge
                                // (#11400), and this test is about availability, not excludedRole.
    let dir = explain_workspace(&json!({
        "runtimes": {"roleDraw": {"builder": [{"attempt": 1, "draw": {"codex": 60, "opencode": 40}}]}}
    }));
    let (doc, text, fake) = explain_via_fake(dir.path(), true);
    let builder = stage_of(&doc, "builder");
    let candidates = &builder["roleDraw"][0]["candidates"];
    assert_eq!(candidates.as_array().unwrap().len(), 2);
    // Codex: admitted (containment verified by the fake), but its pool is
    // empty — not served, with the walk's own reason, and the passed-over
    // selection released.
    assert_eq!(candidates[0]["candidate"], "codex");
    assert_eq!(candidates[0]["admitted"], false);
    assert!(
        candidates[0]["reason"]
            .as_str()
            .unwrap()
            .starts_with("unavailable("),
        "{}",
        candidates[0]["reason"]
    );
    // Opencode: its zai seat is provisioned and enabled — served.
    assert_eq!(candidates[1]["candidate"], "opencode");
    assert_eq!(candidates[1]["admitted"], true);
    // The effective table drops codex and renormalises.
    let effective = &builder["roleDraw"][0]["effective"];
    assert_eq!(effective["total"], 40);
    assert_eq!(effective["empty"], false);
    assert_eq!(effective["candidates"][0]["candidate"], "opencode");
    assert_eq!(effective["candidates"][0]["weight"], 40);
    assert_eq!(effective["candidates"][0]["p"], 1.0);
    // The configured table's p is unchanged (before availability).
    assert_eq!(builder["roleDraw"][0]["candidates"][0]["p"], 0.6);
    assert!(text.contains("not admitted: unavailable("), "{text}");
    assert!(text.contains("effective on this host: Σw=40 — opencode 100.0%"), "{text}");
    assert!(!text.contains("no runtime can serve this role here"), "{text}");
    // The passed-over contained selection was released: nothing stays held.
    assert_eq!(fake.contained, vec!["builder:codex".to_string()]);
    assert!(!fake.held);
}

#[test]
#[serial_test::serial]
fn a_contained_candidate_that_cannot_be_admitted_renders_not_admitted_and_empty() {
    let env = ProbeEnv::new();
    env.provision_codex_seat(); // availability is NOT the refusal: containment is
    let dir = explain_workspace(&json!({
        "runtimes": {"roleDraw": {"builder": [{"attempt": 1, "draw": {"codex/gpt-5": 100}}]}}
    }));
    let (doc, text, fake) = explain_via_fake(dir.path(), false);
    let builder = stage_of(&doc, "builder");
    let candidate = &builder["roleDraw"][0]["candidates"][0];
    assert_eq!(candidate["candidate"], "codex/gpt-5");
    assert_eq!(candidate["admitted"], false);
    // The containment refusal is retried through the preparer (#8787) and the
    // reason names the unmet capability and the failed obligation — the
    // worktreeIsolation case the plain table cannot show (#13983).
    let reason = candidate["reason"].as_str().unwrap();
    assert!(reason.starts_with("not-admitted(worktreeIsolation"), "{reason}");
    assert!(reason.contains("could not satisfy it"), "{reason}");
    // The effective table is empty, plainly.
    let effective = &builder["roleDraw"][0]["effective"];
    assert_eq!(effective["empty"], true);
    assert_eq!(effective["total"], 0);
    assert!(text.contains("no runtime can serve this role here"), "{text}");
    assert!(text.contains("not admitted: not-admitted(worktreeIsolation"), "{text}");
    // The probe asked the real containment path, for the drawn model.
    assert_eq!(fake.contained, vec!["builder:codex".to_string()]);
    assert_eq!(fake.drawn_models, vec![Some("gpt-5".to_string())]);
    // Nothing may stay held: the view releases before it returns.
    assert!(!fake.held);
    assert!(fake.releases >= 1);
}

#[test]
#[serial_test::serial]
fn with_containment_satisfiable_the_candidate_renders_admitted_and_no_lease_is_kept() {
    let env = ProbeEnv::new();
    env.provision_codex_seat();
    let dir = explain_workspace(&json!({
        "runtimes": {"roleDraw": {"builder": [{"attempt": 1, "draw": {"codex": 100}}]}}
    }));
    let (doc, text, fake) = explain_via_fake(dir.path(), true);
    let builder = stage_of(&doc, "builder");
    let candidate = &builder["roleDraw"][0]["candidates"][0];
    assert_eq!(candidate["admitted"], true);
    let effective = &builder["roleDraw"][0]["effective"];
    assert_eq!(effective["total"], 100);
    assert_eq!(effective["candidates"][0]["candidate"], "codex");
    assert_eq!(effective["candidates"][0]["p"], 1.0);
    assert!(text.contains("effective on this host: Σw=100 — codex 100.0%"), "{text}");
    assert!(!text.contains("no runtime can serve this role here"), "{text}");
    assert_eq!(fake.contained, vec!["builder:codex".to_string()]);
    // The admitted contained selection is released too: the view never
    // launches, so it never keeps the account lease a launch would take.
    assert!(!fake.held);
    assert_eq!(fake.releases, 1);
}
