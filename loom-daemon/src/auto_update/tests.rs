use super::*;
use serial_test::serial;
// `compare_versions` moved to the `artifact_verdict` sibling in #8513; it is
// crate-private there, so name it explicitly rather than re-exporting it.
use super::artifact_verdict::compare_versions;

// Wrong-repo-resolution coverage (Issue #8513) lives in its own child module
// so this file, already over `.loom/docs/file-size-policy.md`'s threshold,
// does not grow to hold it. It reuses the fixtures below via `use super::*`.
mod stale_repo;
use std::fs;
use std::sync::atomic::{AtomicUsize, Ordering};

fn write_config(root: &Path, contents: &str) {
    fs::create_dir_all(root.join(".loom")).unwrap();
    fs::write(root.join(".loom").join("config.json"), contents).unwrap();
}

fn write_project_config(root: &Path, contents: &str) {
    let full = root.join(crate::config_resolver::PROJECT_CONFIG_REL);
    fs::create_dir_all(full.parent().unwrap()).unwrap();
    fs::write(full, contents).unwrap();
}

fn stale(commit: &str) -> UpdateCheck {
    UpdateCheck {
        update_available: Some(true),
        source_commit: Some(commit.to_string()),
        commits_behind: None,
        hours_behind: None,
    }
}

fn stale_with_lag(commit: &str, commits_behind: u32, hours_behind: u32) -> UpdateCheck {
    UpdateCheck {
        commits_behind: Some(commits_behind),
        hours_behind: Some(hours_behind),
        ..stale(commit)
    }
}

// ===================================================================
// Config surface — autonomous.autoUpdate (soft-fail + happy path)
// ===================================================================

#[test]
#[serial(loom_config_env)]
fn test_config_missing_file_is_default() {
    std::env::set_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV, "");
    let tmp = tempfile::tempdir().unwrap();
    let cfg = read_auto_update_config(tmp.path());
    std::env::remove_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV);
    assert_eq!(cfg, AutoUpdateConfig::default());
}

#[test]
#[serial(loom_config_env)]
fn test_config_malformed_json_is_default() {
    std::env::set_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV, "");
    let tmp = tempfile::tempdir().unwrap();
    write_config(tmp.path(), "{not valid json");
    let cfg = read_auto_update_config(tmp.path());
    std::env::remove_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV);
    assert_eq!(cfg, AutoUpdateConfig::default());
}

#[test]
#[serial(loom_config_env)]
fn test_config_missing_block_is_default() {
    std::env::set_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV, "");
    let tmp = tempfile::tempdir().unwrap();
    write_config(tmp.path(), r#"{"autonomous": {"workFinder": {"enabled": true}}}"#);
    let cfg = read_auto_update_config(tmp.path());
    std::env::remove_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV);
    assert_eq!(cfg, AutoUpdateConfig::default());
}

#[test]
fn test_config_reads_all_fields() {
    let tmp = tempfile::tempdir().unwrap();
    write_config(
        tmp.path(),
        r#"{"autonomous": {"autoUpdate": {"enabled": true, "intervalSecs": 120, "settleSecs": 30, "deferDeadlineSecs": 7200}}}"#,
    );
    assert_eq!(
        read_auto_update_config(tmp.path()),
        AutoUpdateConfig {
            enabled: Some(true),
            interval_secs: Some(120),
            settle_secs: Some(30),
            defer_deadline_secs: Some(7200),
        }
    );
}

#[test]
fn test_config_zero_values_dropped_to_none() {
    let tmp = tempfile::tempdir().unwrap();
    write_config(
        tmp.path(),
        r#"{"autonomous": {"autoUpdate": {"intervalSecs": 0, "settleSecs": 0, "deferDeadlineSecs": 0}}}"#,
    );
    let cfg = read_auto_update_config(tmp.path());
    assert_eq!(cfg.interval_secs, None);
    assert_eq!(cfg.settle_secs, None);
    assert_eq!(cfg.defer_deadline_secs, None);
}

// ===================================================================
// config_resolver migration (#4058) — .loom-project/ tier
// ===================================================================

#[test]
#[serial(loom_config_env)]
fn test_config_project_tier_only_is_honored() {
    std::env::set_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV, "");
    let tmp = tempfile::tempdir().unwrap();
    write_project_config(
        tmp.path(),
        r#"{"autonomous": {"autoUpdate": {"enabled": true, "intervalSecs": 120}}}"#,
    );
    let cfg = read_auto_update_config(tmp.path());
    std::env::remove_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV);
    assert_eq!(cfg.enabled, Some(true));
    assert_eq!(cfg.interval_secs, Some(120));
}

#[test]
#[serial(loom_config_env)]
fn test_config_project_tier_overrides_legacy() {
    std::env::set_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV, "");
    let tmp = tempfile::tempdir().unwrap();
    write_config(
        tmp.path(),
        r#"{"autonomous": {"autoUpdate": {"enabled": true, "settleSecs": 600}}}"#,
    );
    write_project_config(tmp.path(), r#"{"autonomous": {"autoUpdate": {"settleSecs": 30}}}"#);
    let cfg = read_auto_update_config(tmp.path());
    std::env::remove_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV);
    // Overlapping settleSecs -> project tier wins; non-overlapping enabled
    // still supplied by the legacy tier.
    assert_eq!(cfg.settle_secs, Some(30));
    assert_eq!(cfg.enabled, Some(true));
}

// ===================================================================
// Precedence — env > config > default
// ===================================================================

#[test]
#[serial]
fn test_resolve_enabled_default_is_false() {
    std::env::remove_var(AUTO_UPDATE_ENABLE_ENV);
    assert!(
        !resolve_enabled(&AutoUpdateConfig::default()),
        "absent config + unset env ⇒ default OFF (opt-in loop)"
    );
}

#[test]
#[serial]
fn test_resolve_enabled_config_then_env() {
    std::env::remove_var(AUTO_UPDATE_ENABLE_ENV);
    assert!(resolve_enabled(&AutoUpdateConfig {
        enabled: Some(true),
        ..AutoUpdateConfig::default()
    }));
    // Env forces OFF over config-on.
    std::env::set_var(AUTO_UPDATE_ENABLE_ENV, "0");
    assert!(!resolve_enabled(&AutoUpdateConfig {
        enabled: Some(true),
        ..AutoUpdateConfig::default()
    }));
    // Env forces ON over config-off.
    std::env::set_var(AUTO_UPDATE_ENABLE_ENV, "1");
    assert!(resolve_enabled(&AutoUpdateConfig {
        enabled: Some(false),
        ..AutoUpdateConfig::default()
    }));
    std::env::remove_var(AUTO_UPDATE_ENABLE_ENV);
}

#[test]
#[serial]
fn test_resolve_interval_and_settle_precedence() {
    std::env::remove_var(AUTO_UPDATE_INTERVAL_ENV);
    std::env::remove_var(AUTO_UPDATE_SETTLE_ENV);

    // Defaults.
    assert_eq!(
        resolve_interval(&AutoUpdateConfig::default()),
        Duration::from_secs(DEFAULT_AUTO_UPDATE_INTERVAL_SECS)
    );
    assert_eq!(
        resolve_settle(&AutoUpdateConfig::default()),
        Duration::from_secs(DEFAULT_AUTO_UPDATE_SETTLE_SECS)
    );

    // Config alone.
    let cfg = AutoUpdateConfig {
        enabled: None,
        interval_secs: Some(300),
        settle_secs: Some(45),
        defer_deadline_secs: None,
    };
    assert_eq!(resolve_interval(&cfg), Duration::from_secs(300));
    assert_eq!(resolve_settle(&cfg), Duration::from_secs(45));

    // Env overrides config.
    std::env::set_var(AUTO_UPDATE_INTERVAL_ENV, "77");
    std::env::set_var(AUTO_UPDATE_SETTLE_ENV, "11");
    assert_eq!(resolve_interval(&cfg), Duration::from_secs(77));
    assert_eq!(resolve_settle(&cfg), Duration::from_secs(11));

    // Zero/garbage env falls through to config, not the default.
    std::env::set_var(AUTO_UPDATE_INTERVAL_ENV, "0");
    std::env::set_var(AUTO_UPDATE_SETTLE_ENV, "garbage");
    assert_eq!(resolve_interval(&cfg), Duration::from_secs(300));
    assert_eq!(resolve_settle(&cfg), Duration::from_secs(45));

    std::env::remove_var(AUTO_UPDATE_INTERVAL_ENV);
    std::env::remove_var(AUTO_UPDATE_SETTLE_ENV);
}

/// Gate 4's deferral deadline (#4929) resolves **env > config > default**
/// like every other knob on this block.
#[test]
#[serial]
fn test_resolve_defer_deadline_precedence() {
    std::env::remove_var(AUTO_UPDATE_DEFER_DEADLINE_ENV);
    assert_eq!(
        resolve_defer_deadline(&AutoUpdateConfig::default()),
        Duration::from_secs(DEFAULT_AUTO_UPDATE_DEFER_DEADLINE_SECS)
    );

    let cfg = AutoUpdateConfig {
        defer_deadline_secs: Some(1800),
        ..AutoUpdateConfig::default()
    };
    assert_eq!(resolve_defer_deadline(&cfg), Duration::from_secs(1800));

    std::env::set_var(AUTO_UPDATE_DEFER_DEADLINE_ENV, "60");
    assert_eq!(resolve_defer_deadline(&cfg), Duration::from_secs(60));

    // Zero/garbage env falls through to config, never to "defer forever".
    std::env::set_var(AUTO_UPDATE_DEFER_DEADLINE_ENV, "0");
    assert_eq!(resolve_defer_deadline(&cfg), Duration::from_secs(1800));
    std::env::set_var(AUTO_UPDATE_DEFER_DEADLINE_ENV, "garbage");
    assert_eq!(resolve_defer_deadline(&cfg), Duration::from_secs(1800));

    std::env::remove_var(AUTO_UPDATE_DEFER_DEADLINE_ENV);
}

