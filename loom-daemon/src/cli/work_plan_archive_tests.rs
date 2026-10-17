//! Tests for `work-docs archive|check --plan` (issue #12635).

use std::fs;
use std::path::Path;

use chrono::NaiveDate;

use super::*;

fn today() -> NaiveDate {
    NaiveDate::from_ymd_opt(2026, 10, 17).unwrap()
}

const GENERATED: &str = "<!-- guide:plan-body:start -->\n## Ready\n- #1 generated\n\n\
                         ### Old\n- [ ] generated open item\n<!-- guide:plan-body:end -->\n";

fn setup(plan: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("WORK_PLAN.md"), plan).unwrap();
    dir
}

fn read(root: &Path, rel: &str) -> String {
    fs::read_to_string(root.join(rel)).unwrap()
}

fn run(root: &Path) -> Result<String> {
    archive_plan(root, today(), false)
}

fn root_args(root: &Path) -> RootArgs {
    RootArgs {
        root: root.to_path_buf(),
    }
}

#[test]
fn generated_only_plan_is_a_byte_identical_noop() {
    let text = format!("# Work Plan\n\n{GENERATED}");
    let dir = setup(&text);
    let msg = run(dir.path()).unwrap();
    assert!(msg.contains("nothing to do"), "{msg}");
    assert_eq!(read(dir.path(), "WORK_PLAN.md"), text);
    assert!(!dir.path().join("work_plan").exists());
}

#[test]
fn old_unmarked_open_item_stays() {
    let text = format!(
        "# Work Plan\n\n## Backlog (2024-01-02)\n- [ ] ancient but still open\n\n{GENERATED}"
    );
    let dir = setup(&text);
    run(dir.path()).unwrap();
    assert_eq!(read(dir.path(), "WORK_PLAN.md"), text);
    assert!(!dir.path().join("work_plan").exists());
}

#[test]
fn marked_section_moves_verbatim_and_rerun_is_noop() {
    let section =
        "<!-- loom:plan-archive -->\n## Phase 1 (done)\n- [x] shipped\n\n### Detail\nbody\n\n";
    let text =
        format!("# Work Plan\n\n## Keep\n- [ ] open\n\n{section}## After\ntail\n\n{GENERATED}");
    let dir = setup(&text);
    run(dir.path()).unwrap();

    let active = read(dir.path(), "WORK_PLAN.md");
    assert!(!active.contains("Phase 1"));
    assert!(active.contains("## Keep\n- [ ] open\n\n"));
    assert!(active.contains("## After\ntail\n"));
    assert!(active.contains("## Archives\n\n- [2026-10](work_plan/WORK_PLAN_2026-10.md)\n"));
    assert!(active.ends_with(GENERATED));

    let archived = read(dir.path(), "work_plan/WORK_PLAN_2026-10.md");
    assert!(archived.starts_with("# Work Plan Archive — 2026-10\n"));
    assert!(archived.ends_with(section));

    let again = run(dir.path()).unwrap();
    assert!(again.contains("nothing to do"), "{again}");
    assert_eq!(read(dir.path(), "WORK_PLAN.md"), active);
    assert_eq!(read(dir.path(), "work_plan/WORK_PLAN_2026-10.md"), archived);

    let report = check_plan(&root_args(dir.path())).unwrap();
    assert!(report.starts_with("ok:"), "{report}");
}

#[test]
fn generated_region_and_adjacent_hand_text_are_untouched() {
    let text = format!(
        "# Work Plan\n\n<!-- loom:plan-archive -->\n## Done\nx\n{GENERATED}## Hand below\nkeep\n"
    );
    let dir = setup(&text);
    run(dir.path()).unwrap();
    let active = read(dir.path(), "WORK_PLAN.md");
    assert!(active.contains(GENERATED), "{active}");
    assert!(active.ends_with("## Hand below\nkeep\n"));
    // The section stopped at the generated start marker.
    let archived = read(dir.path(), "work_plan/WORK_PLAN_2026-10.md");
    assert!(archived.ends_with("<!-- loom:plan-archive -->\n## Done\nx\n"));
}

#[test]
fn marker_inside_generated_region_or_fence_is_ignored() {
    let text = "# P\n<!-- guide:plan-body:start -->\n<!-- loom:plan-archive -->\n## Gen\n\
                <!-- guide:plan-body:end -->\n```\n<!-- loom:plan-archive -->\n## Fenced\n```\n";
    let dir = setup(text);
    run(dir.path()).unwrap();
    assert_eq!(read(dir.path(), "WORK_PLAN.md"), text);
}

#[test]
fn marked_section_with_open_item_moves_with_warning() {
    let text = "# P\n<!-- loom:plan-archive -->\n## Done?\n- [ ] still open\n";
    let dir = setup(text);
    let msg = run(dir.path()).unwrap();
    assert!(msg.contains("warning") && msg.contains("open `- [ ]`"), "{msg}");
    assert!(read(dir.path(), "work_plan/WORK_PLAN_2026-10.md").contains("- [ ] still open"));
    assert!(!read(dir.path(), "WORK_PLAN.md").contains("still open"));
}

