#![allow(clippy::unwrap_used)]
use super::*;

const DENIED: &str = "exec\n/bin/bash -lc 'cat SKILL.md' in /w\n exited 1 in 0ms:\n\
    BLOCKED [role-tool-policy]: role 'judge' is restricted, and loom-daemon role-tool-policy \
    check could not answer (exit 2), so this command is denied (#8256).\n";
const RAN: &str = "exec\n/bin/bash -lc 'gh pr list' in /w\n succeeded in 9ms:\n[]\n";

#[test]
fn every_call_denied_is_a_failure_naming_the_exit() {
    let detail = verdict_in(&format!("{DENIED}{DENIED}codex\nblocked.\n")).unwrap();
    assert!(detail.starts_with(REASON_PREFIX), "{detail}");
    assert!(detail.contains("(exit 2)"), "{detail}");
}

#[test]
fn one_denied_command_among_successes_does_not_count() {
    assert_eq!(verdict_in(&format!("{DENIED}{RAN}")), None);
    assert_eq!(verdict_in(&format!("{RAN}{DENIED}")), None);
}

#[test]
fn a_denial_beside_an_ordinary_failure_does_not_count() {
    let ordinary = "exec\n/bin/bash -lc 'cat nope' in /w\n exited 1 in 3ms:\ncat: nope\n";
    assert_eq!(verdict_in(&format!("{DENIED}{ordinary}")), None);
}

#[test]
fn agent_prose_never_matches() {
    let prose = "codex\nThe run is blocked; no issues or PRs were changed.\n\
                 I saw: BLOCKED [role-tool-policy] could not answer (exit 2)\n";
    assert_eq!(verdict_in(prose), None);
    assert_eq!(verdict_in(""), None);
}

#[test]
fn only_this_ticks_region_is_read() {
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("role-judge.log");
    let header =
        |anchor: &str| format!("\n==== loom-daemon role_runner: {anchor} role=judge ====\n");
    let (old, new) = ("2026-10-13T01:00:00.000Z", "2026-10-13T02:00:00.000Z");
    std::fs::write(&log, format!("{}{DENIED}{}{RAN}", header(old), header(new))).unwrap();
    assert_eq!(detect(&log, new), None, "an earlier tick's denial is not this tick's");
    assert_eq!(detect(&log, ""), None);
    assert_eq!(detect(&dir.path().join("missing.log"), new), None);
}
