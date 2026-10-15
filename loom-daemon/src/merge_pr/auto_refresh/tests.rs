//! The #12014 decision table: remedy outcome -> (exit, next), refresh-head
//! detection, the re-invoke wall-clock bound, the verdict gate, and config
//! resolution.

use super::*;
use chrono::TimeZone;
use serde_json::json;

const HEAD: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const NEW: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
const OLD: &str = "cccccccccccccccccccccccccccccccccccccccc";

fn at(secs: i64) -> DateTime<Utc> {
    Utc.timestamp_opt(1_800_000_000 + secs, 0).unwrap()
}

fn comment(body: &str, secs: i64) -> Value {
    json!({ "body": body, "created_at": at(secs).to_rfc3339() })
}

/// The `next=` token of a decision's first line, or the line itself when it
/// is not a `LOOM-AUTO-REFRESH next=` line.
fn first_line(d: &Decision) -> &str {
    d.stdout.lines().next().unwrap_or("")
}

fn next_of(d: &Decision) -> Option<&str> {
    first_line(d)
        .strip_prefix("LOOM-AUTO-REFRESH next=")
        .and_then(|r| r.split(' ').next())
}

#[test]
fn every_remedy_outcome_maps_to_its_exit_and_next() {
    let cases: Vec<(RemedyOutcome, i32, Option<&str>)> = vec![
        (
            RemedyOutcome::Pushed {
                new_sha: NEW.into(),
                n: 1,
            },
            4,
            Some("reinvoke"),
        ),
        (
            RemedyOutcome::Deferred {
                spent: 1,
                retry_after: at(600),
            },
            4,
            Some("requeue"),
        ),
        (
            RemedyOutcome::SyncedBase {
                head: HEAD.into(),
                n: 1,
                max: 1,
            },
            4,
            Some("reinvoke"),
        ),
        (
            RemedyOutcome::SyncConflict {
                notice_posted: true,
            },
            4,
            Some("requeue"),
        ),
        (
            RemedyOutcome::Escalated {
                notice_posted: true,
                spent: 3,
                budget: 3,
            },
            6,
            None,
        ),
        (
            RemedyOutcome::HeadMoved {
                current: NEW.into(),
            },
            3,
            None,
        ),
        (RemedyOutcome::Failed("gh down".into()), 1, None),
    ];
    for (outcome, exit, next) in cases {
        let d = stale_decision(&outcome, "42", HEAD, 3);
        assert_eq!(d.exit, exit, "{outcome:?}");
        assert_eq!(next_of(&d), next, "{outcome:?}: {}", d.stdout);
        // Exactly one sentinel line, and only on the re-queue exits.
        let sentinels = d
            .stdout
            .lines()
            .filter(|l| l.starts_with("LOOM-AUTO-REFRESH next="))
            .count();
        assert_eq!(sentinels, usize::from(next.is_some()), "{outcome:?}");
    }
}

#[test]
fn a_push_reports_its_round_and_the_new_head() {
    let d = stale_decision(
        &RemedyOutcome::Pushed {
            new_sha: NEW.into(),
            n: 2,
        },
        "42",
        HEAD,
        3,
    );
    assert_eq!(
        first_line(&d),
        format!("LOOM-AUTO-REFRESH next=reinvoke pr=42 round=2/3 head={NEW}")
    );
}

#[test]
fn exhaustion_is_exit_6_with_the_hold_kept_never_a_failure() {
    for notice_posted in [true, false] {
        let d = stale_decision(
            &RemedyOutcome::Escalated {
                notice_posted,
                spent: 3,
                budget: 3,
            },
            "42",
            HEAD,
            3,
        );
        assert_eq!(d.exit, EXIT_EXHAUSTED);
        assert_eq!(first_line(&d), "LOOM-AUTO-REFRESH-EXHAUSTED pr=42 spent=3 budget=3");
        assert!(d.stdout.contains("loom:operator"), "{}", d.stdout);
        assert!(!d.stdout.contains("Merge Failed"), "{}", d.stdout);
    }
}

#[test]
fn head_moved_names_the_current_head_on_one_line() {
    let d = stale_decision(
        &RemedyOutcome::HeadMoved {
            current: NEW.into(),
        },
        "42",
        HEAD,
        3,
    );
    assert_eq!(d.stdout, format!("LOOM-AUTO-REFRESH-HEAD-MOVED current={NEW}"));
}

#[test]
fn refresh_mark_reads_the_redate_chain_position() {
    let listing = vec![
        comment(&budget::attempt_marker(HEAD, 1), 0),
        comment(&budget::attempt_marker(HEAD, 2), 900),
    ];
    let mark = refresh_mark(&listing, HEAD, None).expect("a re-date head");
    assert_eq!(mark.round, 2);
    assert_eq!(mark.recorded_at, Some(at(900)));
}