#[test]
fn adjacent_marked_siblings_move_separately_and_stop_at_equal_level() {
    let text =
        "# P\n<!-- loom:plan-archive -->\n## Alpha\na\n<!-- loom:plan-archive -->\n## Beta\nb\n## C\nc\n";
    let dir = setup(text);
    run(dir.path()).unwrap();
    let active = read(dir.path(), "WORK_PLAN.md");
    assert!(
        active.contains("## C\nc\n") && !active.contains("## Alpha") && !active.contains("## Beta")
    );
    let archived = read(dir.path(), "work_plan/WORK_PLAN_2026-10.md");
    assert!(archived.ends_with(
        "<!-- loom:plan-archive -->\n## Alpha\na\n<!-- loom:plan-archive -->\n## Beta\nb\n"
    ));
}

#[test]
fn marker_not_above_heading_is_ignored_with_warning() {
    let text = "# P\n<!-- loom:plan-archive -->\nplain text\n";
    let dir = setup(text);
    let msg = run(dir.path()).unwrap();
    assert!(msg.contains("not directly above a heading"), "{msg}");
    assert_eq!(read(dir.path(), "WORK_PLAN.md"), text);
}

#[test]
fn second_month_updates_index_newest_first() {
    let dir = setup("# P\n<!-- loom:plan-archive -->\n## A\na\n");
    archive_plan(dir.path(), NaiveDate::from_ymd_opt(2026, 9, 3).unwrap(), false).unwrap();
    let mut active = read(dir.path(), "WORK_PLAN.md");
    active.push_str("<!-- loom:plan-archive -->\n## B\nb\n");
    fs::write(dir.path().join("WORK_PLAN.md"), &active).unwrap();
    run(dir.path()).unwrap();
    let active = read(dir.path(), "WORK_PLAN.md");
    assert!(active.contains(
        "## Archives\n\n- [2026-10](work_plan/WORK_PLAN_2026-10.md)\n- [2026-09](work_plan/WORK_PLAN_2026-09.md)\n"
    ), "{active}");
    assert_eq!(active.matches("## Archives").count(), 1);
}

#[test]
fn same_month_appends_and_identical_section_conflicts() {
    let dir = setup("# P\n<!-- loom:plan-archive -->\n## A\na\n");
    run(dir.path()).unwrap();
    let mut active = read(dir.path(), "WORK_PLAN.md");
    active.push_str("<!-- loom:plan-archive -->\n## B\nb\n");
    fs::write(dir.path().join("WORK_PLAN.md"), &active).unwrap();
    run(dir.path()).unwrap();
    let archived = read(dir.path(), "work_plan/WORK_PLAN_2026-10.md");
    assert!(archived.contains("## A\na\n") && archived.ends_with("## B\nb\n"));

    // Re-adding an already archived section is refused, nothing written.
    let mut again = read(dir.path(), "WORK_PLAN.md");
    again.push_str("<!-- loom:plan-archive -->\n## A\na\n");
    fs::write(dir.path().join("WORK_PLAN.md"), &again).unwrap();
    assert!(run(dir.path()).is_err());
    assert_eq!(read(dir.path(), "WORK_PLAN.md"), again);
}

#[test]
fn dry_run_writes_nothing() {
    let text = "# P\n<!-- loom:plan-archive -->\n## A\na\n";
    let dir = setup(text);
    let msg = archive_plan(dir.path(), today(), true).unwrap();
    assert!(msg.starts_with("dry run:"), "{msg}");
    assert_eq!(read(dir.path(), "WORK_PLAN.md"), text);
    assert!(!dir.path().join("work_plan").exists());
}

#[test]
fn check_rejects_archive_with_unmarked_content_and_broken_index() {
    let dir = setup("# P\n<!-- loom:plan-archive -->\n## A\na\n");
    run(dir.path()).unwrap();
    fs::write(dir.path().join("work_plan/WORK_PLAN_2026-10.md"), "# x\n\n## stray\n").unwrap();
    assert!(check_plan(&root_args(dir.path())).is_err());

    let dir = setup("# P\n\n## Archives\n\n- [2026-01](work_plan/WORK_PLAN_2026-01.md)\n");
    assert!(check_plan(&root_args(dir.path())).is_err());
}

#[test]
fn cli_parses_plan_flag() {
    use crate::cli::whole_cli_parse::try_parse_cli;
    let Ok(cli) = try_parse_cli(&["loom-daemon", "work-docs", "archive", "--plan"]) else {
        panic!("did not parse");
    };
    match cli.command {
        Some(crate::Commands::ScriptPorts(
            crate::cli::script_ports::ScriptPortCommand::WorkDocs(
                super::super::work_docs_archive::WorkDocsCommand::Archive(a),
            ),
        )) => assert!(a.plan),
        _ => panic!("parsed to another command"),
    }
}
