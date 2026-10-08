//! `eta-fit/v3` (#10521): the schema layout, the one shared builder, and
//! train/serve parity and leak checks through it.

use super::fit_rows::{approve, cutoff, h, landed, open, secs, snapshot, REPO};
use super::provenance;
use crate::eta::fit::features_v2::{
    model_features_v2, ModelInputsV2, FEATURES_V2, N_FEATURES_V2, SCHEMA_V2,
};
use crate::eta::fit::features_v3::{
    model_features_v3, training_inputs_v3, ModelInputsV3, FEATURES_V3, N_FEATURES_V3, SCHEMA_V3,
};
use crate::eta::fit::rows;
use crate::eta::fit::SCHEMA;
use crate::eta::loop_features::{
    loop_features, loop_vector, CiObservation, FileSnapshot, LoopInputs, LOOP_FEATURES,
    N_LOOP_FEATURES,
};
use crate::eta::tracker::Tracker;
use crate::pr_latency::history::fixtures::{labeled, unlabeled};
use crate::pr_latency::{APPROVED, REVIEW_REQUESTED};
use chrono::{DateTime, Duration, Utc};

#[test]
fn schema_is_v2_then_the_loop_columns() {
    assert_ne!(SCHEMA_V3, SCHEMA);
    assert_ne!(SCHEMA_V3, SCHEMA_V2);
    assert_eq!(N_FEATURES_V3, N_FEATURES_V2 + N_LOOP_FEATURES);
    assert_eq!(&FEATURES_V3[..N_FEATURES_V2], &FEATURES_V2[..]);
    assert_eq!(&FEATURES_V3[N_FEATURES_V2..], &LOOP_FEATURES[..]);
}

#[test]
fn v3_prefix_is_v2_bit_for_bit() {
    let m = ModelInputsV3::default();
    let v3 = model_features_v3(&m);
    let v2 = model_features_v2(&ModelInputsV2::default());
    for i in 0..N_FEATURES_V2 {
        assert_eq!(v3[i].to_bits(), v2[i].to_bits(), "position {i}");
    }
    assert_eq!(&v3[N_FEATURES_V2..], &loop_vector(&m.loops)[..]);
}

fn fleet() -> Vec<crate::eta::fleet::FleetSnapshot> {
    let mut ev = approve(h(1.0), h(3.0));
    ev.push(labeled(REVIEW_REQUESTED, secs(h(6.0))));
    ev.push(unlabeled(APPROVED, secs(h(6.0))));
    ev.push(unlabeled(REVIEW_REQUESTED, secs(h(8.0))));
    ev.push(labeled(APPROVED, secs(h(8.0))));
    let prs = vec![
        landed(90, h(1.0), h(2.0), h(3.0)),
        open(1, ev),
        open(2, vec![labeled(REVIEW_REQUESTED, secs(h(2.0)))]),
    ];
    vec![snapshot(REPO, &prs, cutoff() + Duration::hours(1))]
}

#[test]
fn every_training_row_equals_its_served_vector() {
    let snaps = fleet();
    let a = rows::build(&snaps, cutoff());
    assert!(!a.rows.is_empty());
    assert!(training_inputs_v3(&a, a.rows.len()).is_none());
    let mut tracker = Tracker::new(provenance());
    tracker.on_fleet_snapshots(&snaps, h(10.0));
    for (i, k) in a.row_keys.iter().enumerate() {
        let train = training_inputs_v3(&a, i).unwrap();
        // Serving reads the loop columns through the tracker, then the one
        // transform; the rest of the inputs are the row's own.
        let served = ModelInputsV3 {
            loops: tracker.loop_features_of(&k.repo, k.pr, k.at),
            ..train.clone()
        };
        let (x, y) = (model_features_v3(&train), model_features_v3(&served));
        for j in 0..N_FEATURES_V3 {
            assert_eq!(
                x[j].to_bits(),
                y[j].to_bits(),
                "#{} at {} col {}",
                k.pr,
                k.at,
                FEATURES_V3[j]
            );
        }
    }
}

fn snap(pr: u32, known: DateTime<Utc>, files: &[&str]) -> FileSnapshot {
    FileSnapshot {
        repo: REPO.into(),
        pr,
        known_at: known,
        files: files.iter().map(|f| (*f).to_string()).collect(),
        head_sha: None,
        complete: true,
        additions: None,
        deletions: None,
        listed: None,
    }
}

#[test]
fn file_lists_and_ci_after_as_of_move_no_column() {
    let snaps = fleet();
    let eps = &snaps[0].episodes;
    let refs: Vec<_> = eps.iter().collect();
    let own: Vec<_> = eps.iter().filter(|e| e.pr_number == 1).collect();
    let as_of = h(10.0);
    let known = [
        snap(1, as_of - Duration::hours(2), &["a.rs", "b.rs"]),
        snap(2, as_of - Duration::hours(2), &["b.rs"]),
    ];
    let ci = [CiObservation {
        pr: 1,
        at: as_of - Duration::hours(1),
        failed: false,
    }];
    let vec_of = |files: &[FileSnapshot], ci: &[CiObservation]| {
        let l = loop_features(
            &LoopInputs {
                repo: REPO,
                pr: 1,
                own: &own,
                repo_episodes: &refs,
                files: Some(files),
                ci: Some(ci),
            },
            as_of,
        );
        model_features_v3(&ModelInputsV3 {
            loops: l,
            ..ModelInputsV3::default()
        })
    };
    let base = vec_of(&known, &ci);
    let n = N_FEATURES_V2;
    assert_eq!(base[n + 7], 1.0, "overlap_known");
    assert_eq!(base[n + 5], 1f64.ln_1p(), "one overlapping peer");
    // The final diff (a later snapshot) and a later failing run are not known at as_of.
    let mut later = known.to_vec();
    later.push(snap(1, as_of + Duration::hours(1), &["z.rs"]));
    later.push(snap(2, as_of + Duration::hours(1), &["a.rs", "b.rs", "c.rs"]));
    let mut later_ci = ci.to_vec();
    later_ci.push(CiObservation {
        pr: 1,
        at: as_of + Duration::minutes(1),
        failed: true,
    });
    assert_eq!(base, vec_of(&later, &later_ci));
    // A source that is not logged leaves its indicators at 0, not a known 0.
    let none = model_features_v3(&ModelInputsV3::default());
    assert_eq!((none[n + 7], none[n + 9]), (0.0, 0.0));
}
