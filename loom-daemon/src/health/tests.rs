use super::*;
use serial_test::serial;

fn now() -> DateTime<Utc> {
    DateTime::parse_from_rfc3339("2026-07-31T12:00:00Z")
        .unwrap()
        .with_timezone(&Utc)
}

// `pub(crate)` (Issue #8163): the `busy` sibling module's own test suite
// builds probe-timeout fixtures from the same install-state report, reused
// rather than re-derived so the two suites cannot drift about what
// "alive with a fresh heartbeat" looks like.
pub(crate) fn install_report(state: InstallState) -> InstallStateReport {
    InstallStateReport {
        state,
        started_at: Some("2026-07-30T00:00:00Z".to_string()),
        pid: matches!(state, InstallState::AliveStarting | InstallState::AliveButUnresponsive)
            .then_some(4321),
        liveness_detail: Some("launchd job gui/501/com.loom.daemon alive".to_string()),
        heartbeat_freshness: Some(HeartbeatFreshness::Fresh),
        heartbeat_age_secs: Some(5),
        heartbeat_stale_threshold_secs: Some(120),
        process_age_secs: Some(9),
        startup_grace_threshold_secs: Some(45),
        watchdog_log_path: PathBuf::from("/tmp/watchdog.log"),
        pid_file_stale_note: None,
    }
}

fn healthy_status() -> DaemonStatusReport {
    let mut status = DaemonStatusReport {
        capacity: crate::types::CapacityReport {
            ranking_present: true,
            total_accounts: 8,
            healthy_accounts: 6,
            exhausted_accounts: 2,
            token_axis_limit: 6,
            token_bound: false,
        },
        dynamic_cap: 7,
        work_finder_enabled: Some(true),
        // #4824: the healthy baseline is a daemon built from the SAME
        // commit as the client asking, on the default tick cadence — the
        // only state in which missing telemetry is attributable to the
        // daemon rather than to build skew.
        daemon_build_commit: Some(CLI_COMMIT.to_string()),
        work_finder_interval_secs: Some(60),
        ..Default::default()
    };
    status.last_work_finder_tick = Some(crate::types::WorkFinderTickSummary {
        at: now() - chrono::Duration::seconds(30),
        max_concurrent: 7,
        seen: 12,
        dispatched: 2,
        skipped_in_flight: 10,
        ..Default::default()
    });
    status
}

// `pub(crate)` (Issue #8091): the `operator_attention` sibling module's own
// test suite needs a fixture where every OTHER section is Green to pin
// "operator_attention alone does not change the exit code" — this is that
// known-good fixture, reused rather than re-derived so the two suites cannot
// silently drift into disagreeing about what "healthy" means.
pub(crate) fn healthy_inputs() -> HealthInputs {
    HealthInputs {
        at: now(),
        window: Duration::from_secs(DEFAULT_WINDOW_SECS),
        status: Some(healthy_status()),
        install_state: Some(install_report(InstallState::AliveButUnresponsive)),
        pgrep_pids: vec![4321],
        pid_file: None,
        ranking_present: true,
        ranking_age_secs: Some(240),
        // Healthy baseline: no class-scoped `.bad_tokens` state (#8058
        // Phase 3), so every fixture built from this one renders the exact
        // pre-#8058 single-number tokens line.
        token_class_capacity: None,
        pipeline: Some(vec![RepoPipelineSnapshot {
            root: PathBuf::from("/repos/loom"),
            queued: Some(5),
            merged_24h: Some(3),
            ..Default::default()
        }]),
        gh_unavailable: None,
        cli_build_commit: CLI_COMMIT.to_string(),
        // Healthy baseline: the auto_update loop reports disabled (the
        // default) — same "opted out" GREEN every fixture below builds
        // from unless a test explicitly enables/mutates it.
        self_update: Some(healthy_self_update()),
        // Healthy baseline: no codesign identity configured (the
        // overwhelmingly common case) -- nothing for `codesign_identity`
        // to report.
        codesign_preflight: None,
        // Healthy baseline: no host-load reading (#8163). `None` cannot
        // refute an `indeterminate-busy` story, so every pre-#8163 fixture
        // built from this one keeps its exact pre-#8163 verdict.
        load_per_core: None,
        // The not-collected optionals — `ipc_error`,
        // `work_finder_log_tick_age_secs`, and the #8349
        // `limit_calibration` reading — stay at their `Default` (`None`)
        // via the tail, so this fixture (and the section-inventory tests
        // it feeds) is unchanged by any future optional-signal field.
        ..Default::default()
    }
}

/// A synthetic up-to-date [`crate::self_update::SelfUpdateStatus`] — the
/// healthy baseline every `auto_update` fixture below builds from unless a
/// test explicitly makes it stale.
fn healthy_self_update() -> crate::self_update::SelfUpdateStatus {
    crate::self_update::SelfUpdateStatus {
        built_commit: CLI_COMMIT.to_string(),
        source_commit: Some(CLI_COMMIT.to_string()),
        update_available: Some(false),
        commits_behind: None,
        hours_behind: None,
    }
}

/// The client's build commit in every fixture below. A synthetic value, not
/// the real [`crate::self_update::BUILT_COMMIT`], so the skew assertions do
/// not depend on how the test binary happened to be built.
const CLI_COMMIT: &str = "18887b5c";

// ===================================================================
// #4694 regression pins — liveness precedence
// ===================================================================

/// The single most important assertion in this file: a reachable daemon is
/// GREEN even when the local install-state probe reports it dead. This is
/// the #4694 false negative — the launchd domain probe declaring a live,
/// dispatching daemon dead — and it must never be able to override an
/// answered IPC round-trip.
#[test]
fn ipc_reachable_beats_a_dead_launchd_verdict() {
    let mut inputs = healthy_inputs();
    inputs.install_state = Some(install_report(InstallState::ExpectedButDead));
    let section = assess_liveness(&inputs);
    assert_eq!(section.verdict, Verdict::Green);
    assert_eq!(section.detail["signal"], "ipc");
}

// ===================================================================
// #4774 regression pins — the pid file is advisory, cross-checked
// ===================================================================

/// Build a pid-file observation for the assess tests.
fn pid_obs(recorded: Option<u32>, alive: bool) -> crate::daemon_pidfile::PidFileObservation {
    crate::daemon_pidfile::PidFileObservation {
        path: PathBuf::from("/home/.loom/.daemon.pid"),
        present: recorded.is_some(),
        recorded_pid: recorded,
        recorded_pid_alive: alive,
    }
}

/// An inputs fixture whose daemon answered IPC and reported its own pid —
/// the post-#4774 wire shape every assertion below builds on.
fn reachable_inputs_with_socket_owner(pid: u32) -> HealthInputs {
    let mut inputs = healthy_inputs();
    let mut status = healthy_status();
    status.daemon_pid = Some(pid);
    inputs.status = Some(status);
    inputs
}

/// The healthy case must be *unchanged* by #4774: a pid file naming the
/// process that answered adds no note and no verdict change.
#[test]
fn a_pid_file_matching_the_socket_owner_stays_green() {
    let mut inputs = reachable_inputs_with_socket_owner(99917);
    inputs.pid_file = Some(pid_obs(Some(99917), true));
    let section = assess_liveness(&inputs);
    assert_eq!(section.verdict, Verdict::Green);
    assert_eq!(section.detail["pid_file_state"], "matches");
    assert_eq!(section.detail["socket_owner_pid"], 99917);
    assert!(!section.summary.contains("STALE"), "{}", section.summary);
}

/// THE #4774 pin. The 2026-07-31 incident, exactly: the file says 13724,
/// the daemon answering the socket says 99917. Liveness is not in doubt —
/// the daemon just answered — but the file is a booby trap for every other
/// consumer, so the section degrades and names both pids.
#[test]
fn a_pid_file_naming_a_different_process_degrades_and_is_named() {
    let mut inputs = reachable_inputs_with_socket_owner(99917);
    inputs.pid_file = Some(pid_obs(Some(13724), true));
    let section = assess_liveness(&inputs);
    assert_eq!(
        section.verdict,
        Verdict::Degraded,
        "a stale pid file must not be reported as healthy: {}",
        section.summary
    );
    assert_eq!(section.detail["pid_file_state"], "mismatch");
    assert!(
        section.summary.contains("13724") && section.summary.contains("99917"),
        "the summary must name both the recorded and the real pid: {}",
        section.summary
    );
    // Liveness itself is still positively established.
    assert_eq!(section.detail["ipc_reachable"], true);
}

/// A reachable daemon whose pid file records a pid that is neither the
/// socket owner nor a live process at all — a relaunch after the old pid
/// was recycled away. `classify()` still calls this "mismatch" (a live
/// socket owner is known, so it always outranks the file's own liveness
/// check) — the name reflects the verdict actually produced, not "dead".
#[test]
fn a_pid_file_naming_a_mismatched_dead_process_degrades() {
    let mut inputs = reachable_inputs_with_socket_owner(99917);
    inputs.pid_file = Some(pid_obs(Some(13724), false));
    let section = assess_liveness(&inputs);
    assert_eq!(section.verdict, Verdict::Degraded);
    assert_eq!(section.detail["pid_file_state"], "mismatch");
}

/// An absent pid file makes no false claim, so it is not an anomaly — a
/// daemon started outside the managed start path legitimately has none.
#[test]
fn an_absent_pid_file_does_not_degrade_a_reachable_daemon() {
    let mut inputs = reachable_inputs_with_socket_owner(99917);
    inputs.pid_file = Some(pid_obs(None, false));
    let section = assess_liveness(&inputs);
    assert_eq!(section.verdict, Verdict::Green);
    assert_eq!(section.detail["pid_file_state"], "absent");
}

/// Backward compatibility: a **pre-#4774 daemon** answers without a
/// `daemon_pid`, so there is nothing to cross-check against. That must read
/// as "unverified", never as a mismatch — an upgrade-order false alarm would
/// be worse than the bug this issue fixes, since it would fire on every
/// fleet host until the last daemon rolled.
#[test]
fn a_pre_4774_daemon_never_produces_a_false_mismatch() {
    let mut inputs = healthy_inputs();
    let mut status = healthy_status();
    status.daemon_pid = None;
    inputs.status = Some(status);
    inputs.pid_file = Some(pid_obs(Some(13724), true));
    let section = assess_liveness(&inputs);
    assert_eq!(section.verdict, Verdict::Green);
    assert_eq!(section.detail["pid_file_state"], "unverified");
    assert!(section.detail["socket_owner_pid"].is_null());
}

/// The daemon's self-reported pid outranks the install-state probe's pid
/// for the rendered `pid` — it is `std::process::id()` from inside the
/// answering process, versus a launchd/pid-file inference from outside.
#[test]
fn the_reported_pid_prefers_the_daemons_own_over_the_probes() {
    // `install_report` pins the probe's pid at 4321.
    let inputs = reachable_inputs_with_socket_owner(99917);
    let section = assess_liveness(&inputs);
    assert_eq!(section.detail["pid"], 99917);
    assert!(section.summary.contains("99917"), "{}", section.summary);
}

/// On the *unreachable* path there is no `daemon_pid` to arbitrate with, so
/// the note comes from the install-state probe's own launchd cross-check —
/// and it must reach the operator-facing summary, not just the JSON.
#[test]
fn an_unreachable_daemons_stale_note_reaches_the_summary() {
    let mut inputs = healthy_inputs();
    inputs.status = None;
    inputs.ipc_error = Some("round-trip timed out".to_string());
    let mut report = install_report(InstallState::AliveButUnresponsive);
    report.pid_file_stale_note = Some("STALE pid file /home/.loom/.daemon.pid: …".to_string());
    inputs.install_state = Some(report);
    let section = assess_liveness(&inputs);
    assert_eq!(section.verdict, Verdict::Degraded);
    assert!(
        section.summary.contains("STALE pid file"),
        "the install-state probe's #4774 note must be surfaced: {}",
        section.summary
    );
    // Regression pin: a hand-joined literal + suffix_note() must not
    // leave a double-space run in the operator-facing summary — fmt and
    // clippy cannot see this class of bug, only a runtime assertion can.
    assert!(!section.summary.contains("  "), "double space in summary: {}", section.summary);
}

/// The launchd domain probe alone can never produce a DEAD verdict: with
/// IPC unreachable and the install-state classification negative, a live
/// `pgrep` pid still holds the verdict at DEGRADED.
#[test]
fn pgrep_blocks_a_dead_verdict_when_launchd_and_pidfile_are_negative() {
    let mut inputs = healthy_inputs();
    inputs.status = None;
    inputs.ipc_error = Some("connect failed".to_string());
    inputs.install_state = Some(install_report(InstallState::ExpectedButDead));
    inputs.pgrep_pids = vec![9911];
    let section = assess_liveness(&inputs);
    assert_eq!(section.verdict, Verdict::Degraded);
    assert_eq!(section.detail["signal"], "pgrep");
    assert_eq!(section.detail["declared_dead"], false);
}

/// An install-state classification of "alive but unresponsive" is DEGRADED,
/// never DEAD — the daemon is running, it is just not answering. Uses a
/// **non-timeout** IPC failure (`connect failed`) so this stays pinned to
/// the harder-failure branch; #6103 carved the lone-timeout case out into
/// its own `Unknown` verdict — see
/// `a_single_probe_timeout_against_a_confirmed_alive_daemon_is_unknown_not_degraded`
/// below.
#[test]
fn alive_but_unresponsive_is_degraded_not_dead() {
    let mut inputs = healthy_inputs();
    inputs.status = None;
    inputs.ipc_error = Some("connect failed".to_string());
    inputs.pgrep_pids = vec![];
    let section = assess_liveness(&inputs);
    assert_eq!(section.verdict, Verdict::Degraded);
    assert!(section.summary.contains("NOT dead"));
    // Regression pin: the hand-joined literal must not leave a double
    // space before {ipc_error} — fmt/clippy cannot see this class of bug.
    assert!(!section.summary.contains("  "), "double space in summary: {}", section.summary);
}

/// #6103: a single simulated IPC *timeout* against a daemon this
/// install-state classification already cross-checked as alive must not
/// be treated as "confirmed unhealthy" — the reported false alarm was
/// exactly this shape (29 consecutive watchdog OK ticks and 5/5 immediate
/// manual IPC probes against a daemon `health` called DEGRADED). The
/// liveness section itself reads `Unknown` ("could not determine"), and
/// that alone must not flip `overall` to `Degraded` either.
///
/// #6191: this exact fixture (status unreachable, a lone timeout, and
/// `install_report`'s default fresh heartbeat) is also the precise "busy"
/// shape #6191 was filed against — the `--json` report this issue quotes
/// verbatim. `overall` now carries the distinct `IndeterminateBusy`
/// verdict at its own exit code, rather than the same `EXIT_DEGRADED` a
/// genuine degradation uses — see
/// `health::busy`'s own suite (#8163) for the negative cases: no heartbeat
/// corroboration, a hard non-timeout failure, and an idle host whose load
/// average refutes the busy story all stay ordinary `Unknown`/exit 1.
#[test]
fn a_single_probe_timeout_against_a_confirmed_alive_daemon_is_unknown_not_degraded() {
    let mut inputs = healthy_inputs();
    inputs.status = None;
    inputs.ipc_error = Some("round-trip timed out after 2s".to_string());
    inputs.pgrep_pids = vec![];
    let section = assess_liveness(&inputs);
    assert_eq!(
        section.verdict,
        Verdict::Unknown,
        "a lone probe-budget miss against a demonstrably alive daemon must read as 'could \
             not determine', not 'confirmed degraded': {}",
        section.summary
    );
    assert!(!section.summary.contains("  "), "double space in summary: {}", section.summary);

    let report = assess(&inputs);
    assert_ne!(
        report.overall,
        Verdict::Degraded,
        "a single simulated timeout must not, by itself, flip `overall` to DEGRADED: {}",
        report.render_human()
    );
    assert_eq!(
        report.overall,
        Verdict::IndeterminateBusy,
        "corroborated alive + fresh heartbeat must promote overall to the busy verdict, not \
             leave it at the ordinary Unknown: {}",
        report.render_human()
    );
    assert_eq!(report.exit_code(), EXIT_INDETERMINATE_BUSY);
}

