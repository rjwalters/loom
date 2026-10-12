//! Tests for the curator/auditor/guide event triggers (#10816).
//!
//! The decision ([`should_launch`]) is pure, so most cases drive it directly.
//! The gate ([`run_with_trigger_gate`]) and the dispatcher wiring are driven
//! with a stub probe and a leaked ledger of their own, so nothing here reads
//! the forge or shares state with another test.

use super::*;
use crate::role_runner::concurrent_dispatch::{
    admit_root_tick, DecideFn, QueueProbe, RoleDispatcher, RunnerFactory,
};
use crate::role_runner::triggers::*;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

// -- fixtures ---------------------------------------------------------------

const SHA_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const SHA_B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

fn inputs(observation: Observation, last: Option<&str>) -> TriggerInputs {
    TriggerInputs {
        observation,
        last_fingerprint: last.map(str::to_string),
        quiet_for: Duration::from_secs(60),
        max_quiet: Duration::from_secs(DEFAULT_MAX_QUIET_SECS),
    }
}

fn fingerprint(s: &str) -> Observation {
    Observation::Fingerprint(s.to_string())
}

fn unobserved() -> Observation {
    Observation::Unobserved("listing failed".to_string())
}

fn trigger_of(d: &TriggerDecision) -> Trigger {
    match d {
        TriggerDecision::Launch { trigger, .. } | TriggerDecision::Skip { trigger, .. } => *trigger,
    }
}

fn enabled() -> EventTriggerConfig {
    EventTriggerConfig {
        enabled: true,
        ..EventTriggerConfig::default()
    }
}

fn ledger() -> &'static TriggerLedger {
    Box::leak(Box::new(TriggerLedger::default()))
}

/// A probe returning `obs`, counting its calls.
fn probe(obs: Observation) -> (TriggerProbe, Arc<AtomicUsize>) {
    let calls = Arc::new(AtomicUsize::new(0));
    let c = Arc::clone(&calls);
    let p: TriggerProbe = Arc::new(move |_, _| {
        c.fetch_add(1, Ordering::SeqCst);
        obs.clone()
    });
    (p, calls)
}

/// Run the gate once with a `run` that returns `outcome`; returns the gate's
/// outcome and whether `run` was called.
fn gate(
    cfg: EventTriggerConfig,
    probe: &TriggerProbe,
    ledger: &TriggerLedger,
    role: &'static str,
    outcome: RoleTickOutcome,
) -> (RoleTickOutcome, bool) {
    let mut ran = false;
    let out = run_with_trigger_gate(cfg, probe, ledger, Path::new("/r/repo"), role, || {
        ran = true;
        outcome
    });
    (out, ran)
}

// -- auditor ----------------------------------------------------------------

#[test]
fn auditor_skips_when_origin_main_equals_the_last_audited_sha() {
    let d = should_launch("auditor", &inputs(fingerprint(SHA_A), Some(SHA_A)));
    assert!(!d.launches(), "{d:?}");
    assert_eq!(trigger_of(&d), Trigger::Event);
}

#[test]
fn auditor_launches_when_origin_main_moved() {
    let d = should_launch("auditor", &inputs(fingerprint(SHA_B), Some(SHA_A)));
    assert!(d.launches(), "{d:?}");
    assert_eq!(trigger_of(&d), Trigger::Event);
}

#[test]
fn auditor_launches_when_no_sha_was_ever_recorded() {
    let d = should_launch("auditor", &inputs(fingerprint(SHA_A), None));
    assert!(d.launches(), "{d:?}");
}

#[test]
fn auditor_unobserved_main_fails_open_with_floor() {
    let d = should_launch("auditor", &inputs(unobserved(), Some(SHA_A)));
    assert!(d.launches(), "{d:?}");
    assert_eq!(trigger_of(&d), Trigger::Floor);
}

#[test]
fn auditor_failed_run_does_not_advance_the_last_audited_sha() {
    let l = ledger();
    let (p, _) = probe(fingerprint(SHA_A));
    let failure = RoleTickOutcome::Failure("boom".to_string());
    let (out, ran) = gate(enabled(), &p, l, "auditor", failure.clone());
    assert!(ran, "first sighting launches");
    assert_eq!(out, failure);
    // Not recorded: the same SHA launches again next tick.
    let (_, ran) = gate(enabled(), &p, l, "auditor", RoleTickOutcome::Success);
    assert!(ran, "a failed run must not record the SHA");
    // Recorded after success: the same SHA now skips.
    let (out, ran) = gate(enabled(), &p, l, "auditor", RoleTickOutcome::Success);
    assert!(!ran, "unchanged main after a successful audit skips");
    assert_eq!(out, RoleTickOutcome::QueueEmpty);
    // A new SHA launches again.
    let (p2, _) = probe(fingerprint(SHA_B));
    let (_, ran) = gate(enabled(), &p2, l, "auditor", RoleTickOutcome::Success);
    assert!(ran, "a changed main launches");
}