// ===================================================================
// Backoff math
// ===================================================================

#[test]
fn test_backoff_is_exponential_with_ceiling() {
    assert_eq!(backoff_delay(1), BACKOFF_BASE);
    assert_eq!(backoff_delay(2), Duration::from_secs(120));
    assert_eq!(backoff_delay(3), Duration::from_secs(240));
    // Eventually clamps at the ceiling and never overflows.
    assert_eq!(backoff_delay(10), BACKOFF_CEILING);
    assert_eq!(backoff_delay(u32::MAX), BACKOFF_CEILING);
}

// ===================================================================
// Decision logic — the settle/clean/gate matrix
// ===================================================================

const SETTLE: Duration = Duration::from_secs(60);
/// Gate 4's deferral deadline for the decision tests: long enough that the
/// existing gate-4 cases still exercise the *deferring* branch.
const DEFER: Duration = Duration::from_secs(3600);

#[test]
fn test_decide_up_to_date_is_skip() {
    let mut st = AutoUpdateState::new();
    let now = Instant::now();
    let check = UpdateCheck {
        update_available: Some(false),
        source_commit: Some("abc".into()),
        commits_behind: None,
        hours_behind: None,
    };
    assert!(matches!(
        st.decide_source(now, &check, true, 0, SETTLE, DEFER),
        TickDecision::Skip(_)
    ));
}

#[test]
fn test_decide_undecidable_none_is_skip() {
    let mut st = AutoUpdateState::new();
    let now = Instant::now();
    let check = UpdateCheck {
        update_available: None,
        source_commit: None,
        commits_behind: None,
        hours_behind: None,
    };
    assert!(matches!(
        st.decide_source(now, &check, true, 0, SETTLE, DEFER),
        TickDecision::Skip(_)
    ));
}

#[test]
fn test_decide_stale_dirty_tree_is_skip() {
    let mut st = AutoUpdateState::new();
    let base = Instant::now();
    // First observe (starts settle timer), then advance past settle.
    st.decide_source(base, &stale("c1"), false, 0, SETTLE, DEFER);
    let later = base + SETTLE + Duration::from_secs(1);
    let d = st.decide_source(later, &stale("c1"), false, 0, SETTLE, DEFER);
    assert!(matches!(d, TickDecision::Skip(reason) if reason.contains("dirty")));
}

#[test]
fn test_decide_stale_clean_within_settle_is_skip() {
    let mut st = AutoUpdateState::new();
    let base = Instant::now();
    let d = st.decide_source(base, &stale("c1"), true, 0, SETTLE, DEFER);
    assert!(matches!(d, TickDecision::Skip(reason) if reason.contains("settle")));
}

#[test]
fn test_decide_stale_clean_settled_zero_inflight_is_rebuild() {
    let mut st = AutoUpdateState::new();
    let base = Instant::now();
    st.decide_source(base, &stale("c1"), true, 0, SETTLE, DEFER);
    let later = base + SETTLE + Duration::from_secs(1);
    assert_eq!(
        st.decide_source(later, &stale("c1"), true, 0, SETTLE, DEFER),
        TickDecision::Rebuild {
            low_priority: false
        }
    );
}

#[test]
fn test_decide_gate4_inflight_sweeps_blocks_rebuild() {
    let mut st = AutoUpdateState::new();
    let base = Instant::now();
    st.decide_source(base, &stale("c1"), true, 3, SETTLE, DEFER);
    let later = base + SETTLE + Duration::from_secs(1);
    let d = st.decide_source(later, &stale("c1"), true, 3, SETTLE, DEFER);
    assert!(matches!(d, TickDecision::Skip(reason) if reason.contains("in-flight")));
}

// ===================================================================
// Gate 4's deferral deadline (#4929) — a permanently saturated host must
// still converge instead of deferring the rebuild forever.
// ===================================================================

/// The starvation case from #4929: the host never reaches zero in-flight
/// sweeps, so gate 4 defers at every check. Before the deadline it keeps
/// deferring (unchanged behavior); once the deadline elapses it rebuilds
/// anyway, at low priority — so `last_roll` can finally become non-null.
#[test]
fn test_decide_gate4_deadline_forces_low_priority_rebuild() {
    let mut st = AutoUpdateState::new();
    let base = Instant::now();
    // First observation starts both the settle timer and (once settled) the
    // gate-4 deferral clock.
    st.decide_source(base, &stale("c1"), true, 13, SETTLE, DEFER);

    // Settled, but still busy: deferral begins here.
    let settled = base + SETTLE + Duration::from_secs(1);
    let d = st.decide_source(settled, &stale("c1"), true, 13, SETTLE, DEFER);
    assert!(
        matches!(&d, TickDecision::Skip(reason) if reason.contains("in-flight")),
        "still within the deadline ⇒ defer, got {d:?}"
    );

    // Just short of the deadline: still deferring, and the note counts down.
    let almost = settled + DEFER - Duration::from_secs(1);
    let d = st.decide_source(almost, &stale("c1"), true, 13, SETTLE, DEFER);
    assert!(
        matches!(&d, TickDecision::Skip(reason) if reason.contains("low-priority rebuild in")),
        "one second short of the deadline must still defer, got {d:?}"
    );

    // Past the deadline with the host STILL saturated: rebuild anyway.
    let past = settled + DEFER + Duration::from_secs(1);
    assert_eq!(
        st.decide_source(past, &stale("c1"), true, 13, SETTLE, DEFER),
        TickDecision::Rebuild { low_priority: true },
        "a continuously saturated host must eventually rebuild (#4929)"
    );
}

/// The deadline measures *continuous* deferral: a host that dips to zero
/// in-flight sweeps re-arms it, so a long series of short busy bursts never
/// accumulates into a forced build-under-load (the "do not trade
/// never-rebuilds for stampede-on-every-busy-period" edge case).
#[test]
fn test_decide_gate4_deadline_resets_when_host_goes_idle() {
    let mut st = AutoUpdateState::new();
    let base = Instant::now();
    st.decide_source(base, &stale("c1"), true, 2, SETTLE, DEFER);

    // Busy for most of the deadline...
    let busy = base + SETTLE + DEFER - Duration::from_secs(1);
    assert!(matches!(
        st.decide_source(busy, &stale("c1"), true, 2, SETTLE, DEFER),
        TickDecision::Skip(_)
    ));

    // ...then one idle observation: that tick rolls normally, at NORMAL
    // priority, because the host is quiescent.
    let idle = busy + Duration::from_secs(1);
    assert_eq!(
        st.decide_source(idle, &stale("c1"), true, 0, SETTLE, DEFER),
        TickDecision::Rebuild {
            low_priority: false
        }
    );

    // And the deferral clock restarted, so a later busy tick defers again
    // rather than immediately forcing a build.
    let busy_again = idle + Duration::from_secs(1);
    assert!(
        matches!(
            st.decide_source(busy_again, &stale("c1"), true, 2, SETTLE, DEFER),
            TickDecision::Skip(reason) if reason.contains("in-flight")
        ),
        "an idle observation must re-arm the gate-4 deadline"
    );
}

/// Issue #6261 fix: a new source commit landing while the host has been
/// CONTINUOUSLY busy must NOT restart gate 4's deferral clock (the
/// pre-fix behavior this test used to assert, under the name
/// `test_decide_gate4_deadline_resets_on_new_commit` — that reset is
/// exactly the bug: a host busy for hours with commits landing
/// throughout never accumulated toward `deferDeadlineSecs` at all).
/// `first_stale_since` (also not reset by a new commit) lets the
/// settle-ceiling carry the tick straight past the settle gate too, so
/// the already-overdue rebuild fires on the SAME tick the new commit is
/// observed, rather than deferring for another full settle + deadline.
#[test]
fn test_decide_gate4_deadline_persists_across_new_commit_when_continuously_busy() {
    let mut st = AutoUpdateState::new();
    let base = Instant::now();
    st.decide_source(base, &stale("c1"), true, 4, SETTLE, DEFER);
    // The deferral clock only starts once a tick actually reaches gate 4
    // (i.e. past the settle window), so take one settled-but-busy tick.
    let settled = base + SETTLE + Duration::from_secs(1);
    st.decide_source(settled, &stale("c1"), true, 4, SETTLE, DEFER);
    let deep = settled + DEFER + Duration::from_secs(1);
    // c1 would force a rebuild now...
    assert_eq!(
        st.decide_source(deep, &stale("c1"), true, 4, SETTLE, DEFER),
        TickDecision::Rebuild { low_priority: true }
    );
    // ...and a new commit landing on the SAME tick, with the host STILL
    // busy throughout, does not reset the clock: the rebuild it was
    // already overdue for fires immediately instead of deferring again.
    assert_eq!(
        st.decide_source(deep, &stale("c2"), true, 4, SETTLE, DEFER),
        TickDecision::Rebuild { low_priority: true },
        "a new commit while continuously busy must not restart the deferral clock (#6261)"
    );
}

