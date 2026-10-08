//! Issue #10719: the repo-ahead demand as a roll target, through
//! `select_target`, `decide` and `run_tick`.
//!
//! The demand is a second floor. The table is demand {unset, satisfied,
//! below, unsatisfiable} x floor {the four floor states} x settle {0, 600}:
//! a demand a release satisfies rolls at once to the exact tag as
//! `repo_ahead` (as `floor` when the floor drives too), an unsatisfiable one
//! stalls and holds nothing back, and without a demand every decision is
//! what the floor table already pins.

use super::*;
use crate::auto_update::floor_roll::repo_ahead::Demand;
use crate::auto_update::floor_roll::{select_target, Release, TargetSource};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Ahead {
    Unset,
    Satisfied,
    Below,
    Unsatisfiable,
}

impl Ahead {
    const ALL: [Self; 4] = [
        Self::Unset,
        Self::Satisfied,
        Self::Below,
        Self::Unsatisfiable,
    ];

    fn demand(self) -> Option<Demand> {
        let version = match self {
            Self::Unset => return None,
            // The host already runs it (a stale demand from before a roll).
            Self::Satisfied => "0.19.700",
            Self::Below => "0.19.870",
            Self::Unsatisfiable => "0.19.9000",
        };
        Some(Demand {
            version: version.to_string(),
            workspace: "acme/app".to_string(),
        })
    }
}

fn rel(version: &str) -> Release {
    Release {
        tag: format!("v{version}"),
        version: version.to_string(),
    }
}

#[test]
fn select_target_takes_the_max_of_floor_repo_ahead_and_autoupdate() {
    let pick = |floor: Option<&str>, ahead: Option<&str>, auto: Option<&str>| {
        let (floor, ahead, auto) = (floor.map(rel), ahead.map(rel), auto.map(rel));
        select_target("0.19.800", floor.as_ref(), ahead.as_ref(), auto.as_ref())
            .map(|t| (t.tag, t.source))
    };
    let got = |tag: &str, source| Some((tag.to_string(), source));
    // Repo-ahead alone drives, pinned to its tag.
    assert_eq!(pick(None, Some("0.19.900"), None), got("v0.19.900", TargetSource::RepoAhead));
    // A higher autoUpdate target wins the tag; the roll is still repo-ahead.
    assert_eq!(
        pick(None, Some("0.19.900"), Some("0.19.950")),
        got("v0.19.950", TargetSource::RepoAhead)
    );
    assert_eq!(
        pick(None, Some("0.19.900"), Some("0.19.850")),
        got("v0.19.900", TargetSource::RepoAhead)
    );
    // With the floor counting too, the source is the floor and the tag the max.
    assert_eq!(
        pick(Some("0.19.900"), Some("0.19.900"), None),
        got("v0.19.900", TargetSource::Floor)
    );
    assert_eq!(
        pick(Some("0.19.850"), Some("0.19.900"), Some("0.19.870")),
        got("v0.19.900", TargetSource::Floor)
    );
    // A target at or below running is no reason to roll.
    assert_eq!(
        pick(None, Some("0.19.800"), Some("0.19.950")),
        got("v0.19.950", TargetSource::AutoUpdate)
    );
    // An unparseable one never wins.
    assert_eq!(
        pick(None, Some("0.20.0-rc1"), Some("0.19.950")),
        got("v0.19.950", TargetSource::AutoUpdate)
    );
    assert_eq!(pick(None, None, None), None);
}