// -- curator ----------------------------------------------------------------

#[test]
fn curator_skips_with_zero_untriaged_issues() {
    let d = should_launch("curator", &inputs(Observation::Count(0), None));
    assert!(!d.launches(), "{d:?}");
    assert_eq!(trigger_of(&d), Trigger::Debt(UNTRIAGED_AXIS));
}

#[test]
fn curator_work_observation_counts_pending_revision_requests() {
    // A revision request carries `loom:needs-revision`, not `loom:triage`; with
    // zero triage issues it must still be observed, or the gate skips Curator.
    assert!(CURATOR_WORK_LABELS.contains(&"loom:triage"));
    assert!(CURATOR_WORK_LABELS.contains(&"loom:needs-revision"));
}

#[test]
fn curator_work_observation_counts_the_blocked_unnamed_queue() {
    // A repository whose only pending Curator work is `loom:blocked-unnamed`
    // (#10558) must be observed, or event triggers wait out the quiet ceiling.
    assert!(CURATOR_WORK_LABELS.contains(&"loom:blocked-unnamed"));
    let d = should_launch("curator", &inputs(Observation::Count(1), None));
    assert!(d.launches(), "{d:?}");
}

#[test]
fn curator_launches_with_untriaged_issues() {
    let d = should_launch("curator", &inputs(Observation::Count(3), None));
    assert!(d.launches(), "{d:?}");
    assert_eq!(trigger_of(&d), Trigger::Debt(UNTRIAGED_AXIS));
}

#[test]
fn curator_unobserved_listing_fails_open_with_floor() {
    let d = should_launch("curator", &inputs(unobserved(), None));
    assert!(d.launches(), "{d:?}");
    assert_eq!(trigger_of(&d), Trigger::Floor);
}

#[test]
fn curator_gate_skip_spawns_nothing_and_is_not_a_failure() {
    let (p, calls) = probe(Observation::Count(0));
    let (out, ran) = gate(enabled(), &p, ledger(), "curator", RoleTickOutcome::Success);
    assert!(!ran);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(out, RoleTickOutcome::QueueEmpty);
    assert!(!crate::role_tick_telemetry::classify(&out).0.spawned());
}

// -- guide ------------------------------------------------------------------

#[test]
fn guide_skips_with_an_unchanged_ready_backlog_set() {
    let set = set_fingerprint(vec![3, 1, 2]);
    let d = should_launch("guide", &inputs(fingerprint(&set), Some(&set)));
    assert!(!d.launches(), "{d:?}");
    assert_eq!(trigger_of(&d), Trigger::Event);
}

#[test]
fn guide_launches_when_the_ready_backlog_set_changed() {
    let before = set_fingerprint(vec![1, 2, 3]);
    let after = set_fingerprint(vec![1, 2, 4]);
    let d = should_launch("guide", &inputs(fingerprint(&after), Some(&before)));
    assert!(d.launches(), "{d:?}");
    assert_eq!(trigger_of(&d), Trigger::Event);
}

#[test]
fn guide_unobserved_set_fails_open_with_floor() {
    let set = set_fingerprint(vec![1]);
    let d = should_launch("guide", &inputs(unobserved(), Some(&set)));
    assert!(d.launches(), "{d:?}");
    assert_eq!(trigger_of(&d), Trigger::Floor);
}

#[test]
fn guide_records_the_set_only_after_a_successful_run() {
    let l = ledger();
    let (p, _) = probe(fingerprint(&set_fingerprint(vec![7, 8])));
    let (_, ran) = gate(enabled(), &p, l, "guide", RoleTickOutcome::Success);
    assert!(ran);
    let (_, ran) = gate(enabled(), &p, l, "guide", RoleTickOutcome::Success);
    assert!(!ran, "an unchanged set after a successful run skips");
}