/// A successful rebuild re-arms the deadline, so a roll whose drain was
/// refused does not re-force a build-under-load on every subsequent tick.
#[test]
fn test_successful_rebuild_rearms_gate4_deadline() {
    let mut st = AutoUpdateState::new();
    let base = Instant::now();
    st.decide_source(base, &stale("c1"), true, 5, SETTLE, DEFER);
    // One settled-but-busy tick starts gate 4's deferral clock.
    let settled = base + SETTLE + Duration::from_secs(1);
    st.decide_source(settled, &stale("c1"), true, 5, SETTLE, DEFER);
    let past = settled + DEFER + Duration::from_secs(1);
    assert_eq!(
        st.decide_source(past, &stale("c1"), true, 5, SETTLE, DEFER),
        TickDecision::Rebuild { low_priority: true }
    );
    // Provisioned, but the drain was refused ⇒ still reported stale.
    st.record_rebuild(past, &RebuildOutcome::Success, false);
    assert!(st.last_roll.is_some(), "#4929: last_roll must go non-null under saturation");

    let next = past + Duration::from_secs(900);
    let d = st.decide_source(next, &stale("c1"), true, 5, SETTLE, DEFER);
    assert!(
        matches!(&d, TickDecision::Skip(reason) if reason.contains("in-flight")),
        "the next forced rebuild must wait another full deadline, got {d:?}"
    );
}

#[test]
fn test_new_commit_resets_settle_window() {
    let mut st = AutoUpdateState::new();
    let base = Instant::now();
    st.decide_source(base, &stale("c1"), true, 0, SETTLE, DEFER);
    // Settled for c1...
    let later = base + SETTLE + Duration::from_secs(1);
    // ...but a NEW commit lands: the settle timer restarts, so this tick is
    // within-settle again (not a rebuild).
    let d = st.decide_source(later, &stale("c2"), true, 0, SETTLE, DEFER);
    assert!(matches!(d, TickDecision::Skip(reason) if reason.contains("settle")));
}

/// Issue #6261: a stream of commits landing MORE OFTEN than the settle
/// window apart (the 2026-08-14 incident's suspected shape — a 20-merge
/// day against a 600s settle window) must still converge on a rebuild
/// within a bounded worst case, rather than deferring the first attempt
/// forever via repeated `stale_since` resets.
#[test]
fn test_repeated_commits_within_settle_still_converge_via_ceiling() {
    let mut st = AutoUpdateState::new();
    let base = Instant::now();
    let mut t = base;
    let mut last = None;
    // Each iteration lands a NEW commit strictly inside the quiet-period
    // window, so the quiet-period test alone would never pass.
    for i in 0..20u32 {
        t += SETTLE - Duration::from_secs(1);
        let commit = format!("c{i}");
        let d = st.decide_source(t, &stale(&commit), true, 0, SETTLE, DEFER);
        let is_rebuild = matches!(d, TickDecision::Rebuild { .. });
        last = Some(d);
        if is_rebuild {
            break;
        }
    }
    assert_eq!(
        last,
        Some(TickDecision::Rebuild {
            low_priority: false
        }),
        "a stream of sub-settle-interval commits must still converge via the ceiling"
    );
    // Bounded: must not take dramatically longer than the documented
    // ceiling (SETTLE_CEILING_MULTIPLIER * SETTLE from the FIRST stale
    // observation), not merely "eventually". `first_stale_since` is set
    // on the FIRST `decide()` call (at `base + (SETTLE - 1s)`, not
    // `base` itself), and the sub-settle-interval step size means the
    // ceiling can be crossed up to one step late — so allow two extra
    // settle windows of slack on top of the ceiling rather than an exact
    // bound.
    assert!(
        t.duration_since(base) <= SETTLE * (SETTLE_CEILING_MULTIPLIER + 2),
        "converged too slowly: {:?} vs. the {:?} ceiling",
        t.duration_since(base),
        SETTLE * SETTLE_CEILING_MULTIPLIER
    );
}

// ===================================================================
// Backoff + terminal state transitions
// ===================================================================

#[test]
fn test_retryable_failures_back_off_then_reset_on_success() {
    let mut st = AutoUpdateState::new();
    let t0 = Instant::now();
    // Establish a tracked commit + settle so backoff_until is meaningful.
    st.decide_source(t0, &stale("c1"), true, 0, SETTLE, DEFER);

    st.record_rebuild(t0, &RebuildOutcome::Retryable("boom".into()), false);
    assert_eq!(st.consecutive_failures, 1);
    assert_eq!(st.backoff, Some(backoff_delay(1)));

    st.record_rebuild(t0, &RebuildOutcome::Retryable("boom".into()), false);
    assert_eq!(st.consecutive_failures, 2);
    assert_eq!(st.backoff, Some(backoff_delay(2)));

    // A success (with an accepted drain) resets the counter + clears backoff.
    st.record_rebuild(t0, &RebuildOutcome::Success, true);
    assert_eq!(st.consecutive_failures, 0);
    assert_eq!(st.backoff, None);
    assert!(st.last_roll.is_some());
}

#[test]
fn test_backing_off_blocks_rebuild_until_delay_elapses() {
    let mut st = AutoUpdateState::new();
    let t0 = Instant::now();
    st.decide_source(t0, &stale("c1"), true, 0, SETTLE, DEFER);
    // A settled rebuild fails at t1; backoff_until = t1 + backoff_delay(1) (60s).
    let t1 = t0 + SETTLE;
    st.record_rebuild(t1, &RebuildOutcome::Retryable("boom".into()), false);
    // t2 is settled (past t0+SETTLE) but still inside the 60s backoff window.
    let t2 = t1 + Duration::from_secs(30);
    let d = st.decide_source(t2, &stale("c1"), true, 0, SETTLE, DEFER);
    assert!(matches!(d, TickDecision::Skip(reason) if reason.contains("backing off")));
    // Once the backoff elapses, the same settled/clean/idle state rebuilds.
    let t3 = t1 + backoff_delay(1) + Duration::from_secs(1);
    assert_eq!(
        st.decide_source(t3, &stale("c1"), true, 0, SETTLE, DEFER),
        TickDecision::Rebuild {
            low_priority: false
        }
    );
}

#[test]
fn test_terminal_is_sticky_until_commit_changes() {
    let mut st = AutoUpdateState::new();
    let t0 = Instant::now();
    st.decide_source(t0, &stale("c1"), true, 0, SETTLE, DEFER);
    st.record_rebuild(t0, &RebuildOutcome::Terminal("commit mismatch (exit 4)".into()), false);
    assert!(st.terminal_reason.is_some());

    // Same commit, fully settled + clean + idle: still skipped (terminal).
    let later = t0 + SETTLE + Duration::from_secs(1);
    let d = st.decide_source(later, &stale("c1"), true, 0, SETTLE, DEFER);
    assert!(matches!(d, TickDecision::Skip(reason) if reason.contains("terminal")));

    // A NEW commit clears the terminal state (fresh attempt).
    st.decide_source(later, &stale("c2"), true, 0, SETTLE, DEFER);
    assert!(st.terminal_reason.is_none());
    assert_eq!(st.consecutive_failures, 0);
}

// ===================================================================
// Exit-code → RebuildOutcome mapping (#4053 exit 4/5 terminal)
// ===================================================================

fn write_fake_script(dir: &Path, body: &str) -> PathBuf {
    fs::create_dir_all(dir).unwrap();
    let path = dir.join("fake-update.sh");
    fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(&path).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&path, perms).unwrap();
    }
    path
}

#[test]
fn test_run_update_script_exit_0_is_success() {
    let tmp = tempfile::tempdir().unwrap();
    let s = write_fake_script(tmp.path(), "echo built; exit 0");
    assert_eq!(
        run_update_script(&s, tmp.path(), Duration::from_secs(10), false),
        RebuildOutcome::Success
    );
}

#[test]
fn test_run_update_script_exit_1_is_retryable() {
    let tmp = tempfile::tempdir().unwrap();
    let s = write_fake_script(tmp.path(), "echo compile error; exit 1");
    let o = run_update_script(&s, tmp.path(), Duration::from_secs(10), false);
    assert!(matches!(o, RebuildOutcome::Retryable(m) if m.contains("compile error")));
}

#[test]
fn test_run_update_script_exit_4_is_terminal() {
    let tmp = tempfile::tempdir().unwrap();
    let s = write_fake_script(tmp.path(), "echo commit mismatch; exit 4");
    let o = run_update_script(&s, tmp.path(), Duration::from_secs(10), false);
    assert!(matches!(o, RebuildOutcome::Terminal(m) if m.contains("commit mismatch")));
}

#[test]
fn test_run_update_script_exit_5_is_terminal() {
    let tmp = tempfile::tempdir().unwrap();
    let s = write_fake_script(tmp.path(), "exit 5");
    assert!(matches!(
        run_update_script(&s, tmp.path(), Duration::from_secs(10), false),
        RebuildOutcome::Terminal(_)
    ));
}

