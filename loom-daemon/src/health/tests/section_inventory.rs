//! The **section inventory** tests: the ones that enumerate every
//! unconditional [`super::super::assess`] section, in order, and therefore
//! have to be updated by every PR that adds one. Split out of `tests.rs`
//! (#7990, which adds `pool_hold`) so they sit together under a name that
//! says what they are, and so that file stays inside its
//! `.loom/docs/file-size-policy.md` ratchet.

use super::*;

#[test]
fn report_always_has_every_unconditional_section() {
    let report = assess(&healthy_inputs());
    let keys: Vec<&str> = report.sections.iter().map(|s| s.key).collect();
    assert_eq!(
        keys,
        vec![
            "liveness",
            "dispatch",
            "tokens",
            "roles",
            "role_liveness",
            "queues",
            "throughput",
            "peer_coordination",
            "stale_sweeps",
            "auto_update",
            "worktree_reaper",
            "pool_hold"
        ]
    );
}

#[test]
fn the_observability_section_is_appended_last_and_only_when_present() {
    let with_mismatch = assess(&mismatched_inputs(60));
    let keys: Vec<&str> = with_mismatch.sections.iter().map(|s| s.key).collect();
    assert_eq!(keys.last(), Some(&"observability"));
    // 12 always-present sections (#6157 added `peer_coordination`; #6201
    // added `role_liveness`; #7529 added `stale_sweeps`; #7584 added
    // `auto_update`; #7590 added `worktree_reaper`; #7990 added `pool_hold`)
    // + the conditional trailing `observability` note.
    assert_eq!(keys.len(), 13);
    assert_eq!(assess(&healthy_inputs()).sections.len(), 12);
}

#[test]
fn render_human_is_one_line_per_section_plus_overall() {
    let report = assess(&healthy_inputs());
    let rendered = report.render_human();
    let lines: Vec<&str> = rendered.lines().collect();
    // liveness, dispatch, tokens, roles, role_liveness (#6201), queues,
    // throughput, peer_coordination (#6157), stale_sweeps (#7529),
    // auto_update (#7584), worktree_reaper (#7590), pool_hold (#7990),
    // + overall.
    assert_eq!(lines.len(), 13);
    assert!(lines[12].starts_with("overall"));
}

#[test]
fn json_serialization_round_trips() {
    let report = assess(&healthy_inputs());
    let value = serde_json::to_value(&report).unwrap();
    assert_eq!(value["overall"], "green");
    // 12 always-present sections: + `peer_coordination` (#6157),
    // `role_liveness` (#6201), `stale_sweeps` (#7529), `auto_update`
    // (#7584), `worktree_reaper` (#7590), and `pool_hold` (#7990).
    assert_eq!(value["sections"].as_array().unwrap().len(), 12);
}

/// The `pool_hold` bucket is present and GREEN on a host holding nothing —
/// `health --json` answers "is any pool held?" without an operator having to
/// infer it from the absence of a section (#7990).
#[test]
fn the_pool_hold_section_is_green_on_a_healthy_host() {
    let report = assess(&healthy_inputs());
    let section = report.section("pool_hold").expect("pool_hold section");
    assert_eq!(section.verdict, Verdict::Green);
    assert_eq!(section.detail["count"], 0);
}
