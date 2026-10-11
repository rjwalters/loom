//! Issue #10720: detecting a restart-required config change against the boot
//! snapshot. Pure: temp files and a [`Tracker`] of the test's own, never the
//! process-wide one.

use std::path::{Path, PathBuf};

use chrono::{TimeZone, Utc};
use serde_json::json;

use super::*;

struct Host {
    _dir: tempfile::TempDir,
    machine: PathBuf,
    local: PathBuf,
}

impl Host {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let machine = dir.path().join("defaults.json");
        let local = dir.path().join("local.json");
        Self {
            _dir: dir,
            machine,
            local,
        }
    }

    fn tiers(&self) -> Vec<TierSnapshot> {
        read_tiers(Some(&self.machine), &self.local)
    }
}

fn write(path: &Path, value: &serde_json::Value) {
    std::fs::write(path, serde_json::to_vec_pretty(value).unwrap()).unwrap();
}

fn at(min: u32) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 10, 11, 12, min, 0).unwrap()
}

fn booted(host: &Host) -> Tracker {
    Tracker::booted(host.tiers())
}

#[test]
fn no_change_is_no_demand() {
    let host = Host::new();
    write(&host.machine, &json!({"autonomous": {"hostBreaker": {"enabled": true}}}));
    let mut t = booted(&host);
    assert_eq!(t.observe(&host.tiers(), Mode::Write, at(0)), Transition::Unchanged);
    assert_eq!(t.demand(), None);
}

#[test]
fn one_restart_required_change_raises_a_demand() {
    let host = Host::new();
    write(&host.machine, &json!({"autonomous": {"hostBreaker": {"enabled": true}}}));
    let mut t = booted(&host);
    write(&host.machine, &json!({"autonomous": {"hostBreaker": {"enabled": false}}}));
    let Transition::Raised(d) = t.observe(&host.tiers(), Mode::Write, at(1)) else {
        panic!("a restart-required change raises a demand");
    };
    assert_eq!(d.paths, vec!["autonomous.hostBreaker.enabled".to_string()]);
    assert!(d.armable);
    assert_eq!(d.first_seen, at(1));
    assert_eq!(t.demand(), Some(&d));
    // The next pass with nothing new is quiet, and keeps when it was first seen.
    assert_eq!(t.observe(&host.tiers(), Mode::Write, at(5)), Transition::Unchanged);
    assert_eq!(t.demand().unwrap().first_seen, at(1));
}

#[test]
fn a_live_or_annotation_only_change_is_no_demand() {
    let host = Host::new();
    write(
        &host.machine,
        &json!({"autonomous": {"roleRunner": {"enabled": false}}, "fleet": {"_sync_note": "a"}}),
    );
    let mut t = booted(&host);
    write(
        &host.machine,
        &json!({
            "autonomous": {"roleRunner": {"enabled": true}, "workFinder": {"maxConcurrent": 4}},
            "fleet": {"_sync_note": "b"},
            "_comment": "new"
        }),
    );
    assert_eq!(t.observe(&host.tiers(), Mode::Write, at(1)), Transition::Unchanged);
    assert_eq!(t.demand(), None);
}

/// A block added with only live knobs in it is classified knob by knob, not
/// by the block's own (restart-required) entry.
#[test]
fn an_added_block_is_classified_by_its_leaves() {
    let host = Host::new();
    write(&host.machine, &json!({"autonomous": {}}));
    let boot = host.tiers();
    write(
        &host.machine,
        &json!({"autonomous": {"workFinder": {"maxConcurrent": 4, "extraSkipLabels": ["x"]}}}),
    );
    let changes = compare(&boot, &host.tiers());
    assert!(changes.restart_required.is_empty(), "{changes:?}");
    assert_eq!(changes.live.len(), 2, "{changes:?}");
    // The same block added with a restart-required knob in it is.
    write(&host.machine, &json!({"autonomous": {"workFinder": {"enabled": true}}}));
    let changes = compare(&boot, &host.tiers());
    assert_eq!(changes.restart_required, vec!["autonomous.workFinder.enabled".to_string()]);
}

#[test]
fn a_change_then_revert_clears_the_demand() {
    let host = Host::new();
    let a = json!({"observability": {"enabled": false}});
    write(&host.machine, &a);
    let mut t = booted(&host);
    write(&host.machine, &json!({"observability": {"enabled": true}}));
    assert!(matches!(t.observe(&host.tiers(), Mode::Write, at(1)), Transition::Raised(_)));
    write(&host.machine, &a);
    assert_eq!(t.observe(&host.tiers(), Mode::Write, at(2)), Transition::Cleared);
    assert_eq!(t.demand(), None);
    assert_eq!(t.observe(&host.tiers(), Mode::Write, at(3)), Transition::Unchanged);
}

#[test]
fn a_file_missing_at_boot_and_present_later_is_restart_required_as_a_whole() {
    let host = Host::new();
    write(&host.machine, &json!({}));
    let mut t = booted(&host);
    write(&host.local, &json!({"autonomous": {"roleRunner": {"enabled": true}}}));
    let Transition::Raised(d) = t.observe(&host.tiers(), Mode::Write, at(1)) else {
        panic!("a new tier file is a whole-file change");
    };
    assert_eq!(d.paths, vec!["host-local tier (whole file)".to_string()]);
}