#[test]
fn set_fingerprint_ignores_order_and_duplicates() {
    assert_eq!(set_fingerprint(vec![3, 1, 2, 2]), set_fingerprint(vec![1, 2, 3]));
    assert_ne!(set_fingerprint(vec![1, 2]), set_fingerprint(vec![1, 2, 3]));
    assert!(set_fingerprint(vec![]).starts_with("0 issue(s)"));
}

// -- shared rules -------------------------------------------------------------

#[test]
fn quiet_ceiling_launches_with_floor_whatever_the_input() {
    for (role, obs, last) in [
        ("auditor", fingerprint(SHA_A), Some(SHA_A)),
        ("curator", Observation::Count(0), None),
        ("guide", fingerprint("s"), Some("s")),
    ] {
        let mut i = inputs(obs, last);
        i.quiet_for = i.max_quiet;
        let d = should_launch(role, &i);
        assert!(d.launches(), "{role}: {d:?}");
        assert_eq!(trigger_of(&d), Trigger::Floor, "{role}");
    }
}

#[test]
fn ledger_quiet_clock_starts_at_first_sighting_and_resets_on_success() {
    let l = TriggerLedger::default();
    let root = Path::new("/r/repo");
    let t0 = Instant::now();
    assert_eq!(l.read(root, "curator", t0), (None, Duration::ZERO));
    let t1 = t0 + Duration::from_secs(90);
    assert_eq!(l.read(root, "curator", t1).1, Duration::from_secs(90));
    l.record_success(root, "curator", None, t1);
    assert_eq!(l.read(root, "curator", t1 + Duration::from_secs(5)).1, Duration::from_secs(5));
    // A success without a fingerprint keeps the previous one.
    l.record_success(root, "auditor", Some(SHA_A.to_string()), t1);
    l.record_success(root, "auditor", None, t1);
    assert_eq!(l.read(root, "auditor", t1).0.as_deref(), Some(SHA_A));
}

#[test]
fn flag_off_runs_every_role_with_no_probe_read() {
    let (p, calls) = probe(Observation::Count(0));
    let off = EventTriggerConfig::default();
    assert!(!off.enabled, "default is off");
    for role in ["curator", "auditor", "guide", "judge"] {
        let (out, ran) = gate(off, &p, ledger(), role, RoleTickOutcome::Success);
        assert!(ran, "{role} runs with the flag off");
        assert_eq!(out, RoleTickOutcome::Success);
    }
    assert_eq!(calls.load(Ordering::SeqCst), 0, "flag off reads nothing");
}