#[test]
fn test_run_update_script_timeout_is_retryable() {
    let tmp = tempfile::tempdir().unwrap();
    let s = write_fake_script(tmp.path(), "sleep 30");
    let o = run_update_script(&s, tmp.path(), Duration::from_millis(300), false);
    assert!(matches!(o, RebuildOutcome::Retryable(m) if m.contains("timed out")));
}

#[test]
fn test_run_update_script_spawn_failure_is_retryable() {
    let tmp = tempfile::tempdir().unwrap();
    let bogus = tmp.path().join("does-not-exist.sh");
    assert!(matches!(
        run_update_script(&bogus, tmp.path(), Duration::from_secs(10), false),
        RebuildOutcome::Retryable(_)
    ));
}

// ===================================================================
// Loop wiring — fake probe + trigger drive run_tick end to end
// ===================================================================

struct FakeProbe {
    check: UpdateCheck,
    tree_clean: Option<bool>,
    /// The paths `tree_dirty_paths()` reports (Issue #7608); `vec![]` at
    /// most call sites, which don't exercise the dirty-path detail.
    dirty_paths: Vec<String>,
    in_flight: usize,
    rebuild_outcome: RebuildOutcome,
    rebuild_calls: Arc<AtomicUsize>,
    /// How many of those rebuilds asked for the niced/low-priority build
    /// (the gate-4 deadline override, #4929).
    low_priority_calls: Arc<AtomicUsize>,
}

impl AutoUpdateProbe for FakeProbe {
    fn check(&self) -> UpdateCheck {
        self.check.clone()
    }
    fn is_tree_clean(&self) -> Option<bool> {
        self.tree_clean
    }
    fn tree_dirty_paths(&self) -> Vec<String> {
        self.dirty_paths.clone()
    }
    fn in_flight_sweeps(&self) -> usize {
        self.in_flight
    }
    fn rebuild(&mut self, low_priority: bool) -> RebuildOutcome {
        self.rebuild_calls.fetch_add(1, Ordering::SeqCst);
        if low_priority {
            self.low_priority_calls.fetch_add(1, Ordering::SeqCst);
        }
        self.rebuild_outcome.clone()
    }
}

struct FakeTrigger {
    accepted: bool,
    calls: Arc<AtomicUsize>,
}

impl DrainTrigger for FakeTrigger {
    fn trigger(&self) -> bool {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.accepted
    }
}

/// A settled, clean, idle, stale probe rolls exactly once and triggers a
/// drain — the full happy path through `run_tick`.
#[test]
fn test_run_tick_rolls_and_triggers_drain_when_settled() {
    let rebuild_calls = Arc::new(AtomicUsize::new(0));
    let trigger_calls = Arc::new(AtomicUsize::new(0));
    let mut probe = FakeProbe {
        check: stale("c1"),
        tree_clean: Some(true),
        dirty_paths: Vec::new(),
        in_flight: 0,
        rebuild_outcome: RebuildOutcome::Success,
        rebuild_calls: rebuild_calls.clone(),
        low_priority_calls: Arc::new(AtomicUsize::new(0)),
    };
    let trigger = FakeTrigger {
        accepted: true,
        calls: trigger_calls.clone(),
    };
    let status = AutoUpdateStatus::new(true);
    let mut state = AutoUpdateState::new();
    // A zero settle window makes the very first observed-stale tick settled.
    let settle = Duration::from_secs(0);

    run_tick(&mut state, &status, &mut probe, &trigger, settle, DEFER);
    assert_eq!(rebuild_calls.load(Ordering::SeqCst), 1);
    assert_eq!(trigger_calls.load(Ordering::SeqCst), 1);
    let snap = status.snapshot();
    assert!(snap.last_roll.is_some());
    assert_eq!(snap.consecutive_failures, 0);
}

/// Issue #6261: `commits_behind`/`hours_behind` are a purely diagnostic
/// signal (logged when they cross a warn threshold) — they never gate
/// `decide()`'s logic, so a probe reporting extreme lag rolls through
/// the exact same gates as one reporting none.
#[test]
fn test_run_tick_staleness_lag_does_not_affect_decide_gates() {
    let rebuild_calls = Arc::new(AtomicUsize::new(0));
    let trigger_calls = Arc::new(AtomicUsize::new(0));
    let mut probe = FakeProbe {
        check: stale_with_lag("c1", 500, 900),
        tree_clean: Some(true),
        dirty_paths: Vec::new(),
        in_flight: 0,
        rebuild_outcome: RebuildOutcome::Success,
        rebuild_calls: rebuild_calls.clone(),
        low_priority_calls: Arc::new(AtomicUsize::new(0)),
    };
    let trigger = FakeTrigger {
        accepted: true,
        calls: trigger_calls.clone(),
    };
    let status = AutoUpdateStatus::new(true);
    let mut state = AutoUpdateState::new();
    let settle = Duration::from_secs(0);

    run_tick(&mut state, &status, &mut probe, &trigger, settle, DEFER);
    assert_eq!(rebuild_calls.load(Ordering::SeqCst), 1);
    assert_eq!(trigger_calls.load(Ordering::SeqCst), 1);
}

/// Issue #6007 — while a roll is already armed (in particular one *retained*
/// across a refused deadline: dispatch paused, restart re-arming itself at
/// quiescence) the loop must not rebuild or re-trigger. The binary is already
/// provisioned, and a redundant `cargo build` would compete for CPU with the
/// very in-flight sweeps the pending roll is waiting on.
#[test]
fn test_run_tick_skips_while_a_roll_is_already_armed() {
    struct PendingRollTrigger {
        calls: Arc<AtomicUsize>,
    }
    impl DrainTrigger for PendingRollTrigger {
        fn trigger(&self) -> bool {
            self.calls.fetch_add(1, Ordering::SeqCst);
            true
        }
        fn roll_in_progress(&self) -> bool {
            true
        }
    }

    let rebuild_calls = Arc::new(AtomicUsize::new(0));
    let trigger_calls = Arc::new(AtomicUsize::new(0));
    let mut probe = FakeProbe {
        check: stale("c1"),
        tree_clean: Some(true),
        dirty_paths: Vec::new(),
        // Busy host — exactly the shape that made the roll go pending.
        in_flight: 3,
        rebuild_outcome: RebuildOutcome::Success,
        rebuild_calls: rebuild_calls.clone(),
        low_priority_calls: Arc::new(AtomicUsize::new(0)),
    };
    let trigger = PendingRollTrigger {
        calls: trigger_calls.clone(),
    };
    let status = AutoUpdateStatus::new(true);
    let mut state = AutoUpdateState::new();

    run_tick(&mut state, &status, &mut probe, &trigger, Duration::from_secs(0), DEFER);

    assert_eq!(rebuild_calls.load(Ordering::SeqCst), 0, "no redundant rebuild");
    assert_eq!(trigger_calls.load(Ordering::SeqCst), 0, "no redundant drain trigger");
    let snap = status.snapshot();
    assert!(
        snap.note
            .as_deref()
            .is_some_and(|n| n.contains("already armed")),
        "the skip must be explained in status, got: {:?}",
        snap.note
    );
}

#[test]
fn test_run_tick_none_never_rebuilds() {
    let rebuild_calls = Arc::new(AtomicUsize::new(0));
    let trigger_calls = Arc::new(AtomicUsize::new(0));
    let mut probe = FakeProbe {
        check: UpdateCheck {
            update_available: None,
            source_commit: None,
            commits_behind: None,
            hours_behind: None,
        },
        tree_clean: Some(true),
        dirty_paths: Vec::new(),
        in_flight: 0,
        rebuild_outcome: RebuildOutcome::Success,
        rebuild_calls: rebuild_calls.clone(),
        low_priority_calls: Arc::new(AtomicUsize::new(0)),
    };
    let trigger = FakeTrigger {
        accepted: true,
        calls: trigger_calls.clone(),
    };
    let status = AutoUpdateStatus::new(true);
    let mut state = AutoUpdateState::new();
    run_tick(&mut state, &status, &mut probe, &trigger, Duration::from_secs(0), DEFER);
    assert_eq!(rebuild_calls.load(Ordering::SeqCst), 0, "None must never rebuild");
    assert_eq!(trigger_calls.load(Ordering::SeqCst), 0);
}

#[test]
fn test_run_tick_dirty_tree_never_rebuilds() {
    let rebuild_calls = Arc::new(AtomicUsize::new(0));
    let mut probe = FakeProbe {
        check: stale("c1"),
        tree_clean: Some(false),
        dirty_paths: Vec::new(),
        in_flight: 0,
        rebuild_outcome: RebuildOutcome::Success,
        rebuild_calls: rebuild_calls.clone(),
        low_priority_calls: Arc::new(AtomicUsize::new(0)),
    };
    let trigger = FakeTrigger {
        accepted: true,
        calls: Arc::new(AtomicUsize::new(0)),
    };
    let status = AutoUpdateStatus::new(true);
    let mut state = AutoUpdateState::new();
    run_tick(&mut state, &status, &mut probe, &trigger, Duration::from_secs(0), DEFER);
    assert_eq!(rebuild_calls.load(Ordering::SeqCst), 0, "dirty tree must never rebuild");
}

