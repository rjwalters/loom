use super::*;
use serde_json::json;

fn k_of(v: Value) -> Result<u32, String> {
    parse_k(&json!({ "sweep": v }))
}

#[test]
fn k_defaults_to_two_and_honors_positive_values() {
    assert_eq!(parse_k(&json!({})), Ok(2));
    assert_eq!(k_of(json!({})), Ok(2));
    assert_eq!(k_of(json!({ "glmDoctorRejectionsBeforeEscalation": 1 })), Ok(1));
    assert_eq!(k_of(json!({ "glmDoctorRejectionsBeforeEscalation": 5 })), Ok(5));
}

#[test]
fn invalid_k_names_the_key() {
    for bad in [
        json!(0),
        json!(-1),
        json!(1.5),
        json!("2"),
        json!(true),
        json!(null),
    ] {
        let err = k_of(json!({ "glmDoctorRejectionsBeforeEscalation": bad })).unwrap_err();
        assert!(err.contains("sweep.glmDoctorRejectionsBeforeEscalation"), "{err}");
        assert_eq!(
            check_config(&json!({ "sweep": { "glmDoctorRejectionsBeforeEscalation": bad } })).len(),
            1
        );
    }
    assert!(check_config(&json!({})).is_empty());
}

#[test]
fn glm_is_judged_from_the_actual_launch_report() {
    assert!(is_glm("opencode", Some("glm-5.3")));
    assert!(is_glm("opencode", Some("ZAI-GLM")));
    assert!(!is_glm("claude", Some("glm-5.3")));
    assert!(!is_glm("opencode", Some("kimi")));
    assert!(!is_glm("codex", None));
}

fn glm_attempt(s: &mut State, head: &str) {
    record_doctor(s, 7, head, true, false);
}

#[test]
fn escalates_at_k_and_not_before() {
    for k in [1u32, 2, 4] {
        let mut s = State::default();
        for i in 1..=k {
            assert_eq!(route(&s, 7, 2 + i, 99), Route::Preference, "k={k} i={i}");
            glm_attempt(&mut s, &format!("h{i}"));
            let r = record_rejection(&mut s, 7, &format!("h{i}"), k);
            assert_eq!(
                r,
                Rejection::Counted {
                    count: i,
                    escalated: i == k
                }
            );
        }
        assert!(matches!(route(&s, 7, 2 + k, 99), Route::Claude { .. }), "k={k}");
    }
}

#[test]
fn replayed_rejection_does_not_double_count() {
    let mut s = State::default();
    glm_attempt(&mut s, "a");
    assert!(matches!(
        record_rejection(&mut s, 7, "a", 2),
        Rejection::Counted { count: 1, .. }
    ));
    assert_eq!(record_rejection(&mut s, 7, "a", 2), Rejection::Replay);
    assert_eq!(s.rejections, 1);
    assert!(!s.escalated);
}

#[test]
fn non_qualifying_rejections_do_not_advance() {
    let mut s = State::default();
    // Initial Builder rejection: no Doctor attempt yet.
    assert_eq!(
        record_rejection(&mut s, 7, "b0", 1),
        Rejection::NotQualifying("no-completed-doctor-attempt")
    );
    glm_attempt(&mut s, "a");
    // Verdict for a different (stale) head.
    assert_eq!(
        record_rejection(&mut s, 7, "fffe", 1),
        Rejection::NotQualifying("stale-or-unrelated-head")
    );
    // Verdict for another PR.
    assert_eq!(
        record_rejection(&mut s, 8, "a", 1),
        Rejection::NotQualifying("no-completed-doctor-attempt")
    );
    // A Claude attempt's rejection never counts.
    record_doctor(&mut s, 7, "c", false, false);
    assert_eq!(
        record_rejection(&mut s, 7, "c", 1),
        Rejection::NotQualifying("attempt-not-on-glm")
    );
    assert_eq!(s.rejections, 0);
    assert_eq!(route(&s, 7, 2, 1), Route::Preference);
}

#[test]
fn cap_is_authoritative_over_the_latch() {
    let mut s = State::default();
    glm_attempt(&mut s, "a");
    record_rejection(&mut s, 7, "a", 1);
    assert!(s.escalated);
    // Default cap 1: the first Doctor is attempt 2; attempt 3 would be cycle 2.
    assert!(matches!(route(&s, 7, 2, 1), Route::Claude { .. }));
    assert_eq!(route(&s, 7, 3, 1), Route::CapExhausted);
    // Cap of K+1 lets the escalated attempt run.
    assert!(matches!(route(&s, 7, 3, 2), Route::Claude { .. }));
}

#[test]
fn default_cap_stops_before_default_k_reaches_claude() {
    // K=2, cap=1: the single Doctor cycle is GLM; its rejection is counted once
    // (count 1 < 2) and the next attempt is out of cap, never Claude.
    let mut s = State::default();
    assert_eq!(route(&s, 7, 2, 1), Route::Preference);
    glm_attempt(&mut s, "a");
    record_rejection(&mut s, 7, "a", 2);
    assert_eq!(route(&s, 7, 3, 1), Route::CapExhausted);
}

#[test]
fn latch_survives_a_claude_attempt_and_stays_on_claude() {
    let mut s = State::default();
    glm_attempt(&mut s, "a");
    record_rejection(&mut s, 7, "a", 1);
    record_doctor(&mut s, 7, "c", false, false);
    record_rejection(&mut s, 7, "c", 1);
    assert!(matches!(route(&s, 7, 4, 9), Route::Claude { .. }));
}

#[test]
fn another_prs_latch_does_not_apply_and_new_pr_resets() {
    let mut s = State::default();
    glm_attempt(&mut s, "a");
    record_rejection(&mut s, 7, "a", 1);
    assert_eq!(route(&s, 8, 2, 9), Route::Preference);
    record_doctor(&mut s, 8, "x", true, false);
    assert_eq!((s.pr, s.rejections, s.escalated), (8, 0, false));
}

#[test]
fn legacy_state_json_means_zero() {
    let s: State = serde_json::from_value(json!({})).unwrap();
    assert_eq!(s, State::default());
}

#[test]
fn rebase_pass_rejection_never_counts() {
    let mut s = State::default();
    record_doctor(&mut s, 7, "r1", true, true);
    assert_eq!(record_rejection(&mut s, 7, "r1", 1), Rejection::NotQualifying("rebase-pass"));
    assert_eq!((s.rejections, s.escalated), (0, false));
    // A later feedback fix on GLM counts again.
    record_doctor(&mut s, 7, "f1", true, false);
    assert!(matches!(
        record_rejection(&mut s, 7, "f1", 1),
        Rejection::Counted {
            count: 1,
            escalated: true
        }
    ));
}