// The two negative cases for the test above — a STALE heartbeat, and a HARD
// (non-timeout) IPC failure — moved to `health::busy`'s own suite in #8163,
// alongside the `probe_budget_busy` logic they pin and the new load-average
// corroboration that joined it.

/// Pure classification pin for [`alive_with_fresh_heartbeat`]: both
/// signals — a live pid AND a fresh heartbeat — are required; either
/// alone is not enough corroboration.
#[test]
fn alive_with_fresh_heartbeat_requires_both_signals() {
    assert!(!alive_with_fresh_heartbeat(None));

    let fresh = install_report(InstallState::AliveButUnresponsive);
    assert!(alive_with_fresh_heartbeat(Some(&fresh)));

    let mut stale = fresh.clone();
    stale.heartbeat_freshness = Some(HeartbeatFreshness::Stale);
    assert!(!alive_with_fresh_heartbeat(Some(&stale)));

    let mut unknown_heartbeat = fresh.clone();
    unknown_heartbeat.heartbeat_freshness = None;
    assert!(!alive_with_fresh_heartbeat(Some(&unknown_heartbeat)));

    let dead = install_report(InstallState::ExpectedButDead);
    assert!(!alive_with_fresh_heartbeat(Some(&dead)));
}

/// `overall: "indeterminate-busy"` round-trips through `--json` with the
/// hyphenated spelling AC2/AC3 (#6191) specify, not the container's
/// default `#[serde(rename_all = "lowercase")]` mangling.
#[test]
fn indeterminate_busy_serializes_with_a_hyphen() {
    let mut inputs = healthy_inputs();
    inputs.status = None;
    inputs.ipc_error = Some("round-trip timed out after 2s".to_string());
    inputs.pgrep_pids = vec![];
    let report = assess(&inputs);
    assert_eq!(report.overall, Verdict::IndeterminateBusy);
    let value = serde_json::to_value(&report).unwrap();
    assert_eq!(value["overall"], "indeterminate-busy");
}

/// Pure classification pin for [`ipc_error_is_probe_timeout`]: only a
/// budget-exceeded message reads as "just a timeout" — a hard failure
/// (connection refused, an explicit daemon-side error) never does, even
/// though both equally mean "no `DaemonStatusReport` this invocation".
#[test]
fn ipc_error_is_probe_timeout_classifies_timeouts_only() {
    assert!(ipc_error_is_probe_timeout("round-trip timed out after 2s"));
    assert!(ipc_error_is_probe_timeout("connect timed out after 2s"));
    assert!(!ipc_error_is_probe_timeout(
        "connect failed: No such file or directory (os error 2)"
    ));
    assert!(!ipc_error_is_probe_timeout("daemon error: internal"));
    assert!(!ipc_error_is_probe_timeout("unexpected response: DaemonStatus(..)"));
}

#[test]
fn alive_starting_is_degraded_not_dead() {
    let mut inputs = healthy_inputs();
    inputs.status = None;
    inputs.ipc_error = Some("connect failed".to_string());
    inputs.install_state = Some(install_report(InstallState::AliveStarting));
    inputs.pgrep_pids = vec![];
    let section = assess_liveness(&inputs);
    assert_eq!(section.verdict, Verdict::Degraded);
    assert!(section.summary.contains("STARTING"));
    // Regression pin: the hand-joined literal must not leave a double
    // space before {ipc_error} — fmt/clippy cannot see this class of bug.
    assert!(!section.summary.contains("  "), "double space in summary: {}", section.summary);
}

/// Only when *all three* signals are negative is the daemon declared dead.
#[test]
fn all_three_signals_negative_declares_dead() {
    let mut inputs = healthy_inputs();
    inputs.status = None;
    inputs.ipc_error = Some("connect failed".to_string());
    inputs.install_state = Some(install_report(InstallState::ExpectedButDead));
    inputs.pgrep_pids = vec![];
    let section = assess_liveness(&inputs);
    assert_eq!(section.verdict, Verdict::Dead);
    assert_eq!(section.detail["signal"], "all-negative");
}

/// An undiagnosable probe is UNKNOWN (exit 1), never DEAD (exit 2).
#[test]
fn undiagnosable_is_unknown_never_dead() {
    let mut inputs = healthy_inputs();
    inputs.status = None;
    inputs.ipc_error = Some("connect failed".to_string());
    inputs.install_state = None;
    inputs.pgrep_pids = vec![];
    let section = assess_liveness(&inputs);
    assert_eq!(section.verdict, Verdict::Unknown);
    assert_eq!(assess(&inputs).exit_code(), EXIT_DEGRADED);
}

// ===================================================================
// Exit-code contract
// ===================================================================

#[test]
fn healthy_inputs_exit_zero() {
    let report = assess(&healthy_inputs());
    assert_eq!(report.overall, Verdict::Green, "{}", report.render_human());
    assert_eq!(report.exit_code(), EXIT_HEALTHY);
}

#[test]
fn any_degraded_section_exits_one() {
    let mut inputs = healthy_inputs();
    inputs.status.as_mut().unwrap().capacity.healthy_accounts = 0;
    let report = assess(&inputs);
    assert_eq!(report.overall, Verdict::Degraded);
    assert_eq!(report.exit_code(), EXIT_DEGRADED);
}

#[test]
fn dead_daemon_exits_two_and_marks_other_sections_unknown() {
    let mut inputs = healthy_inputs();
    inputs.status = None;
    inputs.ipc_error = Some("connect failed".to_string());
    inputs.install_state = Some(install_report(InstallState::ExpectedButDead));
    inputs.pgrep_pids = vec![];
    let report = assess(&inputs);
    assert_eq!(report.overall, Verdict::Dead);
    assert_eq!(report.exit_code(), EXIT_DEAD);
    assert_eq!(report.section("dispatch").unwrap().verdict, Verdict::Unknown);
    assert_eq!(report.section("tokens").unwrap().verdict, Verdict::Unknown);
}

// ===================================================================
// Observability section (#4830)
// ===================================================================

/// An inputs fixture whose daemon has confirmed a host-identity mismatch —
/// the live 2026-07-31 shape (the Studio's key bound to `robb-pro`).
fn mismatched_inputs(age_secs: i64) -> HealthInputs {
    let mut inputs = healthy_inputs();
    inputs
        .status
        .as_mut()
        .unwrap()
        .observability_host_id_mismatch = Some(crate::types::ObservabilityHostIdMismatch {
        daemon_host_id: "robb-studio".to_string(),
        ingest_host_id: "robb-pro".to_string(),
        first_seen_at: now() - chrono::Duration::seconds(age_secs),
    });
    inputs
}

#[test]
fn no_observability_section_when_the_host_ids_agree() {
    // AC: "no behavior change when they match" — a healthy daemon's report
    // is byte-for-byte what it was before #4830.
    let report = assess(&healthy_inputs());
    assert!(report.section("observability").is_none());
    assert_eq!(report.overall, Verdict::Green);
    assert_eq!(report.exit_code(), EXIT_HEALTHY);
}

#[test]
fn no_observability_section_for_a_disabled_or_keyless_exporter() {
    // A disabled exporter never registers a status handle, so the field is
    // `None` on the wire — indistinguishable, by design, from "enabled and
    // correctly bound".
    let mut inputs = healthy_inputs();
    inputs
        .status
        .as_mut()
        .unwrap()
        .observability_host_id_mismatch = None;
    assert!(assess_observability(&inputs).is_none());
}

#[test]
fn no_observability_section_when_the_daemon_is_unreachable() {
    let mut inputs = healthy_inputs();
    inputs.status = None;
    assert!(assess_observability(&inputs).is_none());
}

#[test]
fn a_host_id_mismatch_is_a_degraded_observability_note() {
    let report = assess(&mismatched_inputs(3600));
    let section = report.section("observability").expect("section present");
    assert_eq!(section.verdict, Verdict::Degraded);
    assert!(
        section.summary.contains("robb-pro") && section.summary.contains("robb-studio"),
        "the note must name BOTH identities: {}",
        section.summary
    );
    assert_eq!(section.detail["daemon_host_id"], "robb-studio");
    assert_eq!(section.detail["ingest_host_id"], "robb-pro");
    assert_eq!(section.detail["first_seen_age_secs"], 3600);
    assert_eq!(report.overall, Verdict::Degraded);
    assert_eq!(report.exit_code(), EXIT_DEGRADED);
}

// ===================================================================
// Dispatch section
// ===================================================================

#[test]
fn dispatch_reports_last_tick_reason_summary() {
    let section = assess_dispatch(&healthy_inputs());
    assert_eq!(section.verdict, Verdict::Green);
    assert!(section
        .summary
        .contains("12 seen, 2 dispatched, 10 in-flight-skip"));
}

/// #5177 AC4 (re-scoped #5270): `disk_binds_cap` is the strict-minimum
/// test against `configured_max` (never merely tied), and ties WITH ram
/// resolve to disk (matching `calibrate::binding_term`'s tie-break order).
/// The token axis no longer participates in the cap at all.
#[test]
fn disk_binds_cap_only_when_smallest_or_tied_with_ram() {
    // disk 2 < ram (unbounded) and < configured 12 → disk binds.
    assert!(disk_binds_cap(2, usize::MAX, 12));
    // disk ties configured_max → operator's own ceiling, not a disk fault.
    assert!(!disk_binds_cap(12, usize::MAX, 12));
    // disk larger than configured_max → the ceiling binds, not disk.
    assert!(!disk_binds_cap(5, usize::MAX, 1));
    // disk larger than ram → ram binds, not disk (see ram_binds_cap below).
    assert!(!disk_binds_cap(5, 2, 12));
    // disk ties ram (both smaller than configured_max) → disk wins the tie.
    assert!(disk_binds_cap(3, 3, 12));
    // the healthy default fixture (disk 0, ram 0, configured 0) must NOT
    // trip it — a 0/0/0 tie is "unconfigured", not "throttling".
    assert!(!disk_binds_cap(0, 0, 0));
}

/// #5270: `ram_binds_cap` is the RAM-headroom mirror of the disk test
/// above — RAM must be the UNIQUE smallest term (a tie resolves to disk).
#[test]
fn ram_binds_cap_only_when_uniquely_smallest() {
    // ram 2 < disk (unbounded) and < configured 12 → ram binds.
    assert!(ram_binds_cap(usize::MAX, 2, 12));
    // ram ties configured_max → operator's own ceiling, not a ram fault.
    assert!(!ram_binds_cap(usize::MAX, 12, 12));
    // ram larger than configured_max → the ceiling binds, not ram.
    assert!(!ram_binds_cap(usize::MAX, 5, 1));
    // ram larger than disk → disk binds, not ram.
    assert!(!ram_binds_cap(2, 5, 12));
    // ram ties disk → disk wins the tie (not flagged as ram).
    assert!(!ram_binds_cap(3, 3, 12));
    // the healthy default fixture (disk 0, ram 0, configured 0) must NOT
    // trip it.
    assert!(!ram_binds_cap(0, 0, 0));
}

/// #5177 AC4: when disk headroom is the binding cap term, the dispatch
/// section is Degraded and names disk as the cause — not a silent green
/// daemon dispatching at a fraction of its token/config capacity.
#[test]
fn disk_bound_cap_produces_degraded_verdict_naming_disk() {
    let mut inputs = healthy_inputs();
    {
        let status = inputs.status.as_mut().unwrap();
        status.capacity.token_axis_limit = 6;
        status.configured_max = 12;
        status.disk_headroom = 2; // strictly the smallest term
        status.ram_headroom = 40; // ample RAM — must not also fire
        status.dynamic_cap = 2;
    }
    let section = assess_dispatch(&inputs);
    assert_eq!(section.verdict, Verdict::Degraded);
    assert!(
        section.summary.to_lowercase().contains("disk"),
        "summary must name disk: {}",
        section.summary
    );
}

/// #5270: the RAM-headroom mirror of the disk test above — a critically-low
/// available-memory host must be named as degraded the same way a
/// critically-low-disk host already is.
#[test]
fn ram_bound_cap_produces_degraded_verdict_naming_ram() {
    let mut inputs = healthy_inputs();
    {
        let status = inputs.status.as_mut().unwrap();
        status.configured_max = 12;
        status.disk_headroom = 40; // ample disk — must not also fire
        status.ram_headroom = 1; // strictly the smallest term
        status.dynamic_cap = 1;
    }
    let section = assess_dispatch(&inputs);
    assert_eq!(section.verdict, Verdict::Degraded);
    assert!(
        section.summary.to_lowercase().contains("ram"),
        "summary must name ram: {}",
        section.summary
    );
}

/// #5270: a host where the CPU saturation admission brake is HOLDING new
/// admissions must be named as degraded, even when the numeric disk/ram/
/// ceiling terms are all comfortably ample — the brake is a point-in-time
/// gate outside the `min(...)` formula (see `crate::admission_brake`).
#[test]
fn cpu_brake_held_produces_degraded_verdict_naming_cpu() {
    let mut inputs = healthy_inputs();
    {
        let status = inputs.status.as_mut().unwrap();
        status.configured_max = 12;
        status.disk_headroom = 40;
        status.ram_headroom = 40;
        status.dynamic_cap = 12;
        status.admission_brake = Some(crate::types::AdmissionBrakeStatus {
            enabled: true,
            held: true,
            load_per_core: Some(1.10),
            load_per_core_threshold: 0.95,
            held_since: None,
            held_ticks: 3,
            starving_since: None,
            starving_ticks: 0,
            escape_hatch_grants: 0,
        });
    }
    let section = assess_dispatch(&inputs);
    assert_eq!(section.verdict, Verdict::Degraded);
    assert!(
        section.summary.to_lowercase().contains("cpu"),
        "summary must name cpu: {}",
        section.summary
    );
    assert!(section.summary.contains("1.10"), "{}", section.summary);
}

/// #5270: a brake that exists but is NOT holding must not itself degrade
/// the section (mirrors the existing disk/ram negative test below).
#[test]
fn cpu_brake_not_holding_is_not_flagged() {
    let mut inputs = healthy_inputs();
    {
        let status = inputs.status.as_mut().unwrap();
        status.configured_max = 12;
        status.disk_headroom = 40;
        status.ram_headroom = 40;
        status.dynamic_cap = 12;
        status.admission_brake = Some(crate::types::AdmissionBrakeStatus {
            enabled: true,
            held: false,
            load_per_core: Some(0.10),
            load_per_core_threshold: 0.95,
            held_since: None,
            held_ticks: 0,
            starving_since: None,
            starving_ticks: 0,
            escape_hatch_grants: 0,
        });
    }
    let section = assess_dispatch(&inputs);
    assert_eq!(section.verdict, Verdict::Green);
}