/// Issue #7608: a dirty-tree refusal names the offending paths (first
/// three, plus a count) in the published note, so `loom-daemon health`
/// (which surfaces `auto_update_note` verbatim, #7584) can say exactly
/// what blocked the rebuild instead of a bare "dirty" with no detail.
#[test]
fn test_run_tick_dirty_tree_note_names_offending_paths() {
    let rebuild_calls = Arc::new(AtomicUsize::new(0));
    let mut probe = FakeProbe {
        check: stale("c1"),
        tree_clean: Some(false),
        dirty_paths: vec![
            "loom-daemon/src/foo.rs".to_string(),
            "Cargo.lock".to_string(),
        ],
        in_flight: 0,
        rebuild_outcome: RebuildOutcome::Success,
        rebuild_calls: rebuild_calls.clone(),
        low_priority_calls: Arc::new(AtomicUsize::new(0)),
    };
    let trigger = FakeTrigger {
        accepted: true,
        calls: Arc::new(AtomicUsize::new(0)),
    };
    let status = AutoUpdateStatus::new(true);
    let mut state = AutoUpdateState::new();
    run_tick(&mut state, &status, &mut probe, &trigger, Duration::from_secs(0), DEFER);
    assert_eq!(rebuild_calls.load(Ordering::SeqCst), 0, "dirty tree must never rebuild");
    let note = status.snapshot().note.expect("note published");
    assert!(note.contains("loom-daemon/src/foo.rs"), "note must name paths: {note}");
    assert!(note.contains("Cargo.lock"), "note must name paths: {note}");
    assert!(note.contains("(2)"), "note must include the dirty-path count: {note}");
}

#[test]
fn with_dirty_paths_empty_returns_reason_unchanged() {
    assert_eq!(with_dirty_paths("dirty", Vec::new()), "dirty");
}

#[test]
fn with_dirty_paths_shows_first_three_plus_remainder_count() {
    let paths: Vec<String> = ["a", "b", "c", "d", "e"]
        .iter()
        .map(|s| (*s).to_string())
        .collect();
    let note = with_dirty_paths("dirty", paths);
    assert!(note.contains("(5): a, b, c"), "got: {note}");
    assert!(note.contains("+2 more"), "got: {note}");
}

#[test]
fn test_run_tick_terminal_exit_not_retried_next_tick() {
    let rebuild_calls = Arc::new(AtomicUsize::new(0));
    let mut probe = FakeProbe {
        check: stale("c1"),
        tree_clean: Some(true),
        dirty_paths: Vec::new(),
        in_flight: 0,
        rebuild_outcome: RebuildOutcome::Terminal("commit mismatch (exit 4)".into()),
        rebuild_calls: rebuild_calls.clone(),
        low_priority_calls: Arc::new(AtomicUsize::new(0)),
    };
    let trigger = FakeTrigger {
        accepted: true,
        calls: Arc::new(AtomicUsize::new(0)),
    };
    let status = AutoUpdateStatus::new(true);
    let mut state = AutoUpdateState::new();
    let settle = Duration::from_secs(0);
    // Tick 1: rebuilds, hits terminal.
    run_tick(&mut state, &status, &mut probe, &trigger, settle, DEFER);
    assert_eq!(rebuild_calls.load(Ordering::SeqCst), 1);
    assert!(status.snapshot().terminal_reason.is_some());
    // Tick 2 (same commit): must NOT rebuild again.
    run_tick(&mut state, &status, &mut probe, &trigger, settle, DEFER);
    assert_eq!(rebuild_calls.load(Ordering::SeqCst), 1, "terminal must not retry same commit");
}

#[test]
fn test_run_tick_gate4_defers_rebuild_with_inflight_sweeps() {
    let rebuild_calls = Arc::new(AtomicUsize::new(0));
    let mut probe = FakeProbe {
        check: stale("c1"),
        tree_clean: Some(true),
        dirty_paths: Vec::new(),
        in_flight: 2,
        rebuild_outcome: RebuildOutcome::Success,
        rebuild_calls: rebuild_calls.clone(),
        low_priority_calls: Arc::new(AtomicUsize::new(0)),
    };
    let trigger = FakeTrigger {
        accepted: true,
        calls: Arc::new(AtomicUsize::new(0)),
    };
    let status = AutoUpdateStatus::new(true);
    let mut state = AutoUpdateState::new();
    run_tick(&mut state, &status, &mut probe, &trigger, Duration::from_secs(0), DEFER);
    assert_eq!(rebuild_calls.load(Ordering::SeqCst), 0, "in-flight sweeps must gate the build");
}

/// End-to-end #4929: a host whose in-flight count NEVER drops to zero still
/// converges. The first check defers (gate 4 intact); once the deferral
/// deadline elapses the loop rebuilds anyway — niced — the drain-and-restart
/// is triggered, and `last_roll` finally goes non-null.
#[test]
fn test_run_tick_saturated_host_eventually_rolls_after_defer_deadline() {
    let rebuild_calls = Arc::new(AtomicUsize::new(0));
    let low_priority_calls = Arc::new(AtomicUsize::new(0));
    let trigger_calls = Arc::new(AtomicUsize::new(0));
    let mut probe = FakeProbe {
        check: stale("c1"),
        tree_clean: Some(true),
        dirty_paths: Vec::new(),
        // Permanently saturated — the sweep count never reaches 0, which is
        // exactly what starved the updater on robb-STUDIO.
        in_flight: 13,
        rebuild_outcome: RebuildOutcome::Success,
        rebuild_calls: rebuild_calls.clone(),
        low_priority_calls: low_priority_calls.clone(),
    };
    let trigger = FakeTrigger {
        accepted: true,
        calls: trigger_calls.clone(),
    };
    let status = AutoUpdateStatus::new(true);
    let mut state = AutoUpdateState::new();
    // `run_tick` reads the real monotonic clock, so use a short deadline and
    // sleep past it. The FIRST tick can never fire the override (it starts
    // the deferral clock at its own `now`), so this is robust in both
    // directions regardless of machine speed.
    let settle = Duration::from_secs(0);
    let deadline = Duration::from_millis(50);

    run_tick(&mut state, &status, &mut probe, &trigger, settle, deadline);
    assert_eq!(
        rebuild_calls.load(Ordering::SeqCst),
        0,
        "first busy check must still defer (gate 4 intact)"
    );
    assert!(status.snapshot().last_roll.is_none());

    std::thread::sleep(Duration::from_millis(120));

    run_tick(&mut state, &status, &mut probe, &trigger, settle, deadline);
    assert_eq!(
        rebuild_calls.load(Ordering::SeqCst),
        1,
        "a permanently saturated host must rebuild once the deadline elapses (#4929)"
    );
    assert_eq!(
        low_priority_calls.load(Ordering::SeqCst),
        1,
        "the forced build must run at reduced priority, not compete head-on"
    );
    assert_eq!(trigger_calls.load(Ordering::SeqCst), 1, "the roll still goes through drain");
    let snap = status.snapshot();
    assert!(snap.last_roll.is_some(), "#4929 acceptance: last_roll must become non-null");
    assert!(
        snap.note.unwrap_or_default().contains("reduced priority"),
        "the forced build must be visible in `loom-daemon status`"
    );
}

// ===================================================================
// IpcDrainTrigger — the roll routes through #4090's drain primitive
// ===================================================================

/// The production trigger genuinely calls [`crate::ipc::handle_drain_request`]
/// (the #4090 drain path), not a bare restart: on an **unsupervised** host it
/// is refused (`accepted: false`) and — critically — dispatch is NOT paused
/// (`is_draining()` stays false), exactly the drain primitive's contract.
/// The supervised happy path (`is_draining()` true, `evaluate_drain_tick`
/// completing only at 0 in-flight) is covered by #4090's own ipc.rs tests;
/// exercising it here would `process::exit` the test runner.
#[tokio::test]
#[serial(loom_daemon_supervisor)]
async fn test_ipc_drain_trigger_routes_through_drain_primitive() {
    std::env::remove_var("LOOM_DAEMON_SUPERVISOR");
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().to_path_buf();
    // Force an empty workspace registry so count_in_flight_sweeps == 0 and
    // the only variable under test is the supervisor refusal.
    std::env::set_var(
        crate::workspace_registry::REGISTRY_PATH_ENV,
        root.join("no-such-workspaces.json"),
    );
    let bus = Arc::new(EventBus::new());
    let pool = Arc::new(WorkspacePool::new(bus.clone(), tokio::runtime::Handle::current()));
    let drain = Arc::new(DrainState::new());
    let trigger =
        IpcDrainTrigger::new(drain.clone(), pool, root, bus, tokio::runtime::Handle::current());

    let accepted = trigger.trigger();
    std::env::remove_var(crate::workspace_registry::REGISTRY_PATH_ENV);

    assert!(!accepted, "unsupervised host must refuse the drain (no bare restart fallback)");
    assert!(!drain.is_draining(), "a refused drain must not pause dispatch");
    assert_eq!(drain.generation(), 0, "a refused drain must not bump the drain generation");
}

// ===================================================================
// Global status handle
// ===================================================================

#[test]
fn test_global_status_defaults_when_unset() {
    // Not registering leaves the default (this may race other tests that
    // DO register, so only assert structural defaults on a fresh snapshot).
    let snap = AutoUpdateStatus::new(false).snapshot();
    assert!(!snap.enabled);
    assert!(snap.last_check.is_none());
}

// ===================================================================
// Artifact-first tick (Issue #7609)
// ===================================================================

/// Issue #8252 — the in-flight/build-stampede gate applies to the rebuild path
/// only. A sibling file (this one is over the file-size ratchet threshold); it
/// reuses the fixtures below via `use super::*`.
mod in_flight_gate;

