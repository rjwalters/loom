//! Roll pause/resume of a step (#11898): an issue step resumes as itself, a
//! PR step is requeued with its PR lock released.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::host::{self, sweeps};
use super::*;
use crate::step_role::StepRole;
use crate::sweep_registry::roll_requeue::RollRequeueNotice;
use crate::sweep_registry::SweepRegistry;
use std::sync::Mutex;

fn step_item(id: &str, issue: Option<u32>, pr: Option<u32>, step: StepRole) -> ManifestItem {
    let mut item: ManifestItem = serde_json::from_value(serde_json::json!({
        "id": id, "kind": "sweep", "repo": "/r", "disposition": "resume", "status": "paused",
        "resume_handle": {
            "runtime": "claude", "session_id": "4910f978-64b9-4654-942a-dae6514e859c",
            "resume_count": 0
        },
    }))
    .unwrap();
    item.issue = issue;
    item.pr = pr;
    item.step = Some(step);
    item
}

fn launch() -> RollResumeLaunch {
    RollResumeLaunch {
        runtime: "claude".into(),
        session_id: "4910f978-64b9-4654-942a-dae6514e859c".into(),
        ..RollResumeLaunch::default()
    }
}

#[test]
fn an_issue_step_resumes_as_its_step_and_a_pr_step_does_not() {
    let curator = step_item("step-curator-5-1", Some(5), None, StepRole::Curator);
    let spec = host::sweep_spec(&curator, &launch()).expect("an issue step is resumable");
    assert_eq!(
        spec.kind(),
        crate::types::SweepKind::Step {
            item: 5,
            role: StepRole::Curator
        }
    );
    let judge = step_item("step-judge-6-1", None, Some(6), StepRole::Judge);
    assert!(host::sweep_spec(&judge, &launch()).is_none(), "a PR step is requeued");
}

#[test]
fn the_manifest_round_trips_the_step_and_omits_it_for_a_sweep() {
    let item = step_item("step-doctor-7-1", None, Some(7), StepRole::Doctor);
    let json = serde_json::to_value(&item).unwrap();
    assert_eq!(json["step"], "doctor");
    let back: ManifestItem = serde_json::from_value(json).unwrap();
    assert_eq!(back.step, Some(StepRole::Doctor));
    let mut sweep = item;
    sweep.step = None;
    assert!(serde_json::to_value(&sweep).unwrap().get("step").is_none());
}

#[test]
fn requeueing_a_pr_step_releases_its_pr_lock() {
    let dir = tempfile::tempdir().unwrap();
    let mut config = crate::sweep_registry::SweepRegistryConfig::new(dir.path().to_path_buf());
    config.skip_label_flip = true;
    let registry = Arc::new(Mutex::new(SweepRegistry::new(config)));
    let id = "step-judge-8-1";
    let lock = {
        let reg = registry.lock().unwrap();
        reg.acquire_pr_lock(8, id).unwrap();
        reg.config().locks_dir().join("pr-8")
    };
    assert!(lock.exists());
    let item = step_item(id, None, Some(8), StepRole::Judge);
    sweeps::requeue(&registry, &item, &RollRequeueNotice::default(), true).unwrap();
    assert!(!lock.exists(), "the PR step's lock is released");
}