/// #5177 AC4 (negative): a cap bound by the operator's configured ceiling
/// (disk/ram headroom ample) stays green — neither is the culprit there.
#[test]
fn config_bound_cap_is_not_flagged_as_disk() {
    let mut inputs = healthy_inputs();
    {
        let status = inputs.status.as_mut().unwrap();
        status.capacity.token_axis_limit = 6;
        status.configured_max = 4; // operator's own ceiling binds
        status.disk_headroom = 50; // plenty of disk
        status.ram_headroom = 50; // plenty of ram
        status.dynamic_cap = 4;
    }
    let section = assess_dispatch(&inputs);
    assert_eq!(section.verdict, Verdict::Green);
}

/// Inputs with no tick reported, a daemon well past the warm-up grace
/// window, a matching build, and no corroborating log line — the one state
/// in which "no tick" really does mean the work finder is dead (#4824).
fn missing_tick_inputs() -> HealthInputs {
    let mut inputs = healthy_inputs();
    inputs.status.as_mut().unwrap().last_work_finder_tick = None;
    let mut report = install_report(InstallState::AliveButUnresponsive);
    report.process_age_secs = Some(3600);
    inputs.install_state = Some(report);
    inputs
}

#[test]
fn dispatch_missing_tick_is_degraded_when_work_finder_is_enabled() {
    let section = assess_dispatch(&missing_tick_inputs());
    assert_eq!(section.verdict, Verdict::Degraded);
}

#[test]
fn dispatch_missing_tick_is_green_when_work_finder_is_disabled_and_nothing_else_is_wrong() {
    let mut inputs = healthy_inputs();
    let status = inputs.status.as_mut().unwrap();
    status.last_work_finder_tick = None;
    status.work_finder_enabled = Some(false);
    // The DISABLED work finder is itself the (single) degradation.
    let section = assess_dispatch(&inputs);
    assert_eq!(section.verdict, Verdict::Degraded);
    assert!(section.summary.contains("DISABLED"));
    assert!(!section
        .summary
        .contains("no work-finder tick observed in this daemon process"));
}

// ===================================================================
// #4824 regression pins — build skew and the post-restart grace window
// must not be reported as a dead work finder
// ===================================================================

/// The message a genuinely dead work finder produces. Asserted absent in
/// every false-DEGRADED case below.
const DEAD_WORK_FINDER_MSG: &str = "no work-finder tick observed in this daemon process";

/// THE #4824 pin, mode 1. A `health` built from HEAD querying a daemon
/// built one commit earlier: the daemon cannot report a counter it does not
/// have, and rendering that absence as "work finder dead" paged operators
/// on a fleet that was demonstrably dispatching.
#[test]
fn dispatch_missing_tick_reports_build_skew_not_a_dead_work_finder() {
    let mut inputs = missing_tick_inputs();
    inputs.status.as_mut().unwrap().daemon_build_commit = Some("105f9c12".to_string());
    let section = assess_dispatch(&inputs);
    assert_eq!(section.verdict, Verdict::Green, "{}", section.summary);
    assert!(section.summary.contains("105f9c12"), "{}", section.summary);
    assert!(section.summary.contains("predates tick telemetry"), "{}", section.summary);
    assert!(section.summary.contains("loom-daemon-update.sh"), "{}", section.summary);
    assert!(!section.summary.contains(DEAD_WORK_FINDER_MSG), "{}", section.summary);
    assert_eq!(section.detail["missing_tick"]["reason"], "build_skew");
}

/// A daemon predating #4824 reports no commit at all. That is *also* skew
/// (it necessarily predates every field added since), and must be named as
/// such rather than collapsed into a dead-loop verdict.
#[test]
fn dispatch_missing_tick_from_a_daemon_with_no_reported_commit_is_skew() {
    let mut inputs = missing_tick_inputs();
    inputs.status.as_mut().unwrap().daemon_build_commit = None;
    let section = assess_dispatch(&inputs);
    assert_eq!(section.verdict, Verdict::Green, "{}", section.summary);
    assert!(section.summary.contains("<unreported>"), "{}", section.summary);
    assert!(!section.summary.contains(DEAD_WORK_FINDER_MSG), "{}", section.summary);
}

/// AC4: exit code stays 0 under skew when everything else is green.
#[test]
fn build_skew_alone_does_not_change_the_exit_code() {
    let mut inputs = missing_tick_inputs();
    inputs.status.as_mut().unwrap().daemon_build_commit = Some("105f9c12".to_string());
    assert_eq!(assess(&inputs).exit_code(), EXIT_HEALTHY);
}

/// THE #4824 pin, mode 2. Immediately after `loom-daemon restart` the
/// per-process telemetry is legitimately empty for up to one tick interval.
#[test]
fn dispatch_missing_tick_within_the_grace_window_reports_warming_up() {
    let mut inputs = missing_tick_inputs();
    let mut report = install_report(InstallState::AliveButUnresponsive);
    report.process_age_secs = Some(30);
    inputs.install_state = Some(report);
    let section = assess_dispatch(&inputs);
    assert_eq!(section.verdict, Verdict::Green, "{}", section.summary);
    assert!(section.summary.contains("warming up"), "{}", section.summary);
    assert!(!section.summary.contains(DEAD_WORK_FINDER_MSG), "{}", section.summary);
    assert_eq!(section.detail["missing_tick"]["reason"], "warming_up");
    assert_eq!(assess(&inputs).exit_code(), EXIT_HEALTHY);
}

/// The grace window is `2 ×` the daemon's OWN resolved interval, not the
/// default: a daemon on a 300s cadence must not false-alarm for its whole
/// first interval after a roll.
#[test]
fn the_grace_window_scales_with_the_daemons_own_tick_interval() {
    let mut inputs = missing_tick_inputs();
    let mut report = install_report(InstallState::AliveButUnresponsive);
    report.process_age_secs = Some(400);
    inputs.install_state = Some(report);

    // Default 60s cadence ⇒ 120s grace ⇒ 400s old is well past it.
    inputs.status.as_mut().unwrap().work_finder_interval_secs = Some(60);
    assert_eq!(assess_dispatch(&inputs).verdict, Verdict::Degraded);

    // 300s cadence ⇒ 600s grace ⇒ still warming up.
    inputs.status.as_mut().unwrap().work_finder_interval_secs = Some(300);
    let section = assess_dispatch(&inputs);
    assert_eq!(section.verdict, Verdict::Green, "{}", section.summary);
    assert!(section.summary.contains("warming up"), "{}", section.summary);
}

/// A pre-#4824 daemon reports no interval; fall back to the default cadence
/// rather than treating the absence as "no grace at all".
#[test]
fn an_unreported_tick_interval_falls_back_to_the_default_cadence() {
    let mut status = healthy_status();
    status.work_finder_interval_secs = None;
    assert_eq!(
        work_finder_grace_secs(&status),
        crate::work_finder::DEFAULT_WORK_FINDER_INTERVAL_SECS * WORK_FINDER_TICK_GRACE_INTERVALS
    );
}

/// AC5: the daemon log is consulted as a corroborating signal before the
/// work finder is declared dead — the exact 2026-07-31 disagreement, where
/// `health` said "no tick" while `daemon.log` carried one every ~60s.
#[test]
fn recent_work_finder_log_activity_blocks_a_dead_verdict() {
    let mut inputs = missing_tick_inputs();
    inputs.work_finder_log_tick_age_secs = Some(45);
    let section = assess_dispatch(&inputs);
    assert_eq!(section.verdict, Verdict::Green, "{}", section.summary);
    assert!(section.summary.contains("daemon log"), "{}", section.summary);
    assert!(!section.summary.contains(DEAD_WORK_FINDER_MSG), "{}", section.summary);
    assert_eq!(section.detail["missing_tick"]["reason"], "log_corroborated");
}

/// Corroboration can only *soften* the verdict, never harden it — and a log
/// line older than the grace window corroborates nothing.
#[test]
fn stale_work_finder_log_activity_does_not_block_a_dead_verdict() {
    let mut inputs = missing_tick_inputs();
    inputs.work_finder_log_tick_age_secs = Some(9_999);
    assert_eq!(assess_dispatch(&inputs).verdict, Verdict::Degraded);
}

/// THE regression guard. With a matching build, a daemon long past the
/// grace window, and no corroborating log activity, a missing tick is still
/// a fault — this fix must not be able to silence a genuinely dead work
/// finder.
#[test]
fn a_genuinely_dead_work_finder_is_still_degraded() {
    let inputs = missing_tick_inputs();
    let section = assess_dispatch(&inputs);
    assert_eq!(section.verdict, Verdict::Degraded);
    assert!(section.summary.contains(DEAD_WORK_FINDER_MSG), "{}", section.summary);
    assert_eq!(section.detail["missing_tick"]["reason"], "dead");
    assert_eq!(assess(&inputs).exit_code(), EXIT_DEGRADED);
}

/// Warm-up outranks skew: a just-restarted daemon on a *different* commit
/// is reported as warming up, the explanation that is true regardless of
/// build state.
#[test]
fn warming_up_outranks_build_skew() {
    let mut inputs = missing_tick_inputs();
    inputs.status.as_mut().unwrap().daemon_build_commit = Some("105f9c12".to_string());
    let mut report = install_report(InstallState::AliveButUnresponsive);
    report.process_age_secs = Some(5);
    inputs.install_state = Some(report);
    assert_eq!(
        classify_missing_tick(&inputs, inputs.status.as_ref().unwrap()),
        MissingTick::WarmingUp {
            age_secs: 5,
            grace_secs: 120
        }
    );
}

// -------------------------------------------------------------------
// #4824 — build-skew classification
// -------------------------------------------------------------------

#[test]
fn identical_commits_are_a_match() {
    assert_eq!(classify_build_skew("abc1234", Some("abc1234")), BuildSkew::Match);
    assert!(!BuildSkew::Match.may_predate_client());
}

#[test]
fn differing_commits_are_skew() {
    assert_eq!(
        classify_build_skew("abc1234", Some("def5678")),
        BuildSkew::Skew("def5678".to_string())
    );
    assert!(classify_build_skew("abc1234", Some("def5678")).may_predate_client());
}

#[test]
fn an_absent_daemon_commit_is_daemon_unknown() {
    assert_eq!(classify_build_skew("abc1234", None), BuildSkew::DaemonUnknown);
    assert!(BuildSkew::DaemonUnknown.may_predate_client());
}

/// A tarball build bakes in `"unknown"`. It must never be read as a commit
/// that happens to differ from every real one — that would permanently
/// suppress the dead-work-finder verdict on such an install.
#[test]
fn an_unknown_commit_on_either_side_is_incomparable() {
    assert_eq!(classify_build_skew("unknown", Some("def5678")), BuildSkew::Incomparable);
    assert_eq!(classify_build_skew("abc1234", Some("unknown")), BuildSkew::Incomparable);
    assert!(!BuildSkew::Incomparable.may_predate_client());
}

/// …and an incomparable build must therefore still produce a dead verdict
/// when nothing else explains the silence.
#[test]
fn an_incomparable_build_still_reports_a_dead_work_finder() {
    let mut inputs = missing_tick_inputs();
    inputs.cli_build_commit = "unknown".to_string();
    assert_eq!(assess_dispatch(&inputs).verdict, Verdict::Degraded);
}

// -------------------------------------------------------------------
// #4824 — the daemon-log corroborating probe (pure half)
// -------------------------------------------------------------------

fn log_now() -> chrono::NaiveDateTime {
    chrono::NaiveDateTime::parse_from_str("2026-07-31T14:30:00.000Z", DAEMON_LOG_STAMP_FORMAT)
        .unwrap()
}

#[test]
fn log_probe_reads_the_newest_work_finder_line() {
    let log = "\
[2026-07-31T14:20:00.000Z] [INFO] work_finder: tick — cap 16; 12 seen, 0 dispatched
[2026-07-31T14:29:30.500Z] [INFO] work_finder: tick — cap 16; 12 seen, 2 dispatched
[2026-07-31T14:29:59.000Z] [INFO] sweep_registry: reaped 1 finished sweep
";
    assert_eq!(work_finder_log_tick_age_secs(log, log_now()), Some(29));
}

#[test]
fn log_probe_returns_none_without_a_work_finder_line() {
    let log = "[2026-07-31T14:29:59.000Z] [INFO] sweep_registry: reaped 1 finished sweep\n";
    assert_eq!(work_finder_log_tick_age_secs(log, log_now()), None);
}

/// The tail read starts mid-line, so the first line is routinely a fragment
/// with no parseable `[stamp]`. It must be skipped, not abort the scan.
#[test]
fn log_probe_skips_an_unparseable_partial_first_line() {
    let log = "ck — cap 16; 12 seen, 0 dispatched  work_finder: partial\n\
[2026-07-31T14:28:00.000Z] [INFO] work_finder: tick — cap 16; 12 seen, 1 dispatched\n";
    assert_eq!(work_finder_log_tick_age_secs(log, log_now()), Some(120));
}

/// Clock skew (the log stamped ahead of this process) reads as age 0, never
/// an underflow.
#[test]
fn log_probe_clamps_a_future_stamp_to_zero() {
    let log = "[2026-07-31T14:35:00.000Z] [INFO] work_finder: tick — cap 16\n";
    assert_eq!(work_finder_log_tick_age_secs(log, log_now()), Some(0));
}

#[test]
fn dispatch_flags_a_halted_gate() {
    let mut inputs = healthy_inputs();
    inputs.status.as_mut().unwrap().main_health_gate_halted = true;
    let section = assess_dispatch(&inputs);
    assert_eq!(section.verdict, Verdict::Degraded);
    assert!(section.summary.contains("HALTED"));
}

#[test]
fn dispatch_flags_tick_errors() {
    let mut inputs = healthy_inputs();
    inputs
        .status
        .as_mut()
        .unwrap()
        .last_work_finder_tick
        .as_mut()
        .unwrap()
        .errors = 3;
    let section = assess_dispatch(&inputs);
    assert_eq!(section.verdict, Verdict::Degraded);
    assert!(section.summary.contains("3 dispatch error(s)"));
}

// ===================================================================
// Tokens section
// ===================================================================

#[test]
fn tokens_green_when_healthy_and_ranking_fresh() {
    let section = assess_tokens(&healthy_inputs());
    assert_eq!(section.verdict, Verdict::Green);
    assert!(section.summary.starts_with("6/8 healthy (2 exhausted)"));
}

#[test]
fn tokens_degraded_when_all_exhausted() {
    let mut inputs = healthy_inputs();
    let cap = &mut inputs.status.as_mut().unwrap().capacity;
    cap.healthy_accounts = 0;
    cap.exhausted_accounts = 8;
    let section = assess_tokens(&inputs);
    assert_eq!(section.verdict, Verdict::Degraded);
    assert!(section.summary.contains("token-starved"));
}

#[test]
fn tokens_degraded_when_ranking_is_stale() {
    let mut inputs = healthy_inputs();
    inputs.ranking_age_secs = Some(RANKING_STALE_SECS + 1);
    let section = assess_tokens(&inputs);
    assert_eq!(section.verdict, Verdict::Degraded);
    assert!(section.summary.contains("STALE"));
}

#[test]
fn tokens_degraded_when_ranking_is_absent() {
    let mut inputs = healthy_inputs();
    inputs.ranking_present = false;
    inputs.ranking_age_secs = None;
    let section = assess_tokens(&inputs);
    assert_eq!(section.verdict, Verdict::Degraded);
    assert!(section.summary.contains("no .ranking"));
}

#[test]
fn tokens_degraded_on_an_empty_pool() {
    let mut inputs = healthy_inputs();
    let cap = &mut inputs.status.as_mut().unwrap().capacity;
    cap.total_accounts = 0;
    cap.healthy_accounts = 0;
    cap.exhausted_accounts = 0;
    let section = assess_tokens(&inputs);
    assert_eq!(section.verdict, Verdict::Degraded);
    assert!(section.summary.contains("EMPTY token pool"));
}

