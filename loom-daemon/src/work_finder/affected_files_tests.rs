//! Coverage for the same-tick affected-files overlap gate (#9781). The
//! tick-level cases drive `tick_multi_with_sharding` with minimal fakes.

use std::collections::{BTreeSet, HashSet};

use super::super::*;
use super::{affected_surface, overlap, OverlapGate};
use crate::telemetry::kinds::pick_decision::PickSkipReason;

fn body(paths: &[&str]) -> String {
    let list: String = paths.iter().map(|p| format!("- `{p}` — edit\n")).collect();
    format!("intro\n\n### Affected Files\n\n{list}\n### Other\n\n- `ignored/outside.rs`\n")
}

fn set(paths: &[&str]) -> BTreeSet<String> {
    paths.iter().map(|p| (*p).to_string()).collect()
}

struct Src(Option<Vec<WorkItem>>);

impl WorkSource for Src {
    fn list_ready_issues(&mut self) -> Result<Vec<WorkItem>> {
        Ok(self.0.take().unwrap_or_default())
    }
}

#[derive(Default)]
struct Disp {
    dispatched: Vec<u32>,
    in_flight: HashSet<u32>,
}

impl WorkDispatcher for Disp {
    fn in_flight(&self) -> HashSet<u32> {
        self.in_flight.clone()
    }
    fn occupancy(&self) -> usize {
        self.dispatched.len()
    }
    fn dispatch(&mut self, issue: u32) -> Result<bool> {
        self.dispatched.push(issue);
        Ok(true)
    }
}

fn item(n: u32, body: Option<String>, extra_label: Option<&str>) -> WorkItem {
    let mut labels = vec!["loom:issue".to_string()];
    labels.extend(extra_label.map(str::to_string));
    let mut it = WorkItem::new(n, labels);
    it.body = body;
    it
}

fn run(workspaces: &mut [(Src, Disp)]) -> TickReport {
    let halted = vec![false; workspaces.len()];
    let prios = vec![1; workspaces.len()];
    tick_multi_with_sharding(workspaces, &prios, 10, &halted, usize::MAX, false)
}

fn row(report: &TickReport, n: u32) -> Option<(Qd, Option<String>)> {
    report
        .queue
        .iter()
        .find(|r| r.key.number == n)
        .and_then(|r| r.disposition.map(|d| (d, r.detail.clone())))
}

#[test]
fn parser_extracts_backticked_paths_and_treats_the_rest_as_unknown() {
    let s = affected_surface(&body(&["a/b.rs", "c.md"])).unwrap();
    assert_eq!(s, set(&["a/b.rs", "c.md"]), "section only; identifiers excluded");
    assert_eq!(affected_surface("### Affected Files\n\nTo be determined\n"), None);
    assert_eq!(affected_surface("no section, `a/b.rs` here"), None);
    assert_eq!(affected_surface("## Affected Files\n\n- a/b.rs unquoted\n"), None);
    assert_eq!(affected_surface("## Affected Files\n- `shape_queue` and `Qd::X`\n"), None);
    assert_eq!(
        affected_surface("## Affected Files\n- `src/x.rs:120`\n").unwrap(),
        set(&["src/x.rs"])
    );
}

#[test]
fn slashless_spans_need_a_known_file_extension() {
    let s = affected_surface(
        "## Affected Files\n- `WorkItem.body` via `PickSkipReason.ALL` since `v0.19.1`\n- `CLAUDE.md`, `Cargo.toml`\n",
    )
    .unwrap();
    assert_eq!(s, set(&["CLAUDE.md", "Cargo.toml"]), "identifiers and versions are not paths");
    assert_eq!(affected_surface("## Affected Files\n- `WorkItem.body`\n"), None);
    assert_eq!(
        affected_surface("## Affected Files\n- `src/Type.weird`\n").unwrap(),
        set(&["src/Type.weird"]),
        "a `/` alone makes it a path"
    );
}

#[test]
fn overlap_is_the_sorted_intersection() {
    assert_eq!(overlap(&set(&["b", "a", "c"]), &set(&["c", "a", "z"])), vec!["a", "c"]);
}

#[test]
fn intersecting_same_repo_candidates_defer_the_later_one() {
    let items = vec![
        item(1, Some(body(&["x/a.rs", "x/b.rs"])), None),
        item(2, Some(body(&["x/b.rs", "x/c.rs"])), None),
    ];
    let mut ws = [(Src(Some(items)), Disp::default())];
    let report = run(&mut ws);
    assert_eq!(ws[0].1.dispatched, vec![1]);
    let (d, detail) = row(&report, 2).unwrap();
    assert_eq!(d, Qd::DeferredFileOverlap);
    assert!(detail.unwrap().contains("x/b.rs"));
    assert_eq!(PickSkipReason::from_disposition(d), Some(PickSkipReason::OverlapChain));
    assert_eq!(d.state(), "ready", "waits, never blocked or labelled");
}

