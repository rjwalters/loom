//! The emit policy.

use crate::eta::emit::{EmitState, Signature, Trigger, HOURLY_CAP};
use crate::eta::{NoEstimateReason, Stage};
use chrono::{Duration, TimeZone, Utc};

fn at_stage(stage: Stage) -> Signature {
    Signature {
        stage: Some(stage),
        rework_rounds: 0,
        reason: None,
    }
}

#[test]
fn emit_policy() {
    let t0 = Utc.with_ymd_and_hms(2026, 9, 28, 12, 0, 0).unwrap();
    let review = at_stage(Stage::ReviewWait);
    let mut state = EmitState::default();
    assert_eq!(state.decide(review, t0, 300), Some(Trigger::First));
    state.record(review, t0);

    // No re-emission for an unchanged estimate inside the refresh window.
    for secs in [1, 60, 200, 269] {
        assert_eq!(state.decide(review, t0 + Duration::seconds(secs), 300), None, "{secs}s");
    }
    // Refreshed once the window (less a tenth of slack) has passed.
    assert_eq!(state.decide(review, t0 + Duration::seconds(270), 300), Some(Trigger::Refresh));

    // A stage change emits immediately.
    let merge = at_stage(Stage::MergeWait);
    assert_eq!(state.decide(merge, t0 + Duration::seconds(5), 300), Some(Trigger::Transition));
    // So does a new rework round at the same stage.
    let reworked = Signature {
        rework_rounds: 1,
        ..review
    };
    assert_eq!(
        state.decide(reworked, t0 + Duration::seconds(5), 300),
        Some(Trigger::Transition)
    );

    // A refusal is emitted on its transition, never refreshed.
    let blocked = Signature {
        reason: Some(NoEstimateReason::Blocked),
        ..review
    };
    let t1 = t0 + Duration::seconds(10);
    assert_eq!(state.decide(blocked, t1, 300), Some(Trigger::Transition));
    state.record(blocked, t1);
    assert_eq!(state.decide(blocked, t1 + Duration::hours(3), 300), None);

    // The hourly cap holds even for a flapping item.
    let mut flapping = EmitState::default();
    let mut emitted = 0;
    for i in 0..(HOURLY_CAP * 3) {
        let now = t0 + Duration::seconds(i as i64 * 30);
        let signature = at_stage(if i % 2 == 0 {
            Stage::ReviewWait
        } else {
            Stage::Doctor
        });
        if flapping.decide(signature, now, 300).is_some() {
            flapping.record(signature, now);
            emitted += 1;
        }
    }
    // 60 flips over 30 minutes: capped at the hourly limit.
    assert_eq!(emitted, HOURLY_CAP);
    // The window rolls: an hour after the first emission there is room again.
    let later = t0 + Duration::seconds(3601);
    assert!(flapping
        .decide(at_stage(Stage::MergeWait), later, 300)
        .is_some());
}