/// Issue #5269: the machine-level `pool_path` this section's headline
/// numbers are scoped to is always named in the detail JSON, even when
/// everything is green — so `--json` consumers never have to guess which
/// directory the healthy verdict is about.
#[test]
fn tokens_detail_names_the_evaluated_pool_path() {
    let mut inputs = healthy_inputs();
    inputs.status.as_mut().unwrap().token_pool_dir =
        Some(PathBuf::from("/repos/anvil/.loom/tokens"));
    let section = assess_tokens(&inputs);
    assert_eq!(section.detail["pool_path"], serde_json::json!("/repos/anvil/.loom/tokens"));
}

/// Issue #5269 (the reported scenario): the top-level `ranking_present`/
/// `ranking_age_secs`/verdict cover only the daemon's single
/// `fallback_root`-anchored pool — which can be fresh (or even absent, on
/// a machine-level daemon with no primary-workspace pool of its own)
/// while a DIFFERENT registered repo's own pool is stale. The `per_repo`
/// detail must still surface that repo's own staleness, and the overall
/// verdict must degrade for it, even though the top-level ranking inputs
/// alone would report Green.
#[test]
fn tokens_degraded_when_a_registered_repos_own_ranking_is_stale_even_if_the_anchored_pool_is_fresh()
{
    let mut inputs = healthy_inputs();
    // Top-level (anchored) pool: fresh — would be Green on its own.
    assert!(inputs.ranking_present);
    assert!(inputs.ranking_age_secs.unwrap() < RANKING_STALE_SECS);

    let status = inputs.status.as_mut().unwrap();
    status.per_repo = vec![
        crate::types::RepoStatus {
            root: PathBuf::from("/repos/loom"),
            priority: crate::workspace_registry::default_priority(),
            in_flight_count: 0,
            health_gate_halted: false,
            quarantined_issues: vec![],
            health_gate_not_evaluated: false,
            health_gate_not_evaluated_reason: None,
            health_gate_enabled: None,
            health_gate_verdict_at: None,
            root_missing: false,
            health_gate_deferred: false,
            health_gate_deferred_reason: None,
            health_gate_verdict_tier: None,
            role_runner_enabled: false,
            role_runner_roles: vec![],
            role_runner_intervals: std::collections::BTreeMap::new(),
            role_runner_on_idle_roles: vec![],
            role_runner_on_idle_promotions: vec![],
            role_runner_env_override: None,
            role_runner_shard: None,
            // This repo's OWN pool: present but stale.
            token_pool_dir: Some(PathBuf::from("/repos/loom/.loom/tokens")),
            ranking_present: true,
            ranking_age_secs: Some(RANKING_STALE_SECS + 3600),
            stash_total_count: 0,
            stash_quarantine_count: 0,
            stash_oldest_age_secs: None,
            stash_non_quarantine_unrecoverable_count: 0,
            stash_non_quarantine_unrecoverable_oldest_age_secs: None,
            sweep_command_missing: false,
        },
        crate::types::RepoStatus {
            root: PathBuf::from("/repos/anvil"),
            priority: crate::workspace_registry::default_priority(),
            in_flight_count: 0,
            health_gate_halted: false,
            quarantined_issues: vec![],
            health_gate_not_evaluated: false,
            health_gate_not_evaluated_reason: None,
            health_gate_enabled: None,
            health_gate_verdict_at: None,
            root_missing: false,
            health_gate_deferred: false,
            health_gate_deferred_reason: None,
            health_gate_verdict_tier: None,
            role_runner_enabled: false,
            role_runner_roles: vec![],
            role_runner_intervals: std::collections::BTreeMap::new(),
            role_runner_on_idle_roles: vec![],
            role_runner_on_idle_promotions: vec![],
            role_runner_env_override: None,
            role_runner_shard: None,
            // This repo's OWN pool: fresh.
            token_pool_dir: Some(PathBuf::from("/repos/anvil/.loom/tokens")),
            ranking_present: true,
            ranking_age_secs: Some(30),
            stash_total_count: 0,
            stash_quarantine_count: 0,
            stash_oldest_age_secs: None,
            stash_non_quarantine_unrecoverable_count: 0,
            stash_non_quarantine_unrecoverable_oldest_age_secs: None,
            sweep_command_missing: false,
        },
    ];

    let section = assess_tokens(&inputs);
    assert_eq!(
        section.verdict,
        Verdict::Degraded,
        "a stale per-repo ranking must degrade the section even though the \
             anchored top-level pool is fresh"
    );
    assert!(
        section.summary.contains("1 of 2 registered repo"),
        "summary should name the affected count, got: {}",
        section.summary
    );
    let per_repo = section.detail["per_repo"]
        .as_array()
        .expect("per_repo detail is an array");
    assert_eq!(per_repo.len(), 2);
    let loom_entry = per_repo
        .iter()
        .find(|r| r["root"] == "/repos/loom")
        .expect("loom repo entry present");
    assert_eq!(loom_entry["stale"], true);
    assert_eq!(loom_entry["pool_path"], "/repos/loom/.loom/tokens");
    let anvil_entry = per_repo
        .iter()
        .find(|r| r["root"] == "/repos/anvil")
        .expect("anvil repo entry present");
    assert_eq!(anvil_entry["stale"], false);
}

/// A registered repo with no `.ranking` of its own (never bootstrapped)
/// also counts as "affected" — distinct from, but reported alongside, the
/// stale-age case above.
#[test]
fn tokens_per_repo_missing_ranking_counts_as_affected() {
    let mut inputs = healthy_inputs();
    let status = inputs.status.as_mut().unwrap();
    status.per_repo = vec![crate::types::RepoStatus {
        root: PathBuf::from("/repos/never-bootstrapped"),
        priority: crate::workspace_registry::default_priority(),
        in_flight_count: 0,
        health_gate_halted: false,
        quarantined_issues: vec![],
        health_gate_not_evaluated: false,
        health_gate_not_evaluated_reason: None,
        health_gate_enabled: None,
        health_gate_verdict_at: None,
        root_missing: false,
        health_gate_deferred: false,
        health_gate_deferred_reason: None,
        health_gate_verdict_tier: None,
        role_runner_enabled: false,
        role_runner_roles: vec![],
        role_runner_intervals: std::collections::BTreeMap::new(),
        role_runner_on_idle_roles: vec![],
        role_runner_on_idle_promotions: vec![],
        role_runner_env_override: None,
        role_runner_shard: None,
        token_pool_dir: Some(PathBuf::from("/repos/never-bootstrapped/.loom/tokens")),
        ranking_present: false,
        ranking_age_secs: None,
        stash_total_count: 0,
        stash_quarantine_count: 0,
        stash_oldest_age_secs: None,
        stash_non_quarantine_unrecoverable_count: 0,
        stash_non_quarantine_unrecoverable_oldest_age_secs: None,
        sweep_command_missing: false,
    }];

    let section = assess_tokens(&inputs);
    assert_eq!(section.verdict, Verdict::Degraded);
    assert!(section.summary.contains("1 of 1 registered repo"));
    let entry = &section.detail["per_repo"][0];
    assert_eq!(entry["ranking_present"], false);
    assert_eq!(entry["stale"], false, "absent is reported distinctly from stale");
}

/// An empty `per_repo` (single-workspace daemon, no registry) must not
/// introduce a spurious per-repo note — byte-for-byte the pre-#5269
/// summary/verdict when nothing is registered.
#[test]
fn tokens_no_per_repo_note_when_registry_is_empty() {
    let inputs = healthy_inputs();
    assert!(inputs.status.as_ref().unwrap().per_repo.is_empty());
    let section = assess_tokens(&inputs);
    assert_eq!(section.verdict, Verdict::Green);
    assert!(!section.summary.contains("registered repo"));
    assert_eq!(section.detail["per_repo"], serde_json::json!([]));
}

// ===================================================================
// Role-tick classification
// ===================================================================

fn record(role: &str, root: &str, ago_secs: i64, ok: bool) -> RoleTickRecord {
    RoleTickRecord {
        root: PathBuf::from(root),
        role: role.to_string(),
        at: now() - chrono::Duration::seconds(ago_secs),
        ok,
        detail: (!ok).then(|| "boom".to_string()),
        pool_exhausted: false,
    }
}

#[test]
fn a_failure_followed_by_a_success_is_transient() {
    let records = vec![
        record("curator", "/r/loom", 600, false),
        record("curator", "/r/loom", 300, true),
    ];
    let summary = summarize_role_ticks(&records, now() - chrono::Duration::seconds(1800));
    assert!(summary.persistent.is_empty());
    assert_eq!(summary.transient.len(), 1);
    assert_eq!(summary.transient[0].failures, 1);
}

#[test]
fn a_failure_that_is_still_the_latest_record_is_persistent() {
    let records = vec![
        record("champion", "/r/loom", 600, true),
        record("champion", "/r/loom", 300, false),
        record("champion", "/r/loom", 60, false),
    ];
    let summary = summarize_role_ticks(&records, now() - chrono::Duration::seconds(1800));
    assert_eq!(summary.persistent.len(), 1);
    assert_eq!(summary.persistent[0].failures, 2);
    assert_eq!(summary.persistent[0].detail.as_deref(), Some("boom"));
    assert!(summary.transient.is_empty());
}

#[test]
fn each_root_role_pair_is_classified_independently() {
    let records = vec![
        record("curator", "/r/loom", 300, false),
        record("curator", "/r/anvil", 300, false),
        record("curator", "/r/anvil", 100, true),
    ];
    let summary = summarize_role_ticks(&records, now() - chrono::Duration::seconds(1800));
    assert_eq!(summary.persistent.len(), 1);
    assert_eq!(summary.persistent[0].root, PathBuf::from("/r/loom"));
    assert_eq!(summary.transient.len(), 1);
    assert_eq!(summary.transient[0].root, PathBuf::from("/r/anvil"));
}

#[test]
fn records_outside_the_window_are_ignored() {
    let records = vec![record("guide", "/r/loom", 7200, false)];
    let summary = summarize_role_ticks(&records, now() - chrono::Duration::seconds(1800));
    assert_eq!(summary.total, 0);
    assert!(summary.persistent.is_empty());
}

#[test]
fn only_persistent_failures_make_the_roles_section_degraded() {
    let mut inputs = healthy_inputs();
    inputs.status.as_mut().unwrap().role_tick_records = vec![
        record("curator", "/r/loom", 600, false),
        record("curator", "/r/loom", 300, true),
    ];
    let section = assess_roles(&inputs);
    assert_eq!(section.verdict, Verdict::Green);
    assert!(section.summary.contains("1 transient (self-recovered)"));

    inputs.status.as_mut().unwrap().role_tick_records =
        vec![record("curator", "/r/loom", 60, false)];
    let section = assess_roles(&inputs);
    assert_eq!(section.verdict, Verdict::Degraded);
    assert!(section.summary.contains("PERSISTENT"));
    assert!(section.summary.contains("curator @ loom"));
}

#[test]
fn no_role_ticks_in_window_is_green() {
    let section = assess_roles(&healthy_inputs());
    assert_eq!(section.verdict, Verdict::Green);
    assert!(section.summary.contains("no role ticks in window"));
}

// ------- pool-exhausted classification (#7607) -------

fn record_pool_exhausted(role: &str, root: &str, ago_secs: i64) -> RoleTickRecord {
    RoleTickRecord {
        root: PathBuf::from(root),
        role: role.to_string(),
        at: now() - chrono::Duration::seconds(ago_secs),
        ok: false,
        detail: Some("pool-exhausted: 0/3 spawnable, next check ~soon".to_string()),
        pool_exhausted: true,
    }
}

#[test]
fn a_pool_exhausted_tick_is_routed_to_its_own_bucket_not_persistent() {
    let records = vec![record_pool_exhausted("champion", "/r/loom", 60)];
    let summary = summarize_role_ticks(&records, now() - chrono::Duration::seconds(1800));
    assert!(summary.persistent.is_empty(), "must never land in `persistent`");
    assert!(summary.transient.is_empty());
    assert!(summary.escalated.is_empty(), "must never escalate");
    assert_eq!(summary.pool_exhausted.len(), 1);
    assert_eq!(summary.pool_exhausted[0].root, PathBuf::from("/r/loom"));
}

#[test]
fn many_identical_pool_exhausted_ticks_never_escalate() {
    // Mirrors the incident: hundreds of identical exit-78 skips across a
    // window must never build an escalation streak the way a genuine
    // config-shaped failure would.
    let records: Vec<RoleTickRecord> = (0..20)
        .map(|i| record_pool_exhausted("champion", "/r/loom", i * 10))
        .collect();
    let summary = summarize_role_ticks(&records, now() - chrono::Duration::seconds(1800));
    assert_eq!(summary.pool_exhausted.len(), 1);
    assert!(summary.escalated.is_empty());
    assert!(summary.persistent.is_empty());
}

#[test]
fn assess_roles_distinguishes_pool_exhausted_from_persistent_failures() {
    let mut inputs = healthy_inputs();

    // Pool exhaustion alone: degraded (real, actionable), but the
    // summary must say "pool exhausted", never "PERSISTENT failure(s)".
    inputs.status.as_mut().unwrap().role_tick_records =
        vec![record_pool_exhausted("champion", "/r/loom", 60)];
    let section = assess_roles(&inputs);
    assert_eq!(section.verdict, Verdict::Degraded);
    assert!(
        section.summary.contains("pool exhausted (1 role(s) held)"),
        "{}",
        section.summary
    );
    assert!(!section.summary.contains("PERSISTENT"), "{}", section.summary);

    // A genuine failure alongside a pool-exhausted skip: both call-outs
    // appear, and the PERSISTENT count reflects only the real failure.
    inputs.status.as_mut().unwrap().role_tick_records = vec![
        record("curator", "/r/loom", 60, false),
        record_pool_exhausted("champion", "/r/loom", 60),
    ];
    let section = assess_roles(&inputs);
    assert_eq!(section.verdict, Verdict::Degraded);
    assert!(section.summary.contains("1 PERSISTENT failure(s)"), "{}", section.summary);
    assert!(
        section.summary.contains("pool exhausted (1 role(s) held)"),
        "{}",
        section.summary
    );
}

// ------- Escalation on N consecutive identical failures (#5023) -------

/// A failure record with an explicit `detail`, for the identical-vs-varying
/// escalation tests (#5023) and the summary-bounding tests (#5024) — the
/// plain `record` helper always uses `"boom"`.
fn record_detail(role: &str, root: &str, ago_secs: i64, detail: &str) -> RoleTickRecord {
    RoleTickRecord {
        root: PathBuf::from(root),
        role: role.to_string(),
        at: now() - chrono::Duration::seconds(ago_secs),
        ok: false,
        detail: Some(detail.to_string()),
        pool_exhausted: false,
    }
}

#[test]
fn n_consecutive_identical_failures_escalate() {
    // Exactly ROLE_TICK_ESCALATION_THRESHOLD identical failures, oldest
    // first — the config-shaped case the 2026-08-03 outage produced.
    let n = ROLE_TICK_ESCALATION_THRESHOLD;
    let records: Vec<RoleTickRecord> = (0..n)
        .map(|i| record("judge", "/r/loom", ((n - i) * 10) as i64, false))
        .collect();
    let summary = summarize_role_ticks(&records, now() - chrono::Duration::seconds(1800));
    assert_eq!(summary.persistent.len(), 1);
    assert_eq!(summary.escalated.len(), 1, "N identical failures must escalate");
    assert_eq!(summary.escalated[0].consecutive_identical, n);
    assert_eq!(summary.escalated[0].detail.as_deref(), Some("boom"));
}