#[test]
fn refresh_mark_reads_a_base_sync_through_the_first_parent() {
    let listing = vec![comment(&sync_handoff::sync_marker(OLD, 1), 100)];
    let mark = refresh_mark(&listing, HEAD, Some(OLD)).expect("a sync head");
    assert_eq!(mark.round, 0, "a sync starts a fresh chain");
    assert_eq!(mark.recorded_at, Some(at(100)));
    assert_eq!(
        refresh_mark(&listing, HEAD, Some(NEW)),
        None,
        "a sync FROM another head says nothing about this one"
    );
    assert_eq!(refresh_mark(&listing, HEAD, None), None);
}

#[test]
fn a_head_loom_did_not_refresh_has_no_mark() {
    let listing = vec![
        comment(&budget::attempt_marker(NEW, 1), 0),
        comment("an ordinary comment", 10),
    ];
    assert_eq!(refresh_mark(&listing, HEAD, None), None);
    assert_eq!(refresh_mark(&[], HEAD, Some(OLD)), None);
}

#[test]
fn timeout_reinvokes_only_inside_the_wall_clock_bound() {
    let mark = |s| RefreshMark {
        round: 1,
        recorded_at: Some(at(s)),
    };
    assert_eq!(timeout_next(&mark(0), at(600), 2700), Next::Reinvoke);
    assert_eq!(timeout_next(&mark(0), at(2699), 2700), Next::Reinvoke);
    assert_eq!(timeout_next(&mark(0), at(2700), 2700), Next::Requeue);
    assert_eq!(timeout_next(&mark(0), at(10_000), 2700), Next::Requeue);
    let unknown = RefreshMark {
        round: 1,
        recorded_at: None,
    };
    assert_eq!(timeout_next(&unknown, at(1), 2700), Next::Requeue, "an unknown age never loops");
}

#[test]
fn the_verdict_gate_carries_only_tree_and_clean_merge() {
    use EquivalenceKind::{CleanMerge, RebasePatchIdentical, Tree};
    let cases = [
        (Equivalence::Equivalent(Tree), true),
        (Equivalence::Equivalent(CleanMerge), true),
        (Equivalence::Equivalent(RebasePatchIdentical), false),
        (Equivalence::Changed, false),
        (Equivalence::Indeterminate, false),
    ];
    for (eq, carries) in cases {
        let mut asked = None;
        let gate = verdict_gate(Some(OLD), HEAD, |m| {
            asked = Some(m.to_string());
            eq
        });
        assert_eq!(gate == Gate::Clear, carries, "{eq:?}: {gate:?}");
        assert_eq!(asked.as_deref(), Some(OLD), "compared against the marker");
    }
}

#[test]
fn a_missing_trusted_marker_needs_a_fresh_judge() {
    let gate = verdict_gate(None, HEAD, |_| panic!("nothing to compare"));
    assert!(matches!(gate, Gate::Judge(ref why) if why.contains("#9258")), "{gate:?}");
}

#[test]
fn a_marker_already_on_the_head_is_clear_without_a_compare() {
    assert_eq!(verdict_gate(Some(HEAD), HEAD, |_| panic!("no compare needed")), Gate::Clear);
    assert_eq!(
        verdict_gate(Some(&HEAD[..12]), HEAD, |_| panic!("abbreviated marker")),
        Gate::Clear
    );
}

#[test]
fn config_defaults_on_with_a_2700s_bound() {
    let c = Config::resolve(None, None, &json!({}));
    assert_eq!(
        c,
        Config {
            enabled: true,
            max_wait_secs: DEFAULT_MAX_WAIT_SECS
        }
    );
}

#[test]
fn config_env_beats_config_beats_default() {
    let cfg = json!({"merge": {"autoRefresh": false, "autoRefreshMaxWaitSecs": 1200}});
    assert_eq!(
        Config::resolve(None, None, &cfg),
        Config {
            enabled: false,
            max_wait_secs: 1200
        }
    );
    assert_eq!(
        Config::resolve(Some("on"), Some("60"), &cfg),
        Config {
            enabled: true,
            max_wait_secs: 60
        }
    );
    for off in ["0", "false", "off", "no"] {
        assert!(!Config::resolve(Some(off), None, &json!({})).enabled, "{off}");
    }
    // Unparseable env falls through to the config tier.
    assert_eq!(Config::resolve(Some("maybe"), Some("soon"), &cfg).max_wait_secs, 1200);
    assert!(!Config::resolve(Some("maybe"), None, &cfg).enabled);
}

#[test]
fn config_clamps_the_bound() {
    assert_eq!(
        Config::resolve(None, Some("99999999"), &json!({})).max_wait_secs,
        MAX_MAX_WAIT_SECS
    );
}