#[test]
fn disjoint_candidates_both_dispatch() {
    let items = vec![
        item(1, Some(body(&["a.rs"])), None),
        item(2, Some(body(&["b.rs"])), None),
    ];
    let mut ws = [(Src(Some(items)), Disp::default())];
    run(&mut ws);
    assert_eq!(ws[0].1.dispatched, vec![1, 2]);
}

#[test]
fn unknown_surface_dispatches_and_never_blocks() {
    let items = vec![
        item(1, None, None),
        item(2, Some(body(&["a.rs"])), None),
        item(3, Some("### Affected Files\nTo be determined\n".into()), None),
        item(4, Some(body(&["a.rs"])), None),
    ];
    let mut ws = [(Src(Some(items)), Disp::default())];
    let report = run(&mut ws);
    assert_eq!(ws[0].1.dispatched, vec![1, 2, 3]);
    assert_eq!(row(&report, 4).unwrap().0, Qd::DeferredFileOverlap);
}

#[test]
fn listed_building_item_occupies_its_surface() {
    let items = vec![
        item(9, Some(body(&["a.rs"])), Some("loom:building")),
        item(1, Some(body(&["a.rs"])), None),
        item(2, Some(body(&["z.rs"])), None),
    ];
    let mut disp = Disp::default();
    disp.in_flight.insert(9);
    let mut ws = [(Src(Some(items)), disp)];
    let report = run(&mut ws);
    assert_eq!(ws[0].1.dispatched, vec![2]);
    assert_eq!(row(&report, 1).unwrap().0, Qd::DeferredFileOverlap);
}

#[test]
fn listed_building_item_occupies_without_in_flight_membership() {
    let items = vec![
        item(9, Some(body(&["a.rs"])), Some("loom:building")),
        item(1, Some(body(&["a.rs"])), None),
        item(2, Some(body(&["z.rs"])), None),
    ];
    let mut ws = [(Src(Some(items)), Disp::default())];
    let report = run(&mut ws);
    assert_eq!(ws[0].1.dispatched, vec![2]);
    assert_eq!(row(&report, 1).unwrap().0, Qd::DeferredFileOverlap);
}

#[test]
fn identical_paths_in_different_repos_do_not_defer() {
    let a = vec![item(1, Some(body(&["a.rs"])), None)];
    let b = vec![item(2, Some(body(&["a.rs"])), None)];
    let mut ws = [
        (Src(Some(a)), Disp::default()),
        (Src(Some(b)), Disp::default()),
    ];
    run(&mut ws);
    assert_eq!(ws[0].1.dispatched, vec![1]);
    assert_eq!(ws[1].1.dispatched, vec![2]);
}

/// #11508 (operator ruling): priority never exempts a candidate from the
/// overlap gate. A `loom:very-important` candidate sharing `a.rs` with an
/// in-flight sweep is deferred exactly like a plain one; the plain candidate
/// behind it on a free surface is dispatched.
#[test]
fn a_very_important_candidate_is_deferred_by_the_overlap_gate_like_any_other() {
    let items = vec![
        item(9, Some(body(&["a.rs"])), Some("loom:building")),
        item(1, Some(body(&["b.rs"])), None),
        item(2, Some(body(&["a.rs", "b.rs"])), Some("loom:very-important")),
    ];
    let mut disp = Disp::default();
    disp.in_flight.insert(9);
    let mut ws = [(Src(Some(items)), disp)];
    let report = run(&mut ws);
    assert_eq!(ws[0].1.dispatched, vec![1]);
    let (d, detail) = row(&report, 2).unwrap();
    assert_eq!(d, Qd::DeferredFileOverlap, "very-important waits on the occupied `a.rs`");
    assert!(detail.unwrap().contains("a.rs"));
}

fn cand(number: u32, level: crate::priority_pick::Level) -> PriorityCandidate {
    PriorityCandidate {
        workspace_idx: 0,
        workspace_weight: 0,
        created_at: None,
        number,
        level,
        main_red_fix: false,
    }
}

#[test]
fn the_gate_reports_overlap_for_every_level() {
    use crate::priority_pick::Level;
    let mut gate = OverlapGate::default();
    gate.note(0, 9, Some(&body(&["a.rs"])), true);
    gate.note(0, 1, Some(&body(&["a.rs", "b.rs"])), false);
    gate.note(0, 2, Some(&body(&["b.rs"])), false);
    for level in [Level::Default, Level::Important, Level::VeryImportant] {
        assert_eq!(gate.overlapping(&cand(1, level)), vec!["a.rs"], "{level:?}");
    }
    gate.occupy(&cand(1, Level::VeryImportant));
    assert_eq!(gate.overlapping(&cand(2, Level::Default)), vec!["b.rs"]);
}