#[test]
fn n_minus_one_failures_interspersed_with_a_success_do_not_escalate() {
    // (N-1) identical failures, one success, then (N-1) identical failures:
    // the latest record is a failure (persistent) but the trailing run of
    // *consecutive* identical failures is only N-1 — the success in the
    // middle reset the streak, so escalation must NOT trigger.
    let n = ROLE_TICK_ESCALATION_THRESHOLD;
    let mut records = Vec::new();
    let mut ago = ((2 * n) * 10) as i64;
    for _ in 0..(n - 1) {
        records.push(record("judge", "/r/loom", ago, false));
        ago -= 10;
    }
    records.push(record("judge", "/r/loom", ago, true));
    ago -= 10;
    for _ in 0..(n - 1) {
        records.push(record("judge", "/r/loom", ago, false));
        ago -= 10;
    }
    let summary = summarize_role_ticks(&records, now() - chrono::Duration::seconds(1800));
    assert_eq!(summary.persistent.len(), 1);
    assert_eq!(summary.persistent[0].consecutive_identical, n - 1);
    assert!(
        summary.escalated.is_empty(),
        "a success within the tail must reset the streak below the threshold"
    );
}

#[test]
fn escalation_clears_once_the_tick_succeeds_again() {
    // N identical failures (would escalate) followed by a success: the pair
    // is now transient, `escalated` is empty, and the streak resets to 0 —
    // no permanent lockout from a since-fixed cause.
    let n = ROLE_TICK_ESCALATION_THRESHOLD;
    let mut records: Vec<RoleTickRecord> = (0..n)
        .map(|i| record("judge", "/r/loom", ((n + 1 - i) * 10) as i64, false))
        .collect();
    records.push(record("judge", "/r/loom", 5, true));
    let summary = summarize_role_ticks(&records, now() - chrono::Duration::seconds(1800));
    assert!(summary.escalated.is_empty());
    assert!(summary.persistent.is_empty());
    assert_eq!(summary.transient.len(), 1);
    assert_eq!(summary.transient[0].consecutive_identical, 0);
}

#[test]
fn a_different_failure_detail_breaks_the_identical_streak() {
    // N-1 identical failures then one failure with a DIFFERENT detail: the
    // trailing run of *identical* failures is only 1, so exact-detail
    // matching keeps a flapping (varying-message) failure out of escalation
    // even though the pair is persistent.
    let n = ROLE_TICK_ESCALATION_THRESHOLD;
    let mut records: Vec<RoleTickRecord> = (0..(n - 1))
        .map(|i| record("judge", "/r/loom", ((n - i) * 10) as i64, false))
        .collect();
    records.push(record_detail("judge", "/r/loom", 5, "a-different-transient-error"));
    let summary = summarize_role_ticks(&records, now() - chrono::Duration::seconds(1800));
    assert_eq!(summary.persistent.len(), 1);
    assert_eq!(summary.persistent[0].consecutive_identical, 1);
    assert!(summary.escalated.is_empty());
}

#[test]
fn escalated_failures_surface_distinctly_in_the_roles_section() {
    let n = ROLE_TICK_ESCALATION_THRESHOLD;
    let mut inputs = healthy_inputs();
    inputs.status.as_mut().unwrap().role_tick_records = (0..n)
        .map(|i| record("judge", "/r/loom", ((n - i) * 10) as i64, false))
        .collect();
    let section = assess_roles(&inputs);
    assert_eq!(section.verdict, Verdict::Degraded);
    assert!(
        section.summary.contains("ESCALATED"),
        "escalated pairs must be called out distinctly: {}",
        section.summary
    );
    assert!(section.summary.contains("judge @ loom"));
    // The escalated subset is machine-readable for --json / dashboard
    // consumers (#5022 will export it externally).
    assert_eq!(section.detail["escalated"].as_array().map(Vec::len), Some(1));
}

// -- roles.summary bounding + ANSI cleanup (#5024) ----------------------

#[test]
fn roles_summary_line_stays_bounded_for_an_oversized_ansi_laden_detail() {
    let mut inputs = healthy_inputs();
    let noisy = format!("\x1b[31m{}\x1b[0m", "x".repeat(20_000));
    inputs.status.as_mut().unwrap().role_tick_records =
        vec![record_detail("curator", "/r/loom", 60, &noisy)];

    let section = assess_roles(&inputs);

    assert_eq!(section.verdict, Verdict::Degraded);
    assert!(
        section.summary.chars().count() <= MAX_ROLES_SUMMARY_LINE_CHARS + 100,
        "summary line was not bounded: {} chars",
        section.summary.chars().count()
    );
    assert!(
        !section.summary.contains('\u{1b}'),
        "summary line still contains a raw ANSI escape byte"
    );
}

#[test]
fn roles_summary_line_does_not_multiply_with_many_simultaneous_failures() {
    // The 2026-08-03 12-repo outage: every repo fails identically with a
    // large detail. The summary line must stay bounded regardless of how
    // many `(root, role)` pairs are failing at once.
    let noisy = format!("\x1b[31m{}\x1b[0m", "boom ".repeat(2000));
    let mut inputs = healthy_inputs();
    inputs.status.as_mut().unwrap().role_tick_records = (0..12)
        .map(|i| record_detail("curator", &format!("/r/repo{i}"), 60, &noisy))
        .collect();

    let section = assess_roles(&inputs);

    assert_eq!(section.verdict, Verdict::Degraded);
    assert!(section.summary.contains("12 PERSISTENT failure(s)"));
    assert!(
        section.summary.chars().count() <= MAX_ROLES_SUMMARY_LINE_CHARS + 100,
        "summary line was not bounded across 12 failures: {} chars",
        section.summary.chars().count()
    );
}

#[test]
fn roles_structured_detail_still_carries_the_capped_cleaned_content() {
    let mut inputs = healthy_inputs();
    let noisy = format!("\x1b[31m{}\x1b[0m", "x".repeat(200));
    inputs.status.as_mut().unwrap().role_tick_records =
        vec![record_detail("curator", "/r/loom", 60, &noisy)];

    let section = assess_roles(&inputs);

    // The summary line is bounded/short...
    assert!(section.summary.chars().count() < noisy.chars().count());
    // ...but the structured `detail` field still carries the (ANSI-clean)
    // failure content — moved out of the summary line, not dropped.
    let structured_detail = section.detail["persistent"][0]["detail"]
        .as_str()
        .expect("persistent[0].detail should be a string");
    assert!(structured_detail.contains('x'));
    assert!(
        !structured_detail.contains('\u{1b}'),
        "structured detail should not carry raw ANSI escapes: {structured_detail:?}"
    );
}

#[test]
fn roles_summary_detail_round_trips_short_clean_text_unchanged() {
    let mut inputs = healthy_inputs();
    inputs.status.as_mut().unwrap().role_tick_records = vec![record_detail(
        "curator",
        "/r/loom",
        60,
        "connection refused",
    )];

    let section = assess_roles(&inputs);

    assert!(section.summary.contains("connection refused"));
}

// ===================================================================
// Role liveness (#6201) — "is this role ticking at all"
// ===================================================================

/// A minimal, fully-populated [`crate::types::RepoStatus`] for the
/// `assess_role_liveness` fixtures below — only `root`,
/// `role_runner_enabled`, and `role_runner_roles` vary per test.
/// `role_runner_intervals` is left empty (today's pre-#7238 wire-data
/// shape), so these fixtures exercise the built-in-default fallback path
/// unless a test overrides the map explicitly via
/// [`role_liveness_repo_with_intervals`].
fn role_liveness_repo(
    root: &str,
    role_runner_enabled: bool,
    role_runner_roles: Vec<&str>,
) -> crate::types::RepoStatus {
    role_liveness_repo_with_intervals(root, role_runner_enabled, role_runner_roles, &[])
}

/// [`role_liveness_repo`] plus an explicit `role_runner_intervals` map
/// (Issue #7238) — `(role, resolved_interval_secs)` pairs, for the
/// resolved-cadence fixtures below.
fn role_liveness_repo_with_intervals(
    root: &str,
    role_runner_enabled: bool,
    role_runner_roles: Vec<&str>,
    role_runner_intervals: &[(&str, u64)],
) -> crate::types::RepoStatus {
    crate::types::RepoStatus {
        root: PathBuf::from(root),
        priority: crate::workspace_registry::default_priority(),
        in_flight_count: 0,
        health_gate_halted: false,
        quarantined_issues: vec![],
        health_gate_not_evaluated: false,
        health_gate_not_evaluated_reason: None,
        health_gate_enabled: None,
        health_gate_verdict_at: None,
        root_missing: false,
        health_gate_deferred: false,
        health_gate_deferred_reason: None,
        health_gate_verdict_tier: None,
        role_runner_enabled,
        role_runner_roles: role_runner_roles.into_iter().map(str::to_string).collect(),
        role_runner_intervals: role_runner_intervals
            .iter()
            .map(|(role, secs)| ((*role).to_string(), *secs))
            .collect(),
        role_runner_on_idle_roles: vec![],
        role_runner_on_idle_promotions: vec![],
        role_runner_env_override: None,
        role_runner_shard: None,
        token_pool_dir: None,
        ranking_present: false,
        ranking_age_secs: None,
        stash_total_count: 0,
        stash_quarantine_count: 0,
        stash_oldest_age_secs: None,
        stash_non_quarantine_unrecoverable_count: 0,
        stash_non_quarantine_unrecoverable_oldest_age_secs: None,
        sweep_command_missing: false,
    }
}

#[test]
fn role_liveness_green_when_nothing_registered() {
    let section = assess_role_liveness(&healthy_inputs());
    assert_eq!(section.verdict, Verdict::Green);
    assert_eq!(section.detail["checked"], 0);
}

#[test]
fn role_liveness_unknown_when_status_missing() {
    let mut inputs = healthy_inputs();
    inputs.status = None;
    inputs.ipc_error = Some("connect failed".to_string());
    let section = assess_role_liveness(&inputs);
    assert_eq!(section.verdict, Verdict::Unknown);
}

/// The #6201 incident, reproduced directly: `curator` (300s default
/// interval) has a real last-tick record from nine days ago on a root
/// where the role runner is enabled and `curator` is a resolved role —
/// this must surface as `Degraded`, not the false-`Green` the incident's
/// windowed-ring-only `roles` section produced.
/// A fully-populated [`crate::types::RoleLastTick`] fixture for the
/// `assess_role_liveness` tests below — `ok`/`detail`/
/// `consecutive_identical_failures` default to a clean successful tick;
/// override them for the #6239 "stuck, not silent" fixtures.
fn role_last_tick(
    root: &str,
    role: &str,
    at: DateTime<Utc>,
    ok: bool,
    detail: Option<&str>,
    consecutive_identical_failures: usize,
) -> crate::types::RoleLastTick {
    crate::types::RoleLastTick {
        root: PathBuf::from(root),
        role: role.to_string(),
        at,
        ok,
        detail: detail.map(str::to_string),
        consecutive_identical_failures,
    }
}

#[test]
fn role_liveness_flags_a_role_silent_for_far_beyond_its_interval() {
    let mut inputs = healthy_inputs();
    let status = inputs.status.as_mut().unwrap();
    status.per_repo = vec![role_liveness_repo("/repos/loom", true, vec!["curator"])];
    status.role_last_tick = vec![role_last_tick(
        "/repos/loom",
        "curator",
        inputs.at - chrono::Duration::days(9),
        true,
        None,
        0,
    )];

    let section = assess_role_liveness(&inputs);
    assert_eq!(section.verdict, Verdict::Degraded);
    assert!(section.summary.contains("curator"), "{}", section.summary);
    assert!(section.summary.contains("SILENT"), "{}", section.summary);
    let stale = &section.detail["stale"][0];
    assert_eq!(stale["role"], "curator");
    assert_eq!(stale["expected_interval_secs"], 300);
}

/// A role that has NEVER ticked at all (no [`crate::types::RoleLastTick`]
/// entry) is deliberately not flagged — it may simply have been enabled
/// moments ago; there is nothing to compare a silence duration against.
#[test]
fn role_liveness_never_ticked_is_not_flagged() {
    let mut inputs = healthy_inputs();
    let status = inputs.status.as_mut().unwrap();
    status.per_repo = vec![role_liveness_repo("/repos/loom", true, vec!["curator"])];
    status.role_last_tick = vec![];

    let section = assess_role_liveness(&inputs);
    assert_eq!(section.verdict, Verdict::Green);
}

/// A recent tick, well inside `curator`'s `300s * 4x` threshold, is Green
/// — ordinary cadence, not staleness.
#[test]
fn role_liveness_recent_tick_is_green() {
    let mut inputs = healthy_inputs();
    let status = inputs.status.as_mut().unwrap();
    status.per_repo = vec![role_liveness_repo("/repos/loom", true, vec!["curator"])];
    status.role_last_tick = vec![role_last_tick(
        "/repos/loom",
        "curator",
        inputs.at - chrono::Duration::minutes(2),
        true,
        None,
        0,
    )];

    let section = assess_role_liveness(&inputs);
    assert_eq!(section.verdict, Verdict::Green);
    assert_eq!(section.detail["checked"], 1);
}

/// Issue #7238: a fleet that has deliberately slowed its role-runner
/// cadence (here to 1800s, well above `curator`'s 300s built-in default)
/// must not have a healthy role falsely flagged SILENT just because its
/// last tick is beyond `4x` the UN-configured built-in default — it must
/// be compared against the actually-resolved 1800s interval instead.
#[test]
fn role_liveness_green_under_a_slowed_resolved_cadence_beyond_the_built_in_threshold() {
    let mut inputs = healthy_inputs();
    let status = inputs.status.as_mut().unwrap();
    status.per_repo = vec![role_liveness_repo_with_intervals(
        "/repos/loom",
        true,
        vec!["curator"],
        &[("curator", 1800)],
    )];
    // Beyond `4 * 300s` (the built-in threshold this issue was filed
    // against) but well within `4 * 1800s` (the resolved threshold).
    status.role_last_tick = vec![role_last_tick(
        "/repos/loom",
        "curator",
        inputs.at - chrono::Duration::seconds(1500),
        true,
        None,
        0,
    )];

    let section = assess_role_liveness(&inputs);
    assert_eq!(section.verdict, Verdict::Green, "{}", section.summary);
    assert_eq!(section.detail["checked"], 1);
    assert!(section.detail["stale"].as_array().unwrap().is_empty());
}

/// Issue #7238: the multiplier still applies to the RESOLVED interval —
/// a role genuinely silent beyond `4x` its resolved (not built-in)
/// interval must still report Degraded, so a slowed cadence does not
/// mask a real outage.
#[test]
fn role_liveness_still_flags_stale_beyond_4x_the_resolved_interval() {
    let mut inputs = healthy_inputs();
    let status = inputs.status.as_mut().unwrap();
    status.per_repo = vec![role_liveness_repo_with_intervals(
        "/repos/loom",
        true,
        vec!["curator"],
        &[("curator", 1800)],
    )];
    // Beyond `4 * 1800s = 7200s`.
    status.role_last_tick = vec![role_last_tick(
        "/repos/loom",
        "curator",
        inputs.at - chrono::Duration::seconds(7300),
        true,
        None,
        0,
    )];

    let section = assess_role_liveness(&inputs);
    assert_eq!(section.verdict, Verdict::Degraded, "{}", section.summary);
    assert!(section.summary.contains("SILENT"), "{}", section.summary);
    let stale = &section.detail["stale"][0];
    assert_eq!(stale["role"], "curator");
    assert_eq!(stale["expected_interval_secs"], 1800);
}