const SHA_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const SHA_B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

/// A resolved artifact for `version`, with the published/installed
/// checksums and installed version spelled out.
fn artifact(
    version: &str,
    installed_version: Option<&str>,
    asset_sha: Option<&str>,
    installed_sha: Option<&str>,
) -> ArtifactInfo {
    ArtifactInfo {
        repo: "test-owner/test-repo".to_string(),
        tag: format!("v{version}"),
        version: version.to_string(),
        published_at: Some("2026-09-13T12:00:00Z".to_string()),
        asset_sha256: asset_sha.map(str::to_string),
        target: Some("aarch64-apple-darwin".to_string()),
        installed_version: installed_version.map(str::to_string),
        installed_sha256: installed_sha.map(str::to_string),
    }
}

fn resolved(info: ArtifactInfo) -> ArtifactResolution {
    ArtifactResolution::Resolved(info)
}

fn unresolved() -> ArtifactResolution {
    ArtifactResolution::Unresolved("no releases yet".to_string())
}

/// One tick's readings, bundled for [`AutoUpdateState::decide`].
fn inputs<'a>(
    artifact: &'a ArtifactResolution,
    check: &'a UpdateCheck,
    tree_clean: bool,
    in_flight: usize,
) -> TickInputs<'a> {
    TickInputs {
        artifact,
        check,
        tree_clean,
        in_flight,
    }
}

/// A state whose artifact-roll record lives in a throwaway dir, so the
/// convergence guard never reads or writes the ambient `~/.loom`.
fn state_with_record_dir(dir: &Path) -> AutoUpdateState {
    AutoUpdateState::new_with_record_path(Some(dir.join(ARTIFACT_ROLL_RECORD_FILE)))
}

// ---- version comparison -------------------------------------------

#[test]
fn test_compare_versions_orders_numerically_not_lexically() {
    use std::cmp::Ordering;
    assert_eq!(compare_versions("0.19.24", "0.19.21"), Ordering::Greater);
    // Lexically "0.19.9" > "0.19.24"; numerically it is not.
    assert_eq!(compare_versions("0.19.24", "0.19.9"), Ordering::Greater);
    assert_eq!(compare_versions("0.19.21", "0.19.21"), Ordering::Equal);
    assert_eq!(compare_versions("0.18.121", "0.19.0"), Ordering::Less);
    // A `v` prefix / trailing junk is stripped defensively, matching the
    // update script's own semver_compare.
    assert_eq!(compare_versions("v0.19.24", "0.19.24"), Ordering::Equal);
    // Missing components default to 0.
    assert_eq!(compare_versions("0.19", "0.19.0"), Ordering::Equal);
    assert_eq!(compare_versions("1", "0.99.99"), Ordering::Greater);
}

// ---- classification ------------------------------------------------

#[test]
fn test_classify_newer_artifact() {
    let verdict =
        classify_artifact(&artifact("0.19.24", Some("0.19.21"), Some(SHA_A), Some(SHA_B)));
    assert_eq!(
        verdict,
        ArtifactVerdict::Newer {
            installed: Some("0.19.21".to_string()),
            artifact: "0.19.24".to_string()
        }
    );
}

#[test]
fn test_classify_equal_version_differing_sha_is_convergence() {
    let verdict =
        classify_artifact(&artifact("0.19.24", Some("0.19.24"), Some(SHA_A), Some(SHA_B)));
    assert_eq!(
        verdict,
        ArtifactVerdict::ShaDiffers {
            version: "0.19.24".to_string(),
            asset_sha256: SHA_A.to_string(),
            installed_sha256: SHA_B.to_string(),
        }
    );
}

#[test]
fn test_classify_equal_version_same_sha_is_up_to_date() {
    let verdict =
        classify_artifact(&artifact("0.19.24", Some("0.19.24"), Some(SHA_A), Some(SHA_A)));
    assert!(matches!(verdict, ArtifactVerdict::UpToDate { .. }));
    // Case-insensitively — a hex digest's case is not identity.
    let upper = SHA_A.to_uppercase();
    let verdict =
        classify_artifact(&artifact("0.19.24", Some("0.19.24"), Some(&upper), Some(SHA_A)));
    assert!(matches!(verdict, ArtifactVerdict::UpToDate { .. }));
}

#[test]
fn test_classify_missing_checksum_is_up_to_date_not_a_fetch_loop() {
    // Equal versions with either checksum unknown must NOT fetch: a wrong
    // "differs" would re-fetch and restart on every tick forever.
    assert!(matches!(
        classify_artifact(&artifact("0.19.24", Some("0.19.24"), None, Some(SHA_A))),
        ArtifactVerdict::UpToDate { .. }
    ));
    assert!(matches!(
        classify_artifact(&artifact("0.19.24", Some("0.19.24"), Some(SHA_A), None)),
        ArtifactVerdict::UpToDate { .. }
    ));
}

#[test]
fn test_classify_no_installed_version_is_newer() {
    let verdict = classify_artifact(&artifact("0.19.24", None, Some(SHA_A), None));
    assert_eq!(
        verdict,
        ArtifactVerdict::Newer {
            installed: None,
            artifact: "0.19.24".to_string()
        }
    );
}

// ---- decide(): the AC decision matrix -------------------------------

#[test]
fn test_decide_newer_artifact_fetches_and_never_rebuilds() {
    let tmp = tempfile::tempdir().unwrap();
    let mut st = state_with_record_dir(tmp.path());
    let now = Instant::now();
    // Deliberately hostile source-side inputs: a DIRTY tree and an
    // undecidable staleness — neither may block the artifact path.
    let art = resolved(artifact("0.19.24", Some("0.19.21"), Some(SHA_A), Some(SHA_B)));
    let undecidable = UpdateCheck {
        update_available: None,
        source_commit: None,
        commits_behind: None,
        hours_behind: None,
    };
    let d = st.decide(now, &inputs(&art, &undecidable, false, 0), Duration::from_secs(0), DEFER);
    match d {
        TickDecision::FetchArtifact {
            version,
            why,
            low_priority,
            ..
        } => {
            assert_eq!(version, "0.19.24");
            assert!(why.contains("artifact 0.19.24 > installed 0.19.21"), "why: {why}");
            assert!(!low_priority);
        }
        other => panic!("expected FetchArtifact, got {other:?}"),
    }
}

#[test]
fn test_decide_equal_version_differing_sha_fetches() {
    let tmp = tempfile::tempdir().unwrap();
    let mut st = state_with_record_dir(tmp.path());
    let art = resolved(artifact("0.19.24", Some("0.19.24"), Some(SHA_A), Some(SHA_B)));
    let d = st.decide(
        Instant::now(),
        &inputs(&art, &stale("c1"), true, 0),
        Duration::from_secs(0),
        DEFER,
    );
    match d {
        TickDecision::FetchArtifact { why, .. } => {
            assert!(why.contains("sha differs"), "why: {why}");
        }
        other => panic!("expected FetchArtifact, got {other:?}"),
    }
}

#[test]
fn test_decide_equal_version_same_sha_skips() {
    let tmp = tempfile::tempdir().unwrap();
    let mut st = state_with_record_dir(tmp.path());
    // A stale source checkout must NOT produce a rebuild once an artifact
    // resolves and says the installed binary is already the released one.
    let art = resolved(artifact("0.19.24", Some("0.19.24"), Some(SHA_A), Some(SHA_A)));
    let d = st.decide(
        Instant::now(),
        &inputs(&art, &stale("c1"), true, 0),
        Duration::from_secs(0),
        DEFER,
    );
    match d {
        TickDecision::Skip(reason) => {
            assert!(reason.contains("sha matches"), "reason: {reason}");
            assert!(reason.contains("up to date"), "reason: {reason}");
        }
        other => panic!("expected Skip, got {other:?}"),
    }
}

#[test]
fn test_decide_no_artifact_falls_back_to_stale_source_rebuild() {
    let tmp = tempfile::tempdir().unwrap();
    let mut st = state_with_record_dir(tmp.path());
    let d = st.decide(
        Instant::now(),
        &inputs(&unresolved(), &stale("c1"), true, 0),
        Duration::from_secs(0),
        DEFER,
    );
    assert!(
        matches!(
            d,
            TickDecision::Rebuild {
                low_priority: false
            }
        ),
        "got {d:?}"
    );
}

#[test]
fn test_decide_no_artifact_dirty_source_still_refuses() {
    let tmp = tempfile::tempdir().unwrap();
    let mut st = state_with_record_dir(tmp.path());
    let d = st.decide(
        Instant::now(),
        &inputs(&unresolved(), &stale("c1"), false, 0),
        Duration::from_secs(0),
        DEFER,
    );
    match d {
        TickDecision::Skip(reason) => {
            assert!(reason.ends_with(DIRTY_TREE_REASON), "reason: {reason}");
            // The log line must name which path was taken and why the
            // artifact path was not (Issue #7609's logging AC).
            assert!(
                reason.starts_with("no artifact (no releases yet) → source path:"),
                "reason: {reason}"
            );
        }
        other => panic!("expected Skip, got {other:?}"),
    }
}