#[test]
fn a_file_that_becomes_unparseable_is_restart_required_and_stays_one_demand() {
    let host = Host::new();
    write(&host.machine, &json!({"forge": {"githubApp": {"appId": 1}}}));
    let mut t = booted(&host);
    std::fs::write(&host.machine, b"{ not json").unwrap();
    let Transition::Raised(d) = t.observe(&host.tiers(), Mode::Write, at(1)) else {
        panic!("an unparseable tier is a whole-file change");
    };
    assert_eq!(d.paths, vec!["machine tier (whole file)".to_string()]);
    // The same broken bytes on the next pass are the same demand.
    assert_eq!(t.observe(&host.tiers(), Mode::Write, at(2)), Transition::Unchanged);
}

#[test]
fn an_unparseable_file_unchanged_since_boot_is_no_change() {
    let host = Host::new();
    std::fs::write(&host.machine, b"{ broken").unwrap();
    let mut t = booted(&host);
    assert_eq!(t.observe(&host.tiers(), Mode::Write, at(1)), Transition::Unchanged);
}

#[test]
fn changes_in_both_tiers_are_one_demand() {
    let host = Host::new();
    write(&host.machine, &json!({"safehouse": {"enabled": false}}));
    write(&host.local, &json!({"autonomous": {"worktreeReaper": {"enabled": false}}}));
    let mut t = booted(&host);
    write(&host.machine, &json!({"safehouse": {"enabled": true}}));
    write(&host.local, &json!({"autonomous": {"worktreeReaper": {"enabled": true}}}));
    let Transition::Raised(d) = t.observe(&host.tiers(), Mode::Write, at(1)) else {
        panic!("two changes are one demand");
    };
    assert_eq!(
        d.paths,
        vec![
            "safehouse.enabled".to_string(),
            "autonomous.worktreeReaper.enabled".to_string()
        ]
    );
}

#[test]
fn the_fingerprint_changes_when_a_value_changes_again() {
    let host = Host::new();
    write(&host.machine, &json!({"autonomous": {"ciTelemetry": {"intervalSecs": 60}}}));
    let mut t = booted(&host);
    write(&host.machine, &json!({"autonomous": {"ciTelemetry": {"intervalSecs": 120}}}));
    let Transition::Raised(first) = t.observe(&host.tiers(), Mode::Write, at(1)) else {
        panic!();
    };
    write(&host.machine, &json!({"autonomous": {"ciTelemetry": {"intervalSecs": 180}}}));
    let Transition::Raised(second) = t.observe(&host.tiers(), Mode::Write, at(2)) else {
        panic!("a new value on the same path is a new fingerprint");
    };
    assert_eq!(first.paths, second.paths);
    assert_ne!(first.fingerprint, second.fingerprint);
    assert_eq!(second.first_seen, at(2));
}

#[test]
fn a_check_mode_host_records_but_cannot_arm() {
    let host = Host::new();
    write(&host.machine, &json!({"fleet": {"syncIntervalSecs": 300}}));
    let mut t = booted(&host);
    write(&host.machine, &json!({"fleet": {"syncIntervalSecs": 600}}));
    let Transition::Raised(d) = t.observe(&host.tiers(), Mode::Check, at(1)) else {
        panic!();
    };
    assert!(!d.armable);
    assert!(describe(&d).contains("nothing is armed"), "{}", describe(&d));
}

/// After the restart, the new process snapshots the files as they now are, so
/// it has no demand and arms nothing.
#[test]
fn a_fresh_process_has_no_demand() {
    let host = Host::new();
    write(&host.machine, &json!({"autonomous": {"hostBreaker": {"enabled": true}}}));
    let mut old = booted(&host);
    write(&host.machine, &json!({"autonomous": {"hostBreaker": {"enabled": false}}}));
    assert!(matches!(old.observe(&host.tiers(), Mode::Write, at(1)), Transition::Raised(_)));
    let mut fresh = booted(&host);
    assert_eq!(fresh.observe(&host.tiers(), Mode::Write, at(2)), Transition::Unchanged);
    assert_eq!(fresh.demand(), None);
}

/// Without a boot snapshot (not a fleet host) there is never a demand.
#[test]
fn no_boot_snapshot_is_never_a_demand() {
    let host = Host::new();
    write(&host.machine, &json!({"observability": {"enabled": true}}));
    let mut t = Tracker::default();
    assert_eq!(t.observe(&host.tiers(), Mode::Write, at(1)), Transition::Unchanged);
    assert_eq!(t.demand(), None);
}

/// Paths only: no value from either tier reaches the demand or its log line.
#[test]
fn no_value_leaks_into_the_demand_or_its_log_line() {
    let host = Host::new();
    let secret = "/home/op/.keys/super-secret-app-key.pem";
    write(&host.machine, &json!({"forge": {"githubApp": {"keyPath": "/old/key.pem"}}}));
    let mut t = booted(&host);
    write(&host.machine, &json!({"forge": {"githubApp": {"keyPath": secret}}}));
    let Transition::Raised(d) = t.observe(&host.tiers(), Mode::Write, at(1)) else {
        panic!();
    };
    let line = describe(&d);
    let debug = format!("{d:?}");
    for text in [&line, &debug] {
        assert!(!text.contains("super-secret"), "{text}");
        assert!(!text.contains("/old/key.pem"), "{text}");
    }
    assert!(line.contains("forge.githubApp.keyPath"), "{line}");
}