/// Issue #7238 backward compatibility: pre-#7238 wire data (an
/// absent/empty `role_runner_intervals` map — [`role_liveness_repo`]'s
/// default) must fall back to the role's built-in default exactly like
/// before this issue's fix, preserving today's behavior for a daemon
/// that has not yet been upgraded to populate the new field.
#[test]
fn role_liveness_falls_back_to_built_in_default_when_intervals_map_is_empty() {
    let mut inputs = healthy_inputs();
    let status = inputs.status.as_mut().unwrap();
    status.per_repo = vec![role_liveness_repo("/repos/loom", true, vec!["curator"])];
    // Beyond `4 * 300s` (curator's built-in default) — must still flag,
    // exactly as it did before #7238.
    status.role_last_tick = vec![role_last_tick(
        "/repos/loom",
        "curator",
        inputs.at - chrono::Duration::seconds(1300),
        true,
        None,
        0,
    )];

    let section = assess_role_liveness(&inputs);
    assert_eq!(section.verdict, Verdict::Degraded, "{}", section.summary);
    let stale = &section.detail["stale"][0];
    assert_eq!(stale["expected_interval_secs"], 300);
}

/// A root with the role runner disabled is not checked at all, even if
/// it carries a stale-looking last-tick record from before it was
/// disabled — an operator's deliberate disable is not a silent failure.
#[test]
fn role_liveness_disabled_root_is_not_checked() {
    let mut inputs = healthy_inputs();
    let status = inputs.status.as_mut().unwrap();
    status.per_repo = vec![role_liveness_repo("/repos/loom", false, vec!["curator"])];
    status.role_last_tick = vec![role_last_tick(
        "/repos/loom",
        "curator",
        inputs.at - chrono::Duration::days(9),
        true,
        None,
        0,
    )];

    let section = assess_role_liveness(&inputs);
    assert_eq!(section.verdict, Verdict::Green);
    assert_eq!(section.detail["checked"], 0);
}

// ------- Stuck (ticking, but never actually running) roles (#6239) ----

/// The #6239 incident, reproduced directly: `curator` ticks every
/// interval (so it is nowhere near `stale`) but its last
/// `ROLE_TICK_ESCALATION_THRESHOLD` consecutive ticks all bailed out
/// pre-spawn with a byte-identical `ModelRuntimeMismatch` detail. This
/// must surface as `Degraded` and STUCK, not the false-`Green` a
/// staleness-only check produces.
#[test]
fn role_liveness_flags_a_role_stuck_on_an_identical_pre_spawn_skip() {
    let mut inputs = healthy_inputs();
    let status = inputs.status.as_mut().unwrap();
    status.per_repo = vec![role_liveness_repo("/repos/loom", true, vec!["curator"])];
    status.role_last_tick = vec![role_last_tick(
        "/repos/loom",
        "curator",
        inputs.at - chrono::Duration::seconds(30),
        false,
        Some("model/runtime mismatch: runtime \"codex\" only accepts Codex models"),
        ROLE_TICK_ESCALATION_THRESHOLD,
    )];

    let section = assess_role_liveness(&inputs);
    assert_eq!(section.verdict, Verdict::Degraded);
    assert!(section.summary.contains("STUCK"), "{}", section.summary);
    assert!(section.summary.contains("curator @ loom"), "{}", section.summary);
    assert!(section.summary.contains("model/runtime mismatch"), "{}", section.summary);
    let stuck = &section.detail["stuck"][0];
    assert_eq!(stuck["role"], "curator");
    assert_eq!(stuck["consecutive_identical_failures"], ROLE_TICK_ESCALATION_THRESHOLD);
    assert!(section.detail["stale"].as_array().unwrap().is_empty());
}

/// A failing pair whose consecutive-identical streak has NOT yet reached
/// the threshold is ordinary tick noise, not a config-shaped lockup —
/// must stay Green here (the windowed `roles` section still reports it
/// as an ordinary persistent failure).
#[test]
fn role_liveness_below_threshold_failures_are_not_stuck() {
    let mut inputs = healthy_inputs();
    let status = inputs.status.as_mut().unwrap();
    status.per_repo = vec![role_liveness_repo("/repos/loom", true, vec!["curator"])];
    status.role_last_tick = vec![role_last_tick(
        "/repos/loom",
        "curator",
        inputs.at - chrono::Duration::seconds(30),
        false,
        Some("no-token-pool"),
        ROLE_TICK_ESCALATION_THRESHOLD - 1,
    )];

    let section = assess_role_liveness(&inputs);
    assert_eq!(section.verdict, Verdict::Green);
}

/// A pair that is BOTH silent beyond its interval AND carries a stale
/// failure streak is reported only as stale — silence is the stronger,
/// more informative verdict, and double-reporting the same pair under
/// two labels would be confusing rather than additive.
#[test]
fn role_liveness_stale_takes_precedence_over_stuck_for_the_same_pair() {
    let mut inputs = healthy_inputs();
    let status = inputs.status.as_mut().unwrap();
    status.per_repo = vec![role_liveness_repo("/repos/loom", true, vec!["curator"])];
    status.role_last_tick = vec![role_last_tick(
        "/repos/loom",
        "curator",
        inputs.at - chrono::Duration::days(9),
        false,
        Some("no-token-pool"),
        ROLE_TICK_ESCALATION_THRESHOLD,
    )];

    let section = assess_role_liveness(&inputs);
    assert_eq!(section.verdict, Verdict::Degraded);
    assert!(section.summary.contains("SILENT"), "{}", section.summary);
    assert!(!section.summary.contains("STUCK"), "{}", section.summary);
    assert_eq!(section.detail["stale"].as_array().unwrap().len(), 1);
    assert!(section.detail["stuck"].as_array().unwrap().is_empty());
}

/// Regression for #6239 AC3: even when the shared, capacity-bounded tick
/// ring is fully saturated by OTHER `(root, role)` pairs' ticks — evicting
/// every trace that the target pair ever ran from
/// [`crate::role_runner::role_tick_records`] — the never-evicted
/// `role_last_tick` companion this section reads still carries the
/// target's outcome/streak, so it still reports STUCK.
#[test]
#[serial(role_tick_ring)]
fn role_liveness_reports_stuck_even_when_the_ring_is_saturated_by_other_roots() {
    crate::role_runner::reset_role_tick_ring_for_tests();
    let root = std::path::Path::new("/repos/loom");
    // The target pair fails identically `ROLE_TICK_ESCALATION_THRESHOLD`
    // times FIRST — recorded into both the ring and the never-evicted
    // `role_last_tick` map.
    for _ in 0..ROLE_TICK_ESCALATION_THRESHOLD {
        crate::role_runner::record_role_tick(
            "curator",
            root,
            &crate::role_runner::RoleTickOutcome::NoTokenPool,
        );
    }
    // Then saturate the ring with a different pair's successes —
    // comfortably more than `ROLE_TICK_RING_CAPACITY` so every curator
    // entry above (the oldest in the ring) is evicted.
    for _ in 0..(crate::role_runner::ROLE_TICK_RING_CAPACITY + 50) {
        crate::role_runner::record_role_tick(
            "champion",
            root,
            &crate::role_runner::RoleTickOutcome::Success,
        );
    }

    // Confirm the premise: the bounded ring holds NO curator@loom
    // records at all (fully evicted by the saturating champion ticks).
    let ring_records = crate::role_runner::role_tick_records();
    assert!(
        ring_records.iter().all(|r| r.role != "curator"),
        "the saturating loop must have fully evicted curator's ring records"
    );

    let mut inputs = healthy_inputs();
    inputs.at = Utc::now();
    {
        let status = inputs.status.as_mut().unwrap();
        status.per_repo = vec![role_liveness_repo("/repos/loom", true, vec!["curator"])];
        status.role_last_tick = crate::role_runner::last_role_tick_snapshot();
        status.role_tick_records = ring_records;
    }

    let section = assess_role_liveness(&inputs);
    assert_eq!(section.verdict, Verdict::Degraded, "{}", section.summary);
    assert!(section.summary.contains("STUCK"), "{}", section.summary);
    assert!(section.summary.contains("curator @ loom"), "{}", section.summary);

    // The windowed `roles` section, sourced from the saturated ring
    // alone, is exactly the false-green this issue was filed for.
    let roles_section = assess_roles(&inputs);
    assert!(
        !roles_section.summary.contains("curator"),
        "demonstrates the bounded ring alone cannot see the evicted pair: {}",
        roles_section.summary
    );

    crate::role_runner::reset_role_tick_ring_for_tests();
}

// ===================================================================
// Queues + throughput
// ===================================================================

#[test]
fn queues_sum_ready_counts_per_repo() {
    let mut inputs = healthy_inputs();
    inputs.pipeline = Some(vec![
        RepoPipelineSnapshot {
            root: PathBuf::from("/r/loom"),
            queued: Some(4),
            merged_24h: Some(1),
            ..Default::default()
        },
        RepoPipelineSnapshot {
            root: PathBuf::from("/r/anvil"),
            queued: Some(2),
            merged_24h: Some(0),
            ..Default::default()
        },
    ]);
    let section = assess_queues(&inputs);
    assert_eq!(section.verdict, Verdict::Green);
    assert!(section.summary.contains("6 ready across 2 repo(s)"));
    assert!(section.summary.contains("loom 4"));
    assert!(section.summary.contains("anvil 2"));
}

#[test]
fn a_failed_queue_query_is_unknown_not_green() {
    let mut inputs = healthy_inputs();
    inputs.pipeline = Some(vec![RepoPipelineSnapshot {
        root: PathBuf::from("/r/loom"),
        queued: None,
        merged_24h: Some(1),
        error: Some("rate limited".to_string()),
        ..Default::default()
    }]);
    let section = assess_queues(&inputs);
    assert_eq!(section.verdict, Verdict::Unknown);
    assert_eq!(assess(&inputs).exit_code(), EXIT_DEGRADED);
}

// ===================================================================
// A missing/non-executable `gh` is one fact, not N per-repo failures
// (#5061)
// ===================================================================

fn gh_unavailable_fixture() -> crate::pipeline_snapshot::GhUnavailable {
    crate::pipeline_snapshot::GhUnavailable {
        gh_bin: "gh".to_string(),
        reason: "`gh` not found on PATH — cannot assess queue depth / merge throughput. \
                     This looks like a non-login shell missing PATH entries a login shell would \
                     add — the same failure class as #4875."
            .to_string(),
        observed_path: Some("/usr/bin:/bin".to_string()),
    }
}

fn credential_preflight_ok() -> crate::types::CredentialPreflightReport {
    crate::types::CredentialPreflightReport {
        ok: true,
        mechanism: "keyring".to_string(),
        fingerprint: Some("rjwalters".to_string()),
        message: "authenticated as rjwalters via keyring".to_string(),
        checked_at: now(),
    }
}

/// The core #5061 regression pin: a missing `gh` collapses to a single
/// section-level reason naming PATH, never a per-repo "forge query
/// FAILED for: repoA, repoB, ..." list — even though `pipeline` itself is
/// `None` (the collector never ran the fan-out).
#[test]
fn a_missing_gh_is_one_fact_not_a_per_repo_failure_list() {
    let mut inputs = healthy_inputs();
    inputs.pipeline = None;
    inputs.gh_unavailable = Some(gh_unavailable_fixture());

    let queues = assess_queues(&inputs);
    assert_eq!(queues.verdict, Verdict::Unknown);
    assert!(queues.summary.contains("PATH"), "{}", queues.summary);
    assert!(queues.summary.contains("#4875"), "{}", queues.summary);
    assert!(
        !queues.summary.contains("forge query FAILED for:"),
        "must not regress to the per-repo phrasing: {}",
        queues.summary
    );

    let throughput = assess_throughput(&inputs);
    assert_eq!(throughput.verdict, Verdict::Unknown);
    assert!(
        !throughput.summary.contains("forge query FAILED for:"),
        "must not regress to the per-repo phrasing: {}",
        throughput.summary
    );

    // exit-code contract is unchanged: still "could not determine", exit 1.
    assert_eq!(assess(&inputs).exit_code(), EXIT_DEGRADED);
}

/// AC: cross-reference the daemon's own IPC-answered credential verdict
/// so the two signals never silently contradict each other.
#[test]
fn a_missing_gh_cross_references_a_healthy_daemon_credential() {
    let mut inputs = healthy_inputs();
    inputs.pipeline = None;
    inputs.gh_unavailable = Some(gh_unavailable_fixture());
    inputs.status.as_mut().unwrap().credential_preflight = Some(credential_preflight_ok());

    let queues = assess_queues(&inputs);
    assert!(
        queues.summary.contains("credential OK"),
        "should cross-reference the daemon's own OK verdict: {}",
        queues.summary
    );
    assert!(queues.detail["daemon_credential_preflight"]["ok"] == true);
}

/// Without a daemon-reported credential verdict (unreachable IPC, or a
/// pre-#4005 daemon), the section must still render — no cross-reference
/// clause, not a panic or an empty summary.
#[test]
fn a_missing_gh_with_no_daemon_credential_signal_still_renders() {
    let mut inputs = healthy_inputs();
    inputs.pipeline = None;
    inputs.gh_unavailable = Some(gh_unavailable_fixture());
    inputs.status = None;

    let queues = assess_queues(&inputs);
    assert_eq!(queues.verdict, Verdict::Unknown);
    assert!(!queues.summary.is_empty());
    assert!(queues.detail["daemon_credential_preflight"].is_null());
}

// ===================================================================
// Queues: the review-side axes (#5021)
// ===================================================================

/// One repo's snapshot with every axis observed — the fixture the review
/// tests vary a single field of.
fn review_snapshot(
    name: &str,
    queued: usize,
    review_requested: usize,
    merged: usize,
) -> RepoPipelineSnapshot {
    RepoPipelineSnapshot {
        root: PathBuf::from(format!("/r/{name}")),
        queued: Some(queued),
        review_requested: Some(review_requested),
        changes_requested: Some(0),
        approved: Some(0),
        merged_24h: Some(merged),
        ..Default::default()
    }
}

/// AC1: the section reports the three review-side axes, not just `queued`.
#[test]
fn queues_report_the_review_side_axes_per_repo() {
    let mut inputs = healthy_inputs();
    inputs.pipeline = Some(vec![RepoPipelineSnapshot {
        root: PathBuf::from("/r/loom"),
        queued: Some(1),
        review_requested: Some(2),
        changes_requested: Some(1),
        changes_requested_unclaimed: Some(1),
        approved: Some(3),
        merged_24h: Some(5),
        ..Default::default()
    }]);
    let section = assess_queues(&inputs);

    assert_eq!(section.verdict, Verdict::Green);
    assert!(section.summary.contains("2 awaiting review"), "{}", section.summary);
    assert!(section.summary.contains("1 changes-requested"), "{}", section.summary);
    assert!(section.summary.contains("1 changes-requested-no-owner"), "{}", section.summary);
    assert!(section.summary.contains("3 approved"), "{}", section.summary);

    assert_eq!(section.detail["total_review_requested"], serde_json::json!(2));
    assert_eq!(section.detail["total_changes_requested"], serde_json::json!(1));
    assert_eq!(section.detail["total_changes_requested_unclaimed"], serde_json::json!(1));
    assert_eq!(section.detail["total_approved"], serde_json::json!(3));
    let repo = &section.detail["repos"][0];
    assert_eq!(repo["review_requested"], serde_json::json!(2));
    assert_eq!(repo["changes_requested"], serde_json::json!(1));
    assert_eq!(repo["changes_requested_unclaimed"], serde_json::json!(1));
    assert_eq!(repo["approved"], serde_json::json!(3));
    assert_eq!(repo["review_stalled"], serde_json::json!(false));
}

