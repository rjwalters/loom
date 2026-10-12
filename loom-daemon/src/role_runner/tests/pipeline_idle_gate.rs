//! The pipeline-empty gate on hermit / architect idle generation (#10817).

use super::*;
use crate::role_runner::demand::{AxisDebt, DebtAxis, HostDebt};
use crate::role_runner::idle_gate::{self, IdleGateDecision, PipelineView, IDLE_GATE_ENV};

fn debt(review: Option<usize>) -> HostDebt {
    HostDebt {
        review: review.map(|total| AxisDebt {
            total,
            roots_with_debt: usize::from(total > 0),
        }),
        ..HostDebt::default()
    }
}

#[test]
fn predicate_table() {
    let v = |d, ready, building| PipelineView {
        debt: d,
        ready,
        building,
    };
    assert_eq!(
        idle_gate::pipeline_empty(&v(debt(Some(0)), Some(0), Some(0))),
        IdleGateDecision::Grant
    );
    assert_eq!(
        idle_gate::pipeline_empty(&v(debt(Some(0)), None, None)),
        IdleGateDecision::Grant
    );
    assert_eq!(idle_gate::pipeline_empty(&v(debt(None), None, None)), IdleGateDecision::Defer);
    assert!(matches!(
        idle_gate::pipeline_empty(&v(debt(Some(2)), None, None)),
        IdleGateDecision::Deny(_)
    ));
    assert!(matches!(
        idle_gate::pipeline_empty(&v(debt(Some(0)), Some(1), None)),
        IdleGateDecision::Deny(_)
    ));
    assert!(matches!(
        idle_gate::pipeline_empty(&v(debt(None), None, Some(1))),
        IdleGateDecision::Deny(_)
    ));
}

#[test]
fn flag_precedence_env_over_config_invalid_falls_back() {
    assert_eq!(idle_gate::resolve_flag(None, None), (false, false));
    assert_eq!(idle_gate::resolve_flag(None, Some(true)), (true, false));
    assert_eq!(idle_gate::resolve_flag(Some("1"), Some(false)), (true, false));
    assert_eq!(idle_gate::resolve_flag(Some("off"), Some(true)), (false, false));
    assert_eq!(idle_gate::resolve_flag(Some("bogus"), Some(true)), (true, true));
    assert_eq!(idle_gate::resolve_flag(Some("bogus"), None), (false, true));
}

fn gated_workspace(flag: Option<bool>) -> tempfile::TempDir {
    let tmp = tempfile::tempdir().unwrap();
    let balance = flag.map_or(String::new(), |f| format!(r#","balance":{{"idleGate":{f}}}"#));
    write_config(
        tmp.path(),
        &format!(r#"{{"autonomous":{{"roleRunner":{{"enabled":true}}{balance}}}}}"#),
    );
    tmp
}

fn plan(root: &Path) -> Vec<&'static str> {
    let cfg = on_idle_config(Some(true), vec!["hermit", "architect", "champion"]);
    let mut t = IdleTrigger::new();
    let set = new_in_progress_guard();
    let now = Instant::now();
    assert!(plan_idle_runs(&mut t, &set, root, &cfg, false, false, now).is_empty());
    let out = plan_idle_runs(&mut t, &set, root, &cfg, true, false, now);
    let names = out.iter().map(|(s, _)| s.name).collect();
    drop(out);
    names
}

#[test]
#[serial]
fn debt_denies_hermit_and_architect_but_not_other_roles() {
    let _env = ShardEnvGuard::capture();
    std::env::remove_var(IDLE_GATE_ENV);
    let ws = gated_workspace(Some(true));
    demand::global().record(ws.path(), DebtAxis::Review, 3);
    assert_eq!(plan(ws.path()), vec!["champion"]);
}

#[test]
#[serial]
fn empty_pipeline_grants() {
    let _env = ShardEnvGuard::capture();
    std::env::remove_var(IDLE_GATE_ENV);
    let ws = gated_workspace(Some(true));
    for axis in DebtAxis::ALL {
        demand::global().record(ws.path(), axis, 0);
    }
    let names = plan(ws.path());
    assert!(names.contains(&"hermit") && names.contains(&"architect"), "{names:?}");
    assert_eq!(
        idle_gate::gate(ws.path(), "hermit"),
        Some(IdleGateDecision::Grant),
        "grant is logged with trigger=idle"
    );
}

#[test]
#[serial]
fn flag_off_is_unchanged_even_with_debt() {
    let _env = ShardEnvGuard::capture();
    std::env::remove_var(IDLE_GATE_ENV);
    for flag in [None, Some(false)] {
        let ws = gated_workspace(flag);
        demand::global().record(ws.path(), DebtAxis::Review, 3);
        assert_eq!(idle_gate::gate(ws.path(), "hermit"), None);
        let mut names = plan(ws.path());
        names.sort_unstable();
        assert_eq!(names, vec!["architect", "champion", "hermit"]);
    }
}

#[test]
#[serial]
fn env_overrides_config_both_ways() {
    let _env = ShardEnvGuard::capture();
    let on = gated_workspace(Some(true));
    let off = gated_workspace(Some(false));
    std::env::set_var(IDLE_GATE_ENV, "0");
    assert!(!idle_gate::idle_gate_enabled(on.path()));
    std::env::set_var(IDLE_GATE_ENV, "1");
    assert!(idle_gate::idle_gate_enabled(off.path()));
    std::env::set_var(IDLE_GATE_ENV, "garbage");
    assert!(idle_gate::idle_gate_enabled(on.path()));
    assert!(!idle_gate::idle_gate_enabled(off.path()));
    std::env::remove_var(IDLE_GATE_ENV);
}