#[test]
fn untriggered_roles_are_never_gated() {
    let (p, calls) = probe(Observation::Count(0));
    for role in ["judge", "doctor", "champion", "hermit"] {
        assert!(!is_triggered_role(role));
        let (_, ran) = gate(enabled(), &p, ledger(), role, RoleTickOutcome::Success);
        assert!(ran, "{role}");
    }
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

#[test]
fn decision_log_line_names_role_root_trigger_and_reason() {
    let d = should_launch("curator", &inputs(Observation::Count(0), None));
    let line = decision_log_line("curator", Path::new("/r/repo"), &d);
    for part in [
        "skip",
        "role=curator",
        "root=/r/repo",
        "trigger=debt:untriaged",
        "reason=",
    ] {
        assert!(line.contains(part), "{part} missing from {line}");
    }
    assert_eq!(Trigger::Idle.to_string(), "idle");
    assert_eq!(Trigger::Floor.to_string(), "floor");
}

// -- config -------------------------------------------------------------------

#[test]
fn config_defaults_off_and_parses_each_key_on_its_own() {
    let null = serde_json::Value::Null;
    assert_eq!(parse_event_trigger_config(&null, None), EventTriggerConfig::default());
    let block = serde_json::json!({"eventTriggers": {"enabled": true, "maxQuietSecs": 600}});
    let cfg = parse_event_trigger_config(&block, None);
    assert!(cfg.enabled);
    assert_eq!(cfg.max_quiet_secs, 600);
    let bad = serde_json::json!({"eventTriggers": {"enabled": "yes", "maxQuietSecs": 0}});
    assert_eq!(parse_event_trigger_config(&bad, None), EventTriggerConfig::default());
}

#[test]
fn env_overrides_config_both_ways() {
    let on = serde_json::json!({"eventTriggers": {"enabled": true}});
    let off = serde_json::Value::Null;
    assert!(!parse_event_trigger_config(&on, Some(false)).enabled);
    assert!(parse_event_trigger_config(&off, Some(true)).enabled);
    assert_eq!(parse_env_override(Some(" ON ")), Some(true));
    assert_eq!(parse_env_override(Some("0")), Some(false));
    assert_eq!(parse_env_override(Some("")), None);
    assert_eq!(parse_env_override(Some("maybe")), None);
    assert_eq!(parse_env_override(None), None);
}

// -- probes -------------------------------------------------------------------

fn git(dir: &Path, args: &[&str]) {
    let ok = std::process::Command::new("git")
        .args(["-c", "user.name=t", "-c", "user.email=t@example.com"])
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap()
        .status
        .success();
    assert!(ok, "git {args:?}");
}

#[test]
fn origin_main_probe_reads_the_local_ref_and_fails_open_without_it() {
    let tmp = tempfile::tempdir().unwrap();
    git(tmp.path(), &["init", "-q"]);
    git(tmp.path(), &["commit", "-q", "--allow-empty", "-m", "x"]);
    assert!(matches!(observe_origin_main(tmp.path()), Observation::Unobserved(_)));
    git(tmp.path(), &["update-ref", "refs/remotes/origin/main", "HEAD"]);
    match observe_origin_main(tmp.path()) {
        Observation::Fingerprint(sha) => assert_eq!(sha.len(), 40, "{sha}"),
        other => panic!("expected a SHA, got {other:?}"),
    }
}

// -- dispatcher wiring ----------------------------------------------------------

struct CountingRunner(Arc<AtomicUsize>);

impl RoleInvocationRunner for CountingRunner {
    fn invoke(&mut self, _role: &str, _prompt: &str) -> RoleTickOutcome {
        self.0.fetch_add(1, Ordering::SeqCst);
        RoleTickOutcome::Success
    }
}

fn dispatch_curator_once(config: &str, obs: Observation) -> (RoleTickOutcome, usize) {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let _enter = rt.enter();
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(tmp.path().join(".loom")).unwrap();
    std::fs::write(tmp.path().join(".loom").join("config.json"), config).unwrap();
    let invoked = Arc::new(AtomicUsize::new(0));
    let factory: RunnerFactory = {
        let invoked = Arc::clone(&invoked);
        Arc::new(move |_| Box::new(CountingRunner(Arc::clone(&invoked))))
    };
    let queue: QueueProbe = Arc::new(|_, _| Ok(true));
    let decide: DecideFn = Box::new(|root, in_progress, _| {
        admit_root_tick(
            root,
            "curator",
            "/loom:curator".to_string(),
            in_progress,
            &read_role_runner_config(root),
        )
    });
    let spec = *DEFAULT_ROLES.iter().find(|s| s.name == "curator").unwrap();
    let (p, _) = probe(obs);
    let mut d =
        RoleDispatcher::with_decide(spec, Duration::from_secs(300), factory, queue, None, decide)
            .with_triggers(p, ledger());
    let in_progress = new_in_progress_guard();
    let tick = d.dispatch_tick(vec![tmp.path().to_path_buf()], &in_progress);
    assert_eq!(tick.spawned.len(), 1, "the root is admitted; the gate runs inside the run");
    let (_, run) = rt
        .block_on(async { tokio::time::timeout(Duration::from_secs(30), d.join_next()).await })
        .unwrap()
        .unwrap()
        .unwrap();
    (run.outcome, invoked.load(Ordering::SeqCst))
}

#[test]
fn dispatcher_skips_an_enabled_curator_with_no_untriaged_issues() {
    let cfg = r#"{"autonomous":{"roleRunner":{"enabled":true,"eventTriggers":{"enabled":true}}}}"#;
    let (outcome, invoked) = dispatch_curator_once(cfg, Observation::Count(0));
    assert_eq!(outcome, RoleTickOutcome::QueueEmpty);
    assert_eq!(invoked, 0);
    let (outcome, invoked) = dispatch_curator_once(cfg, Observation::Count(2));
    assert_eq!((outcome, invoked), (RoleTickOutcome::Success, 1));
}

#[test]
fn dispatcher_with_the_flag_off_launches_curator_as_before() {
    let cfg = r#"{"autonomous":{"roleRunner":{"enabled":true}}}"#;
    let (outcome, invoked) = dispatch_curator_once(cfg, Observation::Count(0));
    assert_eq!((outcome, invoked), (RoleTickOutcome::Success, 1));
}
