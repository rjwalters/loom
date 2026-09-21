//! Unit tests for [`crate::foreign_load`] (Issue #8478).
//!
//! Every test here is against a **synthetic** process table. Nothing shells out
//! to `ps`: the whole point of splitting [`super::parse_ps_output`] /
//! [`super::top_consumers`] / [`super::format_clause`] out of the probe is that
//! the attribution logic is testable on a host whose real process list is
//! irrelevant (and, in CI, unreproducible).

use super::{
    format_clause, parse_ps_output, top_consumers, ConsumerGroup, ProcSample, MIN_GROUP_PCT_CPU,
};

/// The incident's shape (#8478): a large fan-out of one command, every member
/// reparented to pid 1 because the sweep that spawned them is gone.
fn incident_table() -> String {
    let mut lines = vec![
        "    1     0   0.0 /sbin/launchd".to_string(),
        " 9001     1   4.2 loom-daemon".to_string(),
    ];
    for pid in 20000..20025 {
        lines.push(format!("{pid}     1  73.7 /opt/homebrew/bin/ngspice"));
    }
    lines.join("\n")
}

#[test]
fn parses_four_columns_and_basenames_the_command() {
    let samples = parse_ps_output(" 4242  4200  12.5 /usr/local/bin/ngspice\n");
    assert_eq!(
        samples,
        vec![ProcSample {
            pid: 4242,
            ppid: 4200,
            pct_cpu: 12.5,
            command: "ngspice".to_string(),
        }],
        "macOS ps prints an absolute path in comm; the group key must be the basename"
    );
}

#[test]
fn skips_unparseable_lines_instead_of_failing_the_whole_sample() {
    // A header line, a truncated line, and a real row. Partial attribution
    // beats none: the probe must still name `cargo`.
    let stdout = "  PID  PPID %CPU COMMAND\n garbage\n 7 1\n 700 1 55.0 cargo\n";
    let samples = parse_ps_output(stdout);
    assert_eq!(samples.len(), 1, "only the well-formed row is a sample");
    assert_eq!(samples[0].command, "cargo");
}

#[test]
fn empty_output_yields_no_samples_and_an_empty_clause() {
    assert!(parse_ps_output("").is_empty());
    assert_eq!(
        format_clause(&top_consumers(&[], 3)),
        "",
        "an empty clause is the contract: the caller concatenates unconditionally"
    );
}

#[test]
fn groups_by_command_summing_cpu_and_counting_instances() {
    let groups = top_consumers(&parse_ps_output(&incident_table()), 3);
    let ngspice = groups
        .iter()
        .find(|g| g.command == "ngspice")
        .expect("ngspice must be reported");
    assert_eq!(ngspice.instances, 25, "25 processes collapse into one group");
    assert!(
        (ngspice.pct_cpu - 25.0 * 73.7).abs() < 0.01,
        "group cpu is the sum across members, got {}",
        ngspice.pct_cpu
    );
}

#[test]
fn ranks_the_runaway_group_first_even_though_it_is_not_the_hottest_single_process() {
    // One 90%-cpu process vs. twenty-five 73.7% ones. Per-process ranking would
    // name the wrong culprit; the incident is the fan-out.
    let mut table = incident_table();
    table.push_str("\n 5555     1  90.0 Xcode");
    let groups = top_consumers(&parse_ps_output(&table), 3);
    assert_eq!(groups[0].command, "ngspice");
    assert_eq!(groups[1].command, "Xcode");
}

#[test]
fn flags_reparented_to_init_and_names_pid_1_even_when_absent_from_the_sample() {
    // An unprivileged `ps` commonly omits pid 1 itself; the parent must still
    // resolve to launchd/init rather than "unknown".
    let samples = parse_ps_output(" 20000     1  73.7 ngspice\n 20001     1  73.7 ngspice\n");
    let groups = top_consumers(&samples, 3);
    assert!(groups[0].reparented_to_init, "ppid 1 must be flagged");
    let (parent_command, parent_pid) = groups[0]
        .parent
        .clone()
        .expect("pid 1 resolves by convention");
    assert_eq!(parent_pid, 1);
    assert!(
        parent_command == "launchd" || parent_command == "init",
        "parent of pid 1 must be named by platform convention, got {parent_command}"
    );
}

#[test]
fn resolves_a_live_parent_to_its_command_name() {
    let samples = parse_ps_output(" 9100  9000  10.0 claude\n 9000     1   1.0 spawn-claude.sh\n");
    let groups = top_consumers(&samples, 3);
    let claude = groups
        .iter()
        .find(|g| g.command == "claude")
        .expect("claude group present");
    assert_eq!(
        claude.parent,
        Some(("spawn-claude.sh".to_string(), 9000)),
        "a live Loom session's child must show its real parent, not pid 1"
    );
    assert!(!claude.reparented_to_init, "an owned process must NOT be flagged as reparented");
}

#[test]
fn drops_groups_under_the_noise_floor() {
    let samples = parse_ps_output(" 10 1 0.1 idled\n 11 1 50.0 ngspice\n");
    let groups = top_consumers(&samples, 3);
    assert_eq!(groups.len(), 1, "sub-{MIN_GROUP_PCT_CPU}% groups are noise");
    assert_eq!(groups[0].command, "ngspice");
}

#[test]
fn honors_the_group_limit() {
    let samples = parse_ps_output(" 1 1 40.0 a\n 2 1 30.0 b\n 3 1 20.0 c\n 4 1 10.0 d\n");
    assert_eq!(top_consumers(&samples, 2).len(), 2);
}

#[test]
fn ordering_is_deterministic_for_equal_cpu() {
    let samples = parse_ps_output(" 1 1 10.0 zebra\n 2 1 10.0 alpha\n");
    let groups = top_consumers(&samples, 3);
    assert_eq!(
        groups
            .iter()
            .map(|g| g.command.as_str())
            .collect::<Vec<_>>(),
        vec!["alpha", "zebra"],
        "ties break on name so consecutive log lines are diffable"
    );
}

#[test]
fn clause_names_the_command_count_cpu_and_parent() {
    let clause = format_clause(&top_consumers(&parse_ps_output(&incident_table()), 3));
    assert!(clause.contains("ngspice \u{d7}25"), "clause was: {clause}");
    assert!(clause.contains("% cpu"), "clause was: {clause}");
    assert!(clause.contains("parent"), "clause was: {clause}");
    assert!(
        clause.contains("reparented to pid 1"),
        "the orphan signature is the whole diagnosis; clause was: {clause}"
    );
    assert!(
        clause.contains("long-running-compute.md"),
        "point the reader at the rule that forbids this; clause was: {clause}"
    );
}

#[test]
fn clause_omits_the_orphan_note_when_nothing_is_reparented() {
    let groups = vec![ConsumerGroup {
        command: "cargo".to_string(),
        instances: 2,
        pct_cpu: 180.0,
        parent: Some(("claude".to_string(), 4242)),
        reparented_to_init: false,
    }];
    let clause = format_clause(&groups);
    assert!(clause.contains("cargo \u{d7}2"), "clause was: {clause}");
    assert!(
        !clause.contains("reparented"),
        "an ordinary busy host must not be accused of an escape; clause was: {clause}"
    );
}

#[test]
fn clause_starts_with_a_separator_so_it_concatenates_onto_a_message() {
    let clause = format_clause(&top_consumers(&parse_ps_output(&incident_table()), 3));
    assert!(
        clause.starts_with(" \u{2014} "),
        "must append cleanly to the STARVING line, got {clause:?}"
    );
}