/// AC4 of #5272: the no-owner subset of `changes_requested` is counted
/// distinctly from the queue depth itself — a repo with a healthy Doctor
/// (every changes-requested PR claimed) must render `0
/// changes-requested-no-owner` even while `changes_requested` itself is
/// nonzero, so a *regression* (this climbing off zero) is visible without
/// being masked by the raw queue-depth axis staying flat.
#[test]
fn changes_requested_unclaimed_is_tracked_distinctly_from_the_raw_queue_depth() {
    let mut inputs = healthy_inputs();
    inputs.pipeline = Some(vec![RepoPipelineSnapshot {
        root: PathBuf::from("/r/loom"),
        queued: Some(0),
        review_requested: Some(0),
        changes_requested: Some(3),
        changes_requested_unclaimed: Some(0),
        approved: Some(0),
        merged_24h: Some(0),
        ..Default::default()
    }]);
    let section = assess_queues(&inputs);

    assert!(section.summary.contains("3 changes-requested"), "{}", section.summary);
    assert!(section.summary.contains("0 changes-requested-no-owner"), "{}", section.summary);
    assert_eq!(section.detail["total_changes_requested"], serde_json::json!(3));
    assert_eq!(section.detail["total_changes_requested_unclaimed"], serde_json::json!(0));
}

/// AC2: a review backlog against a window that merged nothing is non-green
/// — the 2026-08-03 Judge-outage shape, reported without knowing the cause.
#[test]
fn a_review_backlog_with_no_merges_is_degraded() {
    let mut inputs = healthy_inputs();
    inputs.pipeline = Some(vec![review_snapshot("loom", 1, REVIEW_STALL_MIN_BACKLOG, 0)]);
    let section = assess_queues(&inputs);

    assert_eq!(section.verdict, Verdict::Degraded);
    assert!(section.summary.contains("REVIEW STALLED"), "{}", section.summary);
    assert!(section.summary.contains("loom"), "{}", section.summary);
    assert_eq!(section.detail["review_stalled"], serde_json::json!(["loom"]));
    assert_eq!(assess(&inputs).exit_code(), EXIT_DEGRADED);
}

/// AC3 (the inverse): a flowing review pipeline stays green even with the
/// same backlog depth — the merge axis is what distinguishes them.
#[test]
fn a_review_backlog_that_is_still_merging_stays_green() {
    let mut inputs = healthy_inputs();
    inputs.pipeline = Some(vec![review_snapshot("loom", 1, REVIEW_STALL_MIN_BACKLOG + 5, 2)]);
    let section = assess_queues(&inputs);

    assert_eq!(section.verdict, Verdict::Green);
    assert!(!section.summary.contains("REVIEW STALLED"), "{}", section.summary);
}

/// A shallow backlog in a quiet window is *not* a stall — the same
/// cry-wolf rule `assess_throughput` follows for a zero-merge window.
#[test]
fn a_shallow_review_backlog_in_a_quiet_window_stays_green() {
    let mut inputs = healthy_inputs();
    inputs.pipeline = Some(vec![review_snapshot("loom", 1, REVIEW_STALL_MIN_BACKLOG - 1, 0)]);
    assert_eq!(assess_queues(&inputs).verdict, Verdict::Green);
}

/// Edge case: unobserved review axes must not be read as "zero backlog" —
/// they render as `null`, never `0`, and cannot produce a stall verdict.
#[test]
fn unobserved_review_axes_are_null_not_zero() {
    let mut inputs = healthy_inputs();
    inputs.pipeline = Some(vec![RepoPipelineSnapshot {
        root: PathBuf::from("/r/loom"),
        queued: Some(1),
        merged_24h: Some(0),
        ..Default::default()
    }]);
    let section = assess_queues(&inputs);

    assert_eq!(section.verdict, Verdict::Green);
    assert_eq!(section.detail["total_review_requested"], serde_json::Value::Null);
    assert_eq!(section.detail["total_changes_requested"], serde_json::Value::Null);
    assert_eq!(section.detail["total_changes_requested_unclaimed"], serde_json::Value::Null);
    assert_eq!(section.detail["total_approved"], serde_json::Value::Null);
    assert!(
        !section.summary.contains("awaiting review"),
        "an unobserved axis must not be summarized at all: {}",
        section.summary
    );
    assert!(
        !section.summary.contains("changes-requested-no-owner"),
        "an unobserved axis must not be summarized at all: {}",
        section.summary
    );
}

/// Edge case: a repo with **zero** ready issues but a stalled review queue
/// must still be caught — the all-repos-summed `total_ready` says 0 across
/// the fleet, which is exactly the "looks idle, is broken" masking #5004
/// reported.
#[test]
fn an_empty_ready_queue_does_not_mask_a_review_stall() {
    let mut inputs = healthy_inputs();
    inputs.pipeline = Some(vec![
        review_snapshot("anvil", 0, 0, 0),
        review_snapshot("loom", 0, REVIEW_STALL_MIN_BACKLOG, 0),
    ]);
    let section = assess_queues(&inputs);

    assert_eq!(section.detail["total_ready"], serde_json::json!(0));
    assert_eq!(section.verdict, Verdict::Degraded);
    assert_eq!(section.detail["review_stalled"], serde_json::json!(["loom"]));
}

/// A stall is a *known* finding and outranks the `Unknown` a single flaky
/// forge query produces — but the failed repo is still named.
#[test]
fn a_review_stall_outranks_a_failed_query_but_still_names_it() {
    let mut inputs = healthy_inputs();
    inputs.pipeline = Some(vec![
        RepoPipelineSnapshot {
            root: PathBuf::from("/r/anvil"),
            queued: None,
            error: Some("rate limited".to_string()),
            ..Default::default()
        },
        review_snapshot("loom", 2, REVIEW_STALL_MIN_BACKLOG, 0),
    ]);
    let section = assess_queues(&inputs);

    assert_eq!(section.verdict, Verdict::Degraded);
    assert!(section.summary.contains("REVIEW STALLED"), "{}", section.summary);
    assert!(section.summary.contains("forge query FAILED for: anvil"), "{}", section.summary);
}

/// The whole-fleet shape of the 2026-08-03 outage: every repo idle on the
/// `queued` axis, nothing merging, PRs piling up awaiting review. The
/// pre-#5021 assessment reported this green.
#[test]
fn a_fleet_wide_judge_outage_is_not_green() {
    let mut inputs = healthy_inputs();
    inputs.pipeline = Some(
        (0..12)
            .map(|i| review_snapshot(&format!("repo{i}"), 0, 4, 0))
            .collect(),
    );
    let section = assess_queues(&inputs);

    assert_ne!(section.verdict, Verdict::Green);
    assert_eq!(section.detail["total_review_requested"], serde_json::json!(48));
    assert_eq!(assess(&inputs).exit_code(), EXIT_DEGRADED);
}

#[test]
fn zero_merges_in_window_is_green() {
    let mut inputs = healthy_inputs();
    inputs.pipeline.as_mut().unwrap()[0].merged_24h = Some(0);
    let section = assess_throughput(&inputs);
    assert_eq!(section.verdict, Verdict::Green);
    assert!(section.summary.contains("0 merged in 30m"));
}

#[test]
fn a_failed_throughput_query_is_unknown() {
    let mut inputs = healthy_inputs();
    inputs.pipeline.as_mut().unwrap()[0].merged_24h = None;
    let section = assess_throughput(&inputs);
    assert_eq!(section.verdict, Verdict::Unknown);
}

#[test]
fn an_empty_fleet_is_green_not_unknown() {
    let mut inputs = healthy_inputs();
    inputs.pipeline = Some(vec![]);
    assert_eq!(assess_queues(&inputs).verdict, Verdict::Green);
    assert_eq!(assess_throughput(&inputs).verdict, Verdict::Green);
    assert_eq!(assess(&inputs).exit_code(), EXIT_HEALTHY);
}

// ===================================================================
// Peer-coordination section (Issue #6157)
// ===================================================================

#[test]
fn peer_coordination_is_green_when_safehouse_not_configured() {
    // `healthy_inputs()` carries no `safehouse`/`peer_claims` at all —
    // the common single-host / non-fleet case.
    let section = assess_peer_coordination(&healthy_inputs());
    assert_eq!(section.verdict, Verdict::Green);
    assert!(section.summary.contains("not configured"));
}

#[test]
fn peer_coordination_is_green_when_receiving_normally() {
    let mut inputs = healthy_inputs();
    inputs.status.as_mut().unwrap().safehouse = Some(crate::types::SafehouseStatus {
        state: "connected".to_string(),
        socket: Some(PathBuf::from("/tmp/safehoused.sock")),
        room: Some("loom-fleet".to_string()),
        reason: None,
    });
    inputs.status.as_mut().unwrap().peer_claims = Some(crate::types::PeerClaimStatus {
        self_host: "robb-studio".to_string(),
        ttl_secs: 120,
        entries: vec![],
        advertised: 2510,
        received: 1800,
        expired: 12,
        dispatch_skipped: 4,
        coordination: crate::types::PeerCoordinationHealth::default(),
        claims_room: Some("!claims:example.org".to_string()),
        same_issue_collisions: 0,
    });
    let section = assess_peer_coordination(&inputs);
    assert_eq!(section.verdict, Verdict::Green);
    assert!(section.summary.contains("1800 received"));
    assert!(section.summary.contains("2510 advertised"));
    // Issue #6242: the resolved claims room is visible in the summary
    // so two hosts' `health` output makes a mismatch a one-line diff.
    assert!(section.summary.contains("!claims:example.org"));
    assert_eq!(section.detail["claims_room"], serde_json::json!("!claims:example.org"));
    // Issue #6243: the counter is always present in the JSON detail (the
    // machine-readable surface the 24h observation reads) …
    assert_eq!(section.detail["same_issue_collisions_24h"], serde_json::json!(0));
    // … but a ZERO count must NOT appear in the human summary, so the
    // pre-#6243 wording is byte-for-byte unchanged for a healthy fleet.
    assert!(!section.summary.contains("#6243"));
}

/// Issue #6243: a NON-zero same-issue cross-host claim count is surfaced
/// in the human-readable summary too — an operator running the 24h
/// verification must not have to reach for `--json` to see the fleet is
/// colliding. The verdict itself is deliberately unchanged (still Green):
/// this section's verdict answers #6157's mesh-liveness question, and
/// building a collision-rate verdict is #6242's scope.
#[test]
fn peer_coordination_surfaces_a_nonzero_same_issue_collision_count() {
    let mut inputs = healthy_inputs();
    inputs.status.as_mut().unwrap().safehouse = Some(crate::types::SafehouseStatus {
        state: "connected".to_string(),
        socket: Some(PathBuf::from("/tmp/safehoused.sock")),
        room: Some("loom-fleet".to_string()),
        reason: None,
    });
    inputs.status.as_mut().unwrap().peer_claims = Some(crate::types::PeerClaimStatus {
        self_host: "robb-studio".to_string(),
        ttl_secs: 120,
        entries: vec![],
        advertised: 2510,
        received: 1800,
        expired: 12,
        dispatch_skipped: 4,
        coordination: crate::types::PeerCoordinationHealth::default(),
        claims_room: Some("!claims:example.org".to_string()),
        same_issue_collisions: 3,
    });
    let section = assess_peer_coordination(&inputs);
    assert_eq!(section.verdict, Verdict::Green, "the counter must not change the verdict");
    assert!(
        section
            .summary
            .contains("3 same-issue cross-host claim(s) in 24h"),
        "summary must name the non-zero count, got: {}",
        section.summary
    );
    assert_eq!(section.detail["same_issue_collisions_24h"], serde_json::json!(3));
}

/// The 2026-08-13 incident's exact signature: `received=0` while
/// `advertised>0` for hours, on every host — must report DEGRADED, not
/// silently Green.
#[test]
fn peer_coordination_is_degraded_when_coordination_reports_degraded() {
    let mut inputs = healthy_inputs();
    inputs.status.as_mut().unwrap().safehouse = Some(crate::types::SafehouseStatus {
        state: "connected".to_string(),
        socket: Some(PathBuf::from("/tmp/safehoused.sock")),
        room: Some("loom-fleet".to_string()),
        reason: None,
    });
    inputs.status.as_mut().unwrap().peer_claims = Some(crate::types::PeerClaimStatus {
        self_host: "robb-studio".to_string(),
        ttl_secs: 120,
        entries: vec![],
        advertised: 2510,
        received: 0,
        expired: 0,
        dispatch_skipped: 0,
        coordination: crate::types::PeerCoordinationHealth {
            degraded: true,
            degraded_for_secs: Some(9000),
            consecutive_receives_toward_recovery: 0,
            recovery_threshold: 3,
        },
        claims_room: None,
        same_issue_collisions: 0,
    });
    let section = assess_peer_coordination(&inputs);
    assert_eq!(section.verdict, Verdict::Degraded);
    assert!(section.summary.contains("DEGRADED"));
    assert!(section.summary.contains("0 received"));
    // Issue #6242: a host with no resolvable room (edge case — safehouse
    // enabled but no `rooms`/legacy `room` configured at all) renders
    // "none", not a missing field or an empty string.
    assert!(section.summary.contains("room: none"));
    assert_eq!(section.detail["claims_room"], serde_json::json!(null));
    assert_eq!(assess(&inputs).exit_code(), EXIT_DEGRADED);
}

#[test]
fn peer_coordination_is_degraded_when_safehouse_socket_unreachable() {
    let mut inputs = healthy_inputs();
    inputs.status.as_mut().unwrap().safehouse = Some(crate::types::SafehouseStatus {
        state: "unreachable".to_string(),
        socket: Some(PathBuf::from("/tmp/safehoused.sock")),
        room: None,
        reason: None,
    });
    // No `peer_claims` yet (the coordination task never got established)
    // — the socket-unreachable check must fire regardless.
    let section = assess_peer_coordination(&inputs);
    assert_eq!(section.verdict, Verdict::Degraded);
    assert!(section.summary.contains("unreachable"));
}

#[test]
fn peer_coordination_unknown_without_status() {
    let mut inputs = healthy_inputs();
    inputs.status = None;
    let section = assess_peer_coordination(&inputs);
    assert_eq!(section.verdict, Verdict::Unknown);
}

// ===================================================================
// --since parsing / formatting
// ===================================================================

#[test]
fn parse_since_accepts_suffixed_and_bare_values() {
    assert_eq!(parse_since("30m").unwrap(), Duration::from_secs(1800));
    assert_eq!(parse_since("2h").unwrap(), Duration::from_secs(7200));
    assert_eq!(parse_since("90s").unwrap(), Duration::from_secs(90));
    assert_eq!(parse_since("1d").unwrap(), Duration::from_secs(86400));
    assert_eq!(parse_since(" 45 ").unwrap(), Duration::from_secs(45));
}

#[test]
fn parse_since_rejects_junk_and_zero() {
    assert!(parse_since("").is_err());
    assert!(parse_since("later").is_err());
    assert!(parse_since("0m").is_err());
    assert!(parse_since("-5m").is_err());
}

#[test]
fn format_window_round_trips_common_values() {
    assert_eq!(format_window(1800), "30m");
    assert_eq!(format_window(7200), "2h");
    assert_eq!(format_window(45), "45s");
}

