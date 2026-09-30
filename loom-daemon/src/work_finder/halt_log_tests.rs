use std::path::PathBuf;

use super::{tally, HaltTally};
use crate::main_health_gate::WorkspaceHealthStates;

fn roots() -> Vec<PathBuf> {
    vec![PathBuf::from("/r/a"), PathBuf::from("/r/b")]
}

#[test]
fn a_drain_with_every_main_green_never_blames_main_health() {
    let states = WorkspaceHealthStates::new();
    let tally = tally(&[true, true], &states, &roots(), &[false, false], (true, true, false));
    let line = tally.warn_line();
    assert!(!line.contains("main-health"), "{line}");
    assert!(line.contains("(auto-update drain)"), "{line}");
    assert!(line.contains("2 of 2 repo(s)"), "{line}");
}

#[test]
fn a_red_main_still_names_the_main_health_gate() {
    let states = WorkspaceHealthStates::new();
    let roots = roots();
    states.set_halted(&roots[0], true);
    let tally = tally(&[true, false], &states, &roots, &[false, false], (true, false, false));
    let line = tally.warn_line();
    assert!(line.contains("(main-health gate: 1 repo(s) with a red main)"), "{line}");
    assert!(line.contains("1 of 2 repo(s)"), "{line}");
}

#[test]
fn a_red_main_during_a_drain_names_every_active_cause() {
    let states = WorkspaceHealthStates::new();
    let roots = roots();
    states.set_halted(&roots[1], true);
    let tally = tally(&[true, true], &states, &roots, &[true, false], (true, true, true));
    let line = tally.warn_line();
    assert!(line.contains("main-health gate: 1 repo(s)"), "{line}");
    assert!(line.contains("pre-flight advisory / token pool: 1 repo(s)"), "{line}");
    assert!(line.contains("auto-update drain"), "{line}");
    assert!(line.contains("host breaker"), "{line}");
}

#[test]
fn an_unattributable_halt_is_worded_neutrally() {
    let tally = HaltTally {
        held: 1,
        total: 3,
        ..HaltTally::default()
    };
    let line = tally.warn_line();
    assert!(line.contains("dispatch halted for 1 of 3 repo(s) (cause unknown)"), "{line}");
    assert!(!line.contains("main-health"), "{line}");
}