#[test]
fn test_decide_artifact_respects_settle_window() {
    let tmp = tempfile::tempdir().unwrap();
    let mut st = state_with_record_dir(tmp.path());
    let settle = Duration::from_secs(600);
    let base = Instant::now();
    let info = resolved(artifact("0.19.24", Some("0.19.21"), Some(SHA_A), Some(SHA_B)));
    let first = st.decide(base, &inputs(&info, &stale("c1"), true, 0), settle, DEFER);
    assert!(
        matches!(first, TickDecision::Skip(ref r) if r.contains("settle")),
        "got {first:?}"
    );
    let later = base + settle + Duration::from_secs(1);
    let second = st.decide(later, &inputs(&info, &stale("c1"), true, 0), settle, DEFER);
    assert!(matches!(second, TickDecision::FetchArtifact { .. }), "got {second:?}");
}

#[test]
fn test_decide_artifact_honors_backoff_and_terminal() {
    let tmp = tempfile::tempdir().unwrap();
    let mut st = state_with_record_dir(tmp.path());
    let settle = Duration::from_secs(0);
    let base = Instant::now();
    let info = resolved(artifact("0.19.24", Some("0.19.21"), Some(SHA_A), Some(SHA_B)));

    assert!(matches!(
        st.decide(base, &inputs(&info, &stale("c1"), true, 0), settle, DEFER),
        TickDecision::FetchArtifact { .. }
    ));
    // A retryable fetch failure backs off exactly as a rebuild failure does.
    let note = st.record_artifact_roll(
        base,
        &RebuildOutcome::Retryable("download failed".to_string()),
        false,
        &artifact("0.19.24", Some("0.19.21"), Some(SHA_A), Some(SHA_B)),
    );
    assert!(note.starts_with("artifact fetch failed"), "note: {note}");
    let d = st.decide(base, &inputs(&info, &stale("c1"), true, 0), settle, DEFER);
    assert!(matches!(d, TickDecision::Skip(ref r) if r.contains("backing off")), "got {d:?}");

    // A terminal failure is sticky until the target changes.
    st.record_artifact_roll(
        base,
        &RebuildOutcome::Terminal("verification failed".to_string()),
        false,
        &artifact("0.19.24", Some("0.19.21"), Some(SHA_A), Some(SHA_B)),
    );
    let d = st.decide(base, &inputs(&info, &stale("c1"), true, 0), settle, DEFER);
    assert!(matches!(d, TickDecision::Skip(ref r) if r.contains("terminal")), "got {d:?}");
    // A NEWER release clears it — a new artifact is a fresh attempt.
    let newer = resolved(artifact("0.19.25", Some("0.19.21"), Some(SHA_B), Some(SHA_A)));
    let d = st.decide(base, &inputs(&newer, &stale("c1"), true, 0), settle, DEFER);
    assert!(matches!(d, TickDecision::FetchArtifact { .. }), "got {d:?}");
}

// ---- the convergence guard (no fetch/restart loop) -------------------

#[test]
fn test_recorded_roll_suppresses_a_repeat_same_version_convergence() {
    let tmp = tempfile::tempdir().unwrap();
    let mut st = state_with_record_dir(tmp.path());
    let now = Instant::now();
    let info = artifact("0.19.24", Some("0.19.24"), Some(SHA_A), Some(SHA_B));

    // First observation: converge onto the published bytes.
    assert!(matches!(
        st.decide(
            now,
            &inputs(&resolved(info.clone()), &stale("c1"), true, 0),
            Duration::from_secs(0),
            DEFER
        ),
        TickDecision::FetchArtifact { .. }
    ));
    st.record_artifact_roll(now, &RebuildOutcome::Success, true, &info);

    // The host re-signed the binary at provision time, so its sha STILL
    // differs from the release's. Without the record this would fetch (and
    // restart) again every tick, forever.
    let d = st.decide(
        now,
        &inputs(&resolved(info.clone()), &stale("c1"), true, 0),
        Duration::from_secs(0),
        DEFER,
    );
    match d {
        TickDecision::Skip(reason) => {
            assert!(reason.contains("already installed from this release"), "reason: {reason}");
            assert!(reason.contains("not re-fetching"), "reason: {reason}");
        }
        other => panic!("expected Skip, got {other:?}"),
    }

    // The record survives a process restart (it is on disk, not in memory).
    let mut restarted = state_with_record_dir(tmp.path());
    assert!(matches!(
        restarted.decide(
            now,
            &inputs(&resolved(info), &stale("c1"), true, 0),
            Duration::from_secs(0),
            DEFER
        ),
        TickDecision::Skip(_)
    ));
}

#[test]
fn test_recorded_roll_does_not_suppress_a_republished_release() {
    let tmp = tempfile::tempdir().unwrap();
    let mut st = state_with_record_dir(tmp.path());
    let now = Instant::now();
    st.record_artifact_roll(
        now,
        &RebuildOutcome::Success,
        true,
        &artifact("0.19.24", Some("0.19.24"), Some(SHA_A), Some(SHA_A)),
    );
    // Same version, but the release now publishes DIFFERENT bytes — a
    // re-cut release must still converge.
    let republished = resolved(artifact("0.19.24", Some("0.19.24"), Some(SHA_B), Some(SHA_A)));
    let d =
        st.decide(now, &inputs(&republished, &stale("c1"), true, 0), Duration::from_secs(0), DEFER);
    assert!(matches!(d, TickDecision::FetchArtifact { .. }), "got {d:?}");
}

#[test]
fn test_artifact_roll_record_round_trips_on_disk() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("nested").join(ARTIFACT_ROLL_RECORD_FILE);
    let record = ArtifactRollRecord {
        version: "0.19.24".to_string(),
        asset_sha256: SHA_A.to_string(),
        rolled_at: Utc::now(),
    };
    store_artifact_roll_record(Some(&path), &record);
    assert_eq!(load_artifact_roll_record(Some(&path)), Some(record));
    // A corrupt record soft-fails to None rather than wedging the loop.
    std::fs::write(&path, "{not json").unwrap();
    assert_eq!(load_artifact_roll_record(Some(&path)), None);
    assert_eq!(load_artifact_roll_record(None), None);
}

// ---- run_tick end to end on the artifact path ------------------------

/// A probe that resolves a scripted artifact and records whether the tick
/// fetched or rebuilt. Separate from [`FakeProbe`] so the existing
/// source-path tests keep exercising the no-artifact default verbatim.
struct ArtifactFakeProbe {
    artifact: ArtifactResolution,
    check: UpdateCheck,
    tree_clean: Option<bool>,
    in_flight: usize,
    fetch_outcome: RebuildOutcome,
    fetch_calls: Arc<AtomicUsize>,
    rebuild_calls: Arc<AtomicUsize>,
}

impl AutoUpdateProbe for ArtifactFakeProbe {
    fn resolve_artifact(&self) -> ArtifactResolution {
        self.artifact.clone()
    }
    fn fetch_artifact(&mut self, _low_priority: bool) -> RebuildOutcome {
        self.fetch_calls.fetch_add(1, Ordering::SeqCst);
        self.fetch_outcome.clone()
    }
    fn check(&self) -> UpdateCheck {
        self.check.clone()
    }
    fn is_tree_clean(&self) -> Option<bool> {
        self.tree_clean
    }
    fn in_flight_sweeps(&self) -> usize {
        self.in_flight
    }
    fn rebuild(&mut self, _low_priority: bool) -> RebuildOutcome {
        self.rebuild_calls.fetch_add(1, Ordering::SeqCst);
        RebuildOutcome::Success
    }
}

#[test]
fn test_run_tick_fetches_the_artifact_and_never_rebuilds() {
    let fetch_calls = Arc::new(AtomicUsize::new(0));
    let rebuild_calls = Arc::new(AtomicUsize::new(0));
    let trigger_calls = Arc::new(AtomicUsize::new(0));
    let mut probe = ArtifactFakeProbe {
        artifact: resolved(artifact("0.19.24", Some("0.19.21"), Some(SHA_A), Some(SHA_B))),
        // The exact fleet shape this issue exists for: no source checkout
        // AND (therefore) an unprovable-clean tree.
        check: UpdateCheck {
            update_available: None,
            source_commit: None,
            commits_behind: None,
            hours_behind: None,
        },
        tree_clean: None,
        in_flight: 0,
        fetch_outcome: RebuildOutcome::Success,
        fetch_calls: fetch_calls.clone(),
        rebuild_calls: rebuild_calls.clone(),
    };
    let trigger = FakeTrigger {
        accepted: true,
        calls: trigger_calls.clone(),
    };
    let status = AutoUpdateStatus::new(true);
    let tmp = tempfile::tempdir().unwrap();
    let mut state = state_with_record_dir(tmp.path());

    run_tick(&mut state, &status, &mut probe, &trigger, Duration::from_secs(0), DEFER);

    assert_eq!(fetch_calls.load(Ordering::SeqCst), 1, "the artifact must be fetched");
    assert_eq!(rebuild_calls.load(Ordering::SeqCst), 0, "no cargo build on the artifact path");
    assert_eq!(trigger_calls.load(Ordering::SeqCst), 1, "a successful fetch triggers the drain");
    let snap = status.snapshot();
    assert!(snap.last_roll.is_some());
    assert_eq!(snap.artifact_version.as_deref(), Some("0.19.24"));
    assert_eq!(snap.artifact_published_at.as_deref(), Some("2026-09-13T12:00:00Z"));
    assert!(
        snap.note
            .as_deref()
            .unwrap_or_default()
            .contains("fetched release artifact"),
        "note: {:?}",
        snap.note
    );
    // The roll was recorded, so a re-signed binary cannot re-trigger it.
    assert!(tmp.path().join(ARTIFACT_ROLL_RECORD_FILE).exists());
}