#[test]
fn format_age_is_compact_and_never_negative() {
    assert_eq!(format_age(-10), "0s");
    assert_eq!(format_age(43), "43s");
    assert_eq!(format_age(600), "10m");
    assert_eq!(format_age(7200), "2h");
    assert_eq!(format_age(400_000), "4d");
}

// ===================================================================
// Stale-untracked-sweep section (Issue #7529)
// ===================================================================

#[test]
fn stale_sweeps_is_green_when_empty() {
    let section = assess_stale_sweeps(&healthy_inputs());
    assert_eq!(section.verdict, Verdict::Green);
    assert!(section.summary.contains("no stale"));
}

#[test]
fn stale_sweeps_is_degraded_and_names_the_issue_when_present() {
    let mut inputs = healthy_inputs();
    inputs.status.as_mut().unwrap().stale_sweeps = vec![crate::types::StaleSweepFinding {
        root: PathBuf::from("/repos/loom"),
        issue: 7529,
        sweep_id: "sweep-issue-7529-1".to_string(),
        pid: 4242,
        elapsed_secs: 20_000,
        log_idle_secs: None,
    }];
    let section = assess_stale_sweeps(&inputs);
    assert_eq!(section.verdict, Verdict::Degraded);
    assert!(section.summary.contains("#7529"));
    assert!(section.summary.contains("4242"));
    assert_eq!(section.detail["count"], serde_json::json!(1));
    // A DEGRADED stale-sweep finding must flip the overall roll-up too —
    // this is meant to be a hard finding, not merely informational.
    assert_eq!(assess(&inputs).overall, Verdict::Degraded);
}

#[test]
fn stale_sweeps_is_unknown_without_a_status_round_trip() {
    let mut inputs = healthy_inputs();
    inputs.status = None;
    inputs.ipc_error = Some("connection refused".to_string());
    assert_eq!(assess_stale_sweeps(&inputs).verdict, Verdict::Unknown);
}

// ===================================================================
// Auto-update section (Issue #7584)
// ===================================================================

/// A stale (but not yet blocked) [`crate::self_update::SelfUpdateStatus`]
/// fixture: `commits`/`hours` behind, tunable per test.
fn stale_self_update(
    commits_behind: u32,
    hours_behind: u32,
) -> crate::self_update::SelfUpdateStatus {
    crate::self_update::SelfUpdateStatus {
        built_commit: CLI_COMMIT.to_string(),
        source_commit: Some("deadbee".to_string()),
        update_available: Some(true),
        commits_behind: Some(commits_behind),
        hours_behind: Some(hours_behind),
    }
}

#[test]
fn auto_update_is_green_when_disabled() {
    let mut inputs = healthy_inputs();
    inputs.status.as_mut().unwrap().auto_update_enabled = false;
    // Even wildly stale — disabled is a deliberate opt-out, not a fault.
    inputs.self_update = Some(stale_self_update(500, 500));
    let section = assess_auto_update(&inputs);
    assert_eq!(section.verdict, Verdict::Green);
    assert!(section.summary.contains("disabled"));
}

#[test]
fn auto_update_is_green_when_up_to_date() {
    let mut inputs = healthy_inputs();
    inputs.status.as_mut().unwrap().auto_update_enabled = true;
    inputs.status.as_mut().unwrap().auto_update_note =
        Some("up to date with source HEAD".to_string());
    inputs.self_update = Some(healthy_self_update());
    let section = assess_auto_update(&inputs);
    assert_eq!(section.verdict, Verdict::Green);
}

#[test]
fn auto_update_is_green_when_stale_but_settling_under_threshold() {
    let mut inputs = healthy_inputs();
    let status = inputs.status.as_mut().unwrap();
    status.auto_update_enabled = true;
    status.auto_update_note =
        Some("within settle window — waiting for commits to settle".to_string());
    // Below both default thresholds (10 commits / 12h) — must not escalate.
    inputs.self_update = Some(stale_self_update(3, 2));
    let section = assess_auto_update(&inputs);
    assert_eq!(section.verdict, Verdict::Green, "{}", section.summary);
}

#[test]
fn auto_update_is_green_when_stale_past_threshold_but_only_deferring() {
    // Past the warn thresholds, but the loop is merely deferring past
    // in-flight sweeps (normal, self-resolving) rather than blocked by a
    // dirty tree or a build-failure backoff — must not escalate.
    let mut inputs = healthy_inputs();
    let status = inputs.status.as_mut().unwrap();
    status.auto_update_enabled = true;
    status.auto_update_note =
        Some("3 in-flight sweep(s) — deferring the source rebuild (#8252)".to_string());
    inputs.self_update = Some(stale_self_update(199, 110));
    let section = assess_auto_update(&inputs);
    assert_eq!(section.verdict, Verdict::Green, "{}", section.summary);
}

#[test]
fn auto_update_is_degraded_and_names_dirty_tree_past_threshold() {
    // The exact scenario Issue #7584 was filed for: a stray untracked
    // file makes `source_tree_clean()` refuse the rebuild indefinitely.
    let mut inputs = healthy_inputs();
    let status = inputs.status.as_mut().unwrap();
    status.auto_update_enabled = true;
    status.auto_update_note = Some(
        "source tree is dirty — refusing an unattended rebuild (never `git pull`)".to_string(),
    );
    // Past both default thresholds (10 commits / 12h).
    inputs.self_update = Some(stale_self_update(199, 110));
    let section = assess_auto_update(&inputs);
    assert_eq!(section.verdict, Verdict::Degraded, "{}", section.summary);
    assert!(section.summary.contains("dirty"), "{}", section.summary);
    // Overall must escalate too — this is a hard finding.
    assert_eq!(assess(&inputs).overall, Verdict::Degraded);
}

#[test]
fn auto_update_is_green_when_dirty_but_under_threshold() {
    // A dirty tree that has only just gone stale (below both thresholds)
    // is not yet worth paging on.
    let mut inputs = healthy_inputs();
    let status = inputs.status.as_mut().unwrap();
    status.auto_update_enabled = true;
    status.auto_update_note = Some(
        "source tree is dirty — refusing an unattended rebuild (never `git pull`)".to_string(),
    );
    inputs.self_update = Some(stale_self_update(2, 1));
    let section = assess_auto_update(&inputs);
    assert_eq!(section.verdict, Verdict::Green, "{}", section.summary);
}

#[test]
fn auto_update_is_degraded_and_names_backoff_past_threshold() {
    let mut inputs = healthy_inputs();
    let status = inputs.status.as_mut().unwrap();
    status.auto_update_enabled = true;
    status.auto_update_backoff_secs = Some(120);
    status.auto_update_note =
        Some("rebuild failed (attempt 2, backing off 120s): exit status 1".to_string());
    inputs.self_update = Some(stale_self_update(50, 30));
    let section = assess_auto_update(&inputs);
    assert_eq!(section.verdict, Verdict::Degraded, "{}", section.summary);
    assert!(section.summary.contains("backing off"), "{}", section.summary);
}

#[test]
fn auto_update_is_degraded_and_names_the_terminal_reason() {
    let mut inputs = healthy_inputs();
    let status = inputs.status.as_mut().unwrap();
    status.auto_update_enabled = true;
    status.auto_update_terminal_reason = Some("build verification mismatch (#4053)".to_string());
    status.auto_update_note = Some(
        "rebuild TERMINALLY failed — not retrying until a new commit: build verification \
             mismatch (#4053)"
            .to_string(),
    );
    // Terminal escalates regardless of staleness magnitude.
    inputs.self_update = Some(stale_self_update(1, 1));
    let section = assess_auto_update(&inputs);
    assert_eq!(section.verdict, Verdict::Degraded, "{}", section.summary);
    assert!(
        section
            .summary
            .contains("build verification mismatch (#4053)"),
        "{}",
        section.summary
    );
    assert_eq!(assess(&inputs).overall, Verdict::Degraded);
}

#[test]
fn auto_update_is_unknown_without_a_status_round_trip() {
    let mut inputs = healthy_inputs();
    inputs.status = None;
    inputs.ipc_error = Some("connection refused".to_string());
    assert_eq!(assess_auto_update(&inputs).verdict, Verdict::Unknown);
}

#[test]
fn auto_update_detail_carries_the_documented_fields() {
    let mut inputs = healthy_inputs();
    let status = inputs.status.as_mut().unwrap();
    status.auto_update_enabled = true;
    status.auto_update_backoff_secs = Some(60);
    status.auto_update_note = Some("backing off".to_string());
    inputs.self_update = Some(stale_self_update(15, 20));
    let detail = assess_auto_update(&inputs).detail;
    for key in [
        "enabled",
        "commits_behind",
        "hours_behind",
        "update_available",
        "note",
        "terminal_reason",
        "backoff_secs",
    ] {
        assert!(detail.get(key).is_some(), "missing detail key {key}");
    }
    assert_eq!(detail["commits_behind"], serde_json::json!(15));
    assert_eq!(detail["hours_behind"], serde_json::json!(20));
}

// ===================================================================
// Codesign identity preflight (#7605)
// ============================================================
#[test]
fn codesign_identity_absent_produces_no_section() {
    // The healthy baseline: no identity configured at all.
    let inputs = healthy_inputs();
    assert!(assess_codesign_identity(&inputs).is_none());
    assert_eq!(assess(&inputs).overall, Verdict::Green);
}

#[test]
fn codesign_identity_passing_preflight_produces_no_section() {
    // A configured identity that DID sign non-interactively is not an
    // anomaly -- anomaly-only, same rule as `assess_observability`.
    let mut inputs = healthy_inputs();
    inputs.codesign_preflight = Some(CodesignPreflightResult {
        identity: "Loom Local Signing".to_string(),
        ok: true,
        detail: String::new(),
    });
    assert!(assess_codesign_identity(&inputs).is_none());
    assert_eq!(assess(&inputs).overall, Verdict::Green);
}

#[test]
fn codesign_identity_failing_preflight_is_degraded_and_names_identity_and_doc() {
    let mut inputs = healthy_inputs();
    inputs.codesign_preflight = Some(CodesignPreflightResult {
        identity: "Loom Local Signing".to_string(),
        ok: false,
        detail: "timed out after 15s".to_string(),
    });
    let section = assess_codesign_identity(&inputs).expect("section present");
    assert_eq!(section.key, "codesign_identity");
    assert_eq!(section.verdict, Verdict::Degraded);
    assert!(section.summary.contains("Loom Local Signing"), "{}", section.summary);
    assert!(section.summary.contains("timed out after 15s"), "{}", section.summary);
    assert!(section.summary.contains("macos-tcc-codesign.md"), "{}", section.summary);
    assert_eq!(section.detail["identity"], serde_json::json!("Loom Local Signing"));
    assert_eq!(assess(&inputs).overall, Verdict::Degraded);
}

/// Issue #8286: the DEGRADED wording must say *where* the preflight ran and
/// point at re-running from an interactive/tty session, so a failing
/// preflight observed over non-interactive ssh (the login keychain refusing
/// access, independent of whether the identity or the daemon itself is
/// actually broken) is not misread as "the identity fix didn't take" — the
/// exact misreading behind example-org/tool-repo#202.
#[test]
fn codesign_identity_failing_preflight_names_its_own_invocation_context() {
    let mut inputs = healthy_inputs();
    inputs.codesign_preflight = Some(CodesignPreflightResult {
        identity: "Developer ID Application: Example".to_string(),
        ok: false,
        detail: "codesign exited with exit status: 1".to_string(),
    });
    let section = assess_codesign_identity(&inputs).expect("section present");
    assert!(
        section
            .summary
            .contains("THIS `health` invocation's own process context"),
        "{}",
        section.summary
    );
    assert!(section.summary.contains("non-interactive ssh"), "{}", section.summary);
    assert!(section.summary.contains("interactive/tty session"), "{}", section.summary);
}

/// Issue #7609: `health` reports the release artifact available for this
/// host's platform next to the installed version — the fleet-visible
/// answer to "is a newer signed binary published?". `null` (not absent)
/// when no artifact resolved, so a consumer can tell "none available"
/// from "this daemon predates the field".
#[test]
fn auto_update_detail_reports_the_available_artifact() {
    let mut inputs = healthy_inputs();
    let status = inputs.status.as_mut().unwrap();
    status.auto_update_enabled = true;
    status.auto_update_artifact_version = Some("0.19.24".to_string());
    status.auto_update_artifact_published_at = Some("2026-09-13T12:00:00Z".to_string());
    let detail = assess_auto_update(&inputs).detail;
    assert_eq!(detail["artifact_available"]["version"], serde_json::json!("0.19.24"));
    assert_eq!(
        detail["artifact_available"]["published_at"],
        serde_json::json!("2026-09-13T12:00:00Z")
    );
    assert_eq!(detail["installed_version"], serde_json::json!(env!("CARGO_PKG_VERSION")));

    let mut inputs = healthy_inputs();
    let status = inputs.status.as_mut().unwrap();
    status.auto_update_enabled = true;
    let detail = assess_auto_update(&inputs).detail;
    assert_eq!(detail["artifact_available"], serde_json::Value::Null);
}

// ===================================================================
// Stuck worktree-removal backoff section (Issue #7590)
// ===================================================================

fn stuck_reclaim_fixture(attempt_count: u32, cause: &str) -> crate::types::StuckWorktreeReclaim {
    crate::types::StuckWorktreeReclaim {
        repo_root: PathBuf::from("/repos/loom"),
        kind: "issue".to_string(),
        number: 7590,
        path: PathBuf::from("/repos/loom/.loom/worktrees/issue-7590"),
        cause: cause.to_string(),
        first_failure_at: now() - chrono::Duration::hours(6),
        last_attempt_at: now() - chrono::Duration::minutes(5),
        attempt_count,
    }
}

#[test]
fn worktree_reaper_is_green_when_nothing_stuck() {
    let section = assess_worktree_reaper(&healthy_inputs());
    assert_eq!(section.verdict, Verdict::Green);
    assert!(section.summary.contains("no worktree removals"));
}

#[test]
fn worktree_reaper_is_degraded_and_names_the_stuck_path() {
    let mut inputs = healthy_inputs();
    inputs.status.as_mut().unwrap().stuck_worktree_reclaims =
        vec![stuck_reclaim_fixture(1, "Permission denied (os error 13)")];
    let section = assess_worktree_reaper(&inputs);
    assert_eq!(section.verdict, Verdict::Degraded, "{}", section.summary);
    assert!(section.summary.contains("issue-7590"), "{}", section.summary);
    assert!(section.summary.contains("loom"), "{}", section.summary);
    assert!(section.summary.contains("Permission denied"), "{}", section.summary);
    assert_eq!(section.detail["count"], serde_json::json!(1));
    // A stuck removal must flip the overall roll-up too — this is meant
    // to be a hard finding, not merely informational.
    assert_eq!(assess(&inputs).overall, Verdict::Degraded);
}

#[test]
fn worktree_reaper_is_unknown_without_a_status_round_trip() {
    let mut inputs = healthy_inputs();
    inputs.status = None;
    inputs.ipc_error = Some("connection refused".to_string());
    assert_eq!(assess_worktree_reaper(&inputs).verdict, Verdict::Unknown);
}

mod section_inventory;

// Wrong-repo-resolution escalation coverage (Issue #8513) — its own child
// module so this file, already over `.loom/docs/file-size-policy.md`'s
// threshold, does not grow to hold it.
mod auto_update_stale_repo;

#[cfg(test)]
mod model_class_tests;

// Export-liveness coverage (#5083, #5337, #9015) — moved out of this file for
// the same reason as the two modules above: it is over
// `.loom/docs/file-size-policy.md`'s threshold and may not grow.
mod observability_export;