#[test]
fn decision_table_demand_x_floor_x_settle() {
    for ahead in Ahead::ALL {
        for floor in Floor::ALL {
            for settle_secs in [0, 600] {
                let case = format!("ahead={ahead:?} floor={floor:?} settle={settle_secs}");
                let tmp = tempfile::tempdir().unwrap();
                let mut state = floored(tmp.path(), floor);
                state.repo_ahead.set_basis(ahead.demand(), RUNNING);
                let fetch_calls = Arc::new(AtomicUsize::new(0));
                let mut probe = probe(newest(RUNNING), &fetch_calls);
                let trigger = Trigger::new(false);
                let status = AutoUpdateStatus::new(true);
                let settle = Duration::from_secs(settle_secs);

                let summary = run_tick(&mut state, &status, &mut probe, &trigger, settle, DEFER);

                let demanded = ahead == Ahead::Below || floor == Floor::Below;
                let rolls = settle_secs == 0 || demanded;
                assert_eq!(fetch_calls.load(Ordering::SeqCst), usize::from(rolls), "{case}");
                // The same roll path, pinned to the exact tag.
                let pinned = format!("v{NEWEST}@{SHA_B}");
                let expected: Vec<Option<String>> = if rolls { vec![Some(pinned)] } else { vec![] };
                assert_eq!(*trigger.targets.lock().unwrap(), expected, "{case}");
                let source = if floor == Floor::Below {
                    pause_manifest::TargetSource::Floor
                } else if ahead == Ahead::Below {
                    pause_manifest::TargetSource::RepoAhead
                } else {
                    pause_manifest::TargetSource::AutoUpdate
                };
                let sources: Vec<_> = if rolls { vec![source] } else { vec![] };
                assert_eq!(*trigger.sources.lock().unwrap(), sources, "{case}");
                let note = status.snapshot().note.unwrap_or_default();
                assert_eq!(note.contains("[repo-ahead:"), ahead == Ahead::Below, "{case}: {note}");
                // An unsatisfiable demand stalls, arms nothing of its own, and
                // never holds an ordinary roll back.
                assert_eq!(
                    note.contains("REPO-AHEAD DEMAND UNSATISFIABLE"),
                    ahead == Ahead::Unsatisfiable,
                    "{case}: {note}"
                );
                assert_eq!(
                    summary.floor_stall.is_some(),
                    ahead == Ahead::Unsatisfiable || floor == Floor::Unsatisfiable,
                    "{case}"
                );
            }
        }
    }
}

#[test]
fn a_repo_ahead_roll_installs_the_exact_tag_under_a_one_week_settle() {
    let tmp = tempfile::tempdir().unwrap();
    let mut state = floored(tmp.path(), Floor::Unset);
    state.repo_ahead.set_basis(Ahead::Below.demand(), RUNNING);
    let art = newest(RUNNING);
    let check = no_source();
    let inputs = TickInputs {
        artifact: &art,
        check: &check,
        tree_clean: false,
        in_flight: 0,
    };
    let week = Duration::from_secs(7 * 86_400);
    match state.decide(Instant::now(), &inputs, week, DEFER) {
        TickDecision::FetchArtifact {
            version, tag, why, ..
        } => {
            assert_eq!((version.as_str(), tag.as_str()), (NEWEST, "v0.19.900"));
            assert!(why.contains("workspace acme/app needs daemon 0.19.870"), "{why}");
            assert!(why.contains("exact tag v0.19.900"), "{why}");
        }
        other => panic!("expected a repo-ahead fetch, got {other:?}"),
    }
    // Without the demand the same tick waits for settle.
    let mut plain = floored(tmp.path(), Floor::Unset);
    assert!(!matches!(
        plain.decide(Instant::now(), &inputs, week, DEFER),
        TickDecision::FetchArtifact { .. }
    ));
}

#[test]
fn the_stall_alert_is_logged_when_it_starts_and_then_once_per_reminder() {
    use crate::auto_update::floor_roll::alert::REMINDER;
    let mut state = crate::auto_update::floor_roll::repo_ahead::RepoAheadState::default();
    state.set_basis(Ahead::Unsatisfiable.demand(), RUNNING);
    let t0 = Utc::now();
    assert!(!state.alert_due(t0, REMINDER), "nothing observed yet");
    state.observe(Some(&rel(NEWEST)));
    assert!(state.stall().is_some());
    assert!(state.alert_due(t0, REMINDER));
    assert!(!state.alert_due(t0 + chrono::Duration::minutes(30), REMINDER));
    assert!(state.alert_due(t0 + chrono::Duration::minutes(61), REMINDER));
    // A tick that resolves nothing keeps the stall; the demand going away drops it.
    state.observe(None);
    assert!(state.stall().is_some());
    state.set_basis(None, RUNNING);
    assert!(state.stall().is_none());
    assert!(!state.driving());
}