#[test]
fn test_run_tick_up_to_date_artifact_does_not_rebuild_a_stale_checkout() {
    let fetch_calls = Arc::new(AtomicUsize::new(0));
    let rebuild_calls = Arc::new(AtomicUsize::new(0));
    let mut probe = ArtifactFakeProbe {
        artifact: resolved(artifact("0.19.24", Some("0.19.24"), Some(SHA_A), Some(SHA_A))),
        // A stale, clean source checkout — pre-#7609 this would rebuild.
        check: stale("c1"),
        tree_clean: Some(true),
        in_flight: 0,
        fetch_outcome: RebuildOutcome::Success,
        fetch_calls: fetch_calls.clone(),
        rebuild_calls: rebuild_calls.clone(),
    };
    let trigger = FakeTrigger {
        accepted: true,
        calls: Arc::new(AtomicUsize::new(0)),
    };
    let status = AutoUpdateStatus::new(true);
    let tmp = tempfile::tempdir().unwrap();
    let mut state = state_with_record_dir(tmp.path());

    run_tick(&mut state, &status, &mut probe, &trigger, Duration::from_secs(0), DEFER);

    assert_eq!(fetch_calls.load(Ordering::SeqCst), 0);
    assert_eq!(rebuild_calls.load(Ordering::SeqCst), 0);
    let snap = status.snapshot();
    assert_eq!(snap.artifact_version.as_deref(), Some("0.19.24"));
    assert!(
        snap.note
            .as_deref()
            .unwrap_or_default()
            .contains("up to date"),
        "note: {:?}",
        snap.note
    );
}

#[test]
fn test_run_tick_without_an_artifact_still_rebuilds_from_source() {
    let fetch_calls = Arc::new(AtomicUsize::new(0));
    let rebuild_calls = Arc::new(AtomicUsize::new(0));
    let mut probe = ArtifactFakeProbe {
        artifact: unresolved(),
        check: stale("c1"),
        tree_clean: Some(true),
        in_flight: 0,
        fetch_outcome: RebuildOutcome::Success,
        fetch_calls: fetch_calls.clone(),
        rebuild_calls: rebuild_calls.clone(),
    };
    let trigger = FakeTrigger {
        accepted: true,
        calls: Arc::new(AtomicUsize::new(0)),
    };
    let status = AutoUpdateStatus::new(true);
    let tmp = tempfile::tempdir().unwrap();
    let mut state = state_with_record_dir(tmp.path());

    run_tick(&mut state, &status, &mut probe, &trigger, Duration::from_secs(0), DEFER);

    assert_eq!(fetch_calls.load(Ordering::SeqCst), 0);
    assert_eq!(rebuild_calls.load(Ordering::SeqCst), 1, "the source path is preserved verbatim");
    assert_eq!(status.snapshot().artifact_version, None);
}

#[test]
fn test_run_tick_fetch_failure_backs_off_without_falling_back_to_a_build() {
    let fetch_calls = Arc::new(AtomicUsize::new(0));
    let rebuild_calls = Arc::new(AtomicUsize::new(0));
    let mut probe = ArtifactFakeProbe {
        artifact: resolved(artifact("0.19.24", Some("0.19.21"), Some(SHA_A), Some(SHA_B))),
        check: stale("c1"),
        tree_clean: Some(true),
        in_flight: 0,
        fetch_outcome: RebuildOutcome::Retryable("exit 1: no usable release artifact".to_string()),
        fetch_calls: fetch_calls.clone(),
        rebuild_calls: rebuild_calls.clone(),
    };
    let trigger = FakeTrigger {
        accepted: true,
        calls: Arc::new(AtomicUsize::new(0)),
    };
    let status = AutoUpdateStatus::new(true);
    let tmp = tempfile::tempdir().unwrap();
    let mut state = state_with_record_dir(tmp.path());

    run_tick(&mut state, &status, &mut probe, &trigger, Duration::from_secs(0), DEFER);
    // A failed fetch must NOT silently become a source build — the tick
    // backs off and retries the artifact path instead.
    assert_eq!(fetch_calls.load(Ordering::SeqCst), 1);
    assert_eq!(rebuild_calls.load(Ordering::SeqCst), 0);
    let snap = status.snapshot();
    assert_eq!(snap.consecutive_failures, 1);
    assert!(snap.backoff_secs.is_some());
    assert!(
        !tmp.path().join(ARTIFACT_ROLL_RECORD_FILE).exists(),
        "a failed roll records nothing"
    );
}

// ---- script-root resolution (the no-source-checkout host) -------------

/// Run `f` with `LOOM_DAEMON_DEFAULTS_DIR` set to `value` (or unset if
/// `None`), restoring the prior value afterward. A copy of
/// `init::git::tests::with_machine_defaults_env` (that module's test helpers
/// are private) touching the same env var
/// [`crate::init::git::MACHINE_DEFAULTS_ENV`] that
/// `ScriptAutoUpdateProbe::candidate_roots`'s third fallback reads (Issue
/// #7964) — every caller below carries the matching
/// `#[serial(loom_daemon_defaults_dir)]` key so the two test modules'
/// mutations of this process-global var cannot interleave.
fn with_machine_defaults_env<T>(value: Option<&str>, f: impl FnOnce() -> T) -> T {
    let prev = std::env::var(crate::init::git::MACHINE_DEFAULTS_ENV).ok();
    match value {
        Some(v) => std::env::set_var(crate::init::git::MACHINE_DEFAULTS_ENV, v),
        None => std::env::remove_var(crate::init::git::MACHINE_DEFAULTS_ENV),
    }
    let result = f();
    match prev {
        Some(p) => std::env::set_var(crate::init::git::MACHINE_DEFAULTS_ENV, p),
        None => std::env::remove_var(crate::init::git::MACHINE_DEFAULTS_ENV),
    }
    result
}

#[tokio::test]
#[serial(loom_daemon_defaults_dir)]
async fn test_script_root_falls_back_to_the_workspace_root() {
    // The host shape this issue exists for: no build-time source checkout
    // resolvable, but the daemon's own workspace root has the script. The
    // machine-level mirror candidate is disabled so a host with a real
    // `~/.local/share/loom-daemon/defaults` mirror cannot mask the assertion.
    with_machine_defaults_env(Some(""), || {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        std::fs::create_dir_all(root.join(".loom/scripts/cli")).unwrap();
        std::fs::write(root.join(".loom/scripts/cli/loom-daemon-update.sh"), "#!/bin/sh\n")
            .unwrap();
        let bus = Arc::new(EventBus::new());
        let pool = Arc::new(WorkspacePool::new(bus, tokio::runtime::Handle::current()));
        let mut probe = ScriptAutoUpdateProbe::new(pool, root.clone());
        probe.source_root = None;
        assert_eq!(probe.script_root(), Some(root));
    });
}

#[tokio::test]
#[serial(loom_daemon_defaults_dir)]
async fn test_script_root_is_none_without_any_script() {
    // Disable the mirror candidate too, else a host that actually has one
    // provisioned (e.g. any dev machine running `loom update`) would make
    // this "nothing resolves" assertion flaky.
    with_machine_defaults_env(Some(""), || {
        let tmp = tempfile::tempdir().unwrap();
        let bus = Arc::new(EventBus::new());
        let pool = Arc::new(WorkspacePool::new(bus, tokio::runtime::Handle::current()));
        let mut probe = ScriptAutoUpdateProbe::new(pool, tmp.path().to_path_buf());
        probe.source_root = None;
        assert_eq!(probe.script_root(), None);
        // …and a probe with no script resolves no artifact rather than
        // erroring, naming every candidate root tried.
        match probe.resolve_artifact() {
            ArtifactResolution::Unresolved(reason) => {
                assert!(reason.contains(&tmp.path().display().to_string()));
            }
            other => panic!("expected Unresolved, got {other:?}"),
        }
    });
}

#[tokio::test]
#[serial(loom_daemon_defaults_dir)]
async fn test_script_root_falls_back_to_the_machine_level_mirror() {
    // Neither the source checkout nor the workspace root has the script, but
    // the machine-level mirror (Issue #7964's third candidate) does — the
    // exact host shape this issue exists for: a stale in-repo script copy
    // that predates `--resolve-json` sitting alongside an up-to-date mirror
    // maintained by `scripts/install-loom.sh` / `loom update`.
    let mirror = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(mirror.path().join(".loom/scripts/cli")).unwrap();
    std::fs::write(
        mirror
            .path()
            .join(".loom/scripts/cli/loom-daemon-update.sh"),
        "#!/bin/sh\n",
    )
    .unwrap();

    with_machine_defaults_env(Some(mirror.path().to_str().unwrap()), || {
        let no_script = tempfile::tempdir().unwrap();
        let bus = Arc::new(EventBus::new());
        let pool = Arc::new(WorkspacePool::new(bus, tokio::runtime::Handle::current()));
        let mut probe = ScriptAutoUpdateProbe::new(pool, no_script.path().to_path_buf());
        probe.source_root = None;
        assert_eq!(probe.script_root(), Some(mirror.path().to_path_buf()));
    });
}