fn red_fix(n: u32, paths: &[&str]) -> WorkItem {
    let body = format!("{}\n<!-- loom:main-red-fix -->\n", body(paths));
    item(n, Some(body), None)
}

fn run_lane(workspaces: &mut [(Src, Disp)], halted: bool, lane: RedMainLane) -> TickReport {
    tick_multi_with_repo_cap(
        workspaces,
        &[1],
        10.into(),
        &[halted],
        None,
        usize::MAX,
        false,
        None,
        &[lane],
    )
}

/// #11508 (Judge): the red-main-fix lane (#9244) is not a level; it keeps
/// its overlap bypass. On a verified-red `main` the in-flight work it
/// shares `a.rs` with cannot merge, so deferring the fix would deadlock;
/// a very-important candidate on the same surface is still deferred.
#[test]
fn a_verified_red_main_fix_bypasses_the_overlap_gate_but_very_important_does_not() {
    let red = RedMainLane {
        verified_red: true,
        gate_disabled: false,
        other_hold: false,
    };
    let items = vec![
        item(9, Some(body(&["a.rs"])), Some("loom:building")),
        red_fix(4, &["a.rs"]),
    ];
    let mut disp = Disp::default();
    disp.in_flight.insert(9);
    let mut ws = [(Src(Some(items)), disp)];
    let report = run_lane(&mut ws, true, red);
    assert_eq!(ws[0].1.dispatched, vec![4], "the verified fix is not held by `a.rs`");
    let row4 = report.queue.iter().find(|r| r.key.number == 4).unwrap();
    assert!(row4.key.main_red_fix);

    // On a green `main` the marker grants nothing: the fix waits on `a.rs`.
    let items = vec![
        item(9, Some(body(&["a.rs"])), Some("loom:building")),
        red_fix(4, &["a.rs"]),
        item(2, Some(body(&["a.rs"])), Some("loom:very-important")),
    ];
    let mut disp = Disp::default();
    disp.in_flight.insert(9);
    let mut ws = [(Src(Some(items)), disp)];
    let report = run_lane(&mut ws, false, RedMainLane::default());
    assert!(ws[0].1.dispatched.is_empty());
    assert_eq!(row(&report, 4).unwrap().0, Qd::DeferredFileOverlap);
    assert_eq!(row(&report, 2).unwrap().0, Qd::DeferredFileOverlap);
    assert!(report.queue.iter().all(|r| !r.key.main_red_fix));
}

#[test]
fn a_red_main_fix_candidate_skips_the_gate_but_still_occupies() {
    use crate::priority_pick::Level;
    let mut gate = OverlapGate::default();
    gate.note(0, 9, Some(&body(&["a.rs"])), true);
    gate.note(0, 1, Some(&body(&["a.rs", "b.rs"])), false);
    gate.note(0, 2, Some(&body(&["b.rs"])), false);
    let fix = PriorityCandidate {
        main_red_fix: true,
        ..cand(1, Level::Default)
    };
    assert!(gate.overlapping(&fix).is_empty());
    assert_eq!(gate.overlapping(&cand(1, Level::Default)), vec!["a.rs"]);
    gate.occupy(&fix);
    assert_eq!(gate.overlapping(&cand(2, Level::VeryImportant)), vec!["b.rs"]);
}

#[test]
fn extensionless_root_files_are_paths_but_identifiers_are_not() {
    let s = affected_surface(
        "## Affected Files\n- `Dockerfile`, `Makefile`, `VERSION`, `./LICENSE`\n- `version`, `Makefiles`, `dockerfile`\n",
    )
    .unwrap();
    assert_eq!(s, set(&["Dockerfile", "Makefile", "VERSION", "LICENSE"]));
    assert_eq!(affected_surface("## Affected Files\n- `version` and `Makefiles`\n"), None);
}

#[test]
fn candidates_sharing_an_extensionless_root_file_defer_the_later_one() {
    let items = vec![
        item(1, Some(body(&["Dockerfile"])), None),
        item(2, Some(body(&["Dockerfile"])), None),
    ];
    let mut ws = [(Src(Some(items)), Disp::default())];
    let report = run(&mut ws);
    assert_eq!(ws[0].1.dispatched, vec![1]);
    let (d, detail) = row(&report, 2).unwrap();
    assert_eq!(d, Qd::DeferredFileOverlap);
    assert!(detail.unwrap().contains("Dockerfile"));
}
