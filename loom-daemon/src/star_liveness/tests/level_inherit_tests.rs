//! Blocker inheritance of operator priority levels (#10307 §3–§4): the
//! derived `loom:high-priority-inherited` label, across managed repos, with
//! provenance, removal, the cap and the operator-first digest.

use std::collections::HashMap;
use std::path::Path;

use super::fake::{issue, issue_with_body, pr, repo_input, settings, t, Host, World, STAR};
use crate::operator_levels::{PriorityLevel, LEVELS};
use crate::star_liveness::levels::{self, Key};
use crate::star_liveness::{inherit, render, Settings};
use crate::types::{AskKind, LandingStage};

const HIGH: &str = "loom:operator-high-priority";
const INH: &str = "loom:high-priority-inherited";

fn has(world: &World, slug: &str, n: u32, label: &str) -> bool {
    world.repo(slug).items[&n].labels.iter().any(|l| l == label)
}

fn provenance(world: &World, slug: &str, n: u32) -> Vec<String> {
    world
        .posted(slug)
        .into_iter()
        .filter(|(num, body)| *num == n && body.contains("inherited_from="))
        .map(|(_, body)| body)
        .collect()
}

/// The AC scenario: A (level 2) is blocked by B (same repo) and C (another
/// managed repo). One pass labels both, with provenance; removing A's label
/// removes both; B keeps its label while it also blocks level-2 D.
#[test]
fn a_level_two_issue_hands_its_level_to_every_blocker_across_repos_and_takes_it_back() {
    let world = World::default();
    world.add(
        "o/app",
        issue_with_body(
            1,
            &[HIGH, "loom:issue", "loom:blocked"],
            "Blocked by #2\nDepends on o/lib#3\n",
        ),
    );
    world.add("o/app", issue(2, &["loom:issue"]));
    world.add("o/lib", issue(3, &["loom:triage"]));
    let repos = vec![repo_input("o/app"), repo_input("o/lib")];
    let mut a = Host::new("host-a");
    let r = a.pass(&world, &repos, Vec::new(), t(20, 0));

    assert!(has(&world, "o/app", 2, INH), "same-repo blocker");
    assert!(has(&world, "o/lib", 3, INH), "cross-repo blocker in a managed repo");
    for (slug, n) in [("o/app", 2), ("o/lib", 3)] {
        let p = provenance(&world, slug, n);
        assert_eq!(p.len(), 1, "{slug}#{n}: one provenance comment");
        assert!(p[0].contains("inherited_from=o/app#1 level=2"), "{}", p[0]);
        assert!(p[0].contains("label=loom:high-priority-inherited"), "{}", p[0]);
        assert!(!has(&world, slug, n, HIGH), "the operator's label is never copied");
    }
    // A's row: a level-2 cross-repo blocker in a managed repo is an agent wait.
    let row_a = r
        .rows
        .iter()
        .find(|r| r.repo == "o/app" && r.issue == 1)
        .unwrap();
    assert_eq!(row_a.level, 2);
    assert_eq!(row_a.stage, LandingStage::BlockedBy);
    assert!(row_a.ask.is_none(), "{:?}", row_a.ask);
    // The blockers are rows too, with the "via" source.
    let row_c = r
        .rows
        .iter()
        .find(|r| r.repo == "o/lib" && r.issue == 3)
        .unwrap();
    assert_eq!(row_c.level, 2);
    assert_eq!(row_c.level_inherited_from.as_deref(), Some("o/app#1"));
    // The digest orders level 2 first.
    assert_eq!(r.rows[0].level, 2);

    // A second pass writes nothing more.
    let writes = |w: &World| {
        let app = w.repo("o/app").label_writes.len();
        app + w.repo("o/lib").label_writes.len()
    };
    let before = writes(&world);
    a.pass(&world, &repos, Vec::new(), t(20, 2));
    assert_eq!(writes(&world), before);

    // D (level 2) is also blocked by B.
    world.add(
        "o/app",
        issue_with_body(4, &[HIGH, "loom:issue", "loom:blocked"], "Blocked by #2\n"),
    );
    // The operator drops A's level.
    world
        .repo("o/app")
        .items
        .get_mut(&1)
        .unwrap()
        .labels
        .retain(|l| l != HIGH);
    a.pass(&world, &repos, Vec::new(), t(20, 4));
    assert!(has(&world, "o/app", 2, INH), "B still blocks level-2 D");
    assert!(!has(&world, "o/lib", 3, INH), "C no longer blocks any level-2 issue");

    // D drops too: B loses it.
    world
        .repo("o/app")
        .items
        .get_mut(&4)
        .unwrap()
        .labels
        .retain(|l| l != HIGH);
    a.pass(&world, &repos, Vec::new(), t(20, 6));
    assert!(!has(&world, "o/app", 2, INH));
}

#[test]
fn every_open_blocker_inherits_transitively_and_a_cycle_terminates() {
    let world = World::default();
    let slug = "o/chain";
    world.add(
        slug,
        issue_with_body(1, &[HIGH, "loom:blocked"], "Blocked by #2\nBlocked by #5\n"),
    );
    world.add(slug, issue_with_body(2, &["loom:blocked"], "Blocked by #3\n"));
    world.add(slug, issue_with_body(3, &["loom:blocked"], "Blocked by #2\nBlocked by #1\n"));
    world.add(slug, issue(5, &["loom:issue"]));
    let mut closed = issue(6, &["loom:issue"]);
    closed.state = "closed".into();
    world.add(slug, closed);
    world.repo(slug).items.get_mut(&5).unwrap().body = Some("Blocked by #6\n".into());
    Host::new("h").pass(&world, &[repo_input(slug)], Vec::new(), t(20, 0));
    for n in [2, 3, 5] {
        assert!(has(&world, slug, n, INH), "#{n}");
    }
    assert!(!has(&world, slug, 6, INH), "a closed blocker drops out");
    assert!(!has(&world, slug, 1, INH), "the source keeps only its own label");
}

#[test]
fn a_level_does_not_flow_to_an_epics_children_through_containment() {
    let world = World::default();
    let slug = "o/epic";
    world.add(
        slug,
        issue_with_body(
            10,
            &[HIGH, "loom:epic"],
            "Phases:\n- [ ] #11\n- [ ] #12\n<!-- loom:park Blocked by: #13 -->\n",
        ),
    );
    world.add(
        slug,
        issue_with_body(11, &["loom:issue"], "<!-- loom:parent #10 -->\nPart of #10\n"),
    );
    world.add(slug, issue(12, &["loom:issue"]));
    world.add(slug, issue(13, &["loom:issue"]));
    Host::new("h").pass(&world, &[repo_input(slug)], Vec::new(), t(20, 0));
    assert!(!has(&world, slug, 11, INH), "task list / parent marker: containment");
    assert!(!has(&world, slug, 12, INH), "task list: containment");
    assert!(has(&world, slug, 13, INH), "a park record is an explicit blocker");
}

#[test]
fn native_blocked_by_dependencies_are_edges() {
    let world = World::default();
    world.add("o/n", issue(1, &[HIGH]));
    world.add("o/m", issue(7, &[]));
    world
        .repo("o/n")
        .blocked_by
        .insert(1, vec![("o/m".to_string(), 7)]);
    Host::new("h").pass(&world, &[repo_input("o/n"), repo_input("o/m")], Vec::new(), t(20, 0));
    assert!(has(&world, "o/m", 7, INH));
}

#[test]
fn an_unmanaged_blocker_is_reported_not_followed_and_the_star_still_does_not_cross() {
    let world = World::default();
    world.add("o/u", issue_with_body(1, &[HIGH, "loom:blocked"], "Blocked by far/away#9\n"));
    world.add("o/u", issue_with_body(2, &[STAR, "loom:blocked"], "Blocked by o/v#4\n"));
    world.add("o/v", issue(4, &[]));
    let r =
        Host::new("h").pass(&world, &[repo_input("o/u"), repo_input("o/v")], Vec::new(), t(20, 0));
    assert_eq!(r.unfollowed_blockers, vec!["far/away#9".to_string()]);
    assert!(render::lines(Some(&r))
        .iter()
        .any(|l| l.contains("not followed: far/away#9")));
    assert!(!has(&world, "o/v", 4, INH), "a plain star never labels across repos");
    let star_row = r.rows.iter().find(|r| r.issue == 2).unwrap();
    assert_eq!(
        star_row.ask.as_ref().map(|a| a.kind),
        Some(AskKind::BlockedCrossRepo),
        "the star keeps its no-cross-repo rule"
    );
}

#[test]
fn an_operator_only_or_decision_blocker_leads_the_digest() {
    let world = World::default();
    let slug = "o/ops";
    world.add(slug, issue(1, &[STAR, "loom:issue"]));
    world.add(
        slug,
        issue_with_body(2, &[HIGH, "loom:blocked"], "Blocked by #3\nBlocked by #4\n"),
    );
    world.add(slug, issue(3, &["loom:operator-decision"]));
    world.add(slug, issue(4, &["loom:issue"]));
    let r = Host::new("h").pass(&world, &[repo_input(slug)], Vec::new(), t(20, 0));
    let first = &r.rows[0];
    assert_eq!(first.issue, 3, "{:?}", r.rows.iter().map(|r| r.issue).collect::<Vec<_>>());
    assert_eq!(first.level_inherited_from.as_deref(), Some("o/ops#2"));
    assert_eq!(first.ask.as_ref().map(|a| a.kind), Some(AskKind::OperatorDecision));
    let lines = render::lines(Some(&r));
    assert!(lines[1].contains("o/ops#3"), "{lines:?}");
    assert!(lines[1].contains("inherits ⭐⭐ from o/ops#2"), "{lines:?}");
    // The escalation names the level, not a star the issue does not carry.
    let ask = world
        .posted(slug)
        .into_iter()
        .find(|(n, b)| *n == 3 && b.contains("Operator needed"))
        .unwrap();
    assert!(
        ask.1
            .contains("blocks o/ops#2 and inherits its operator priority level"),
        "{}",
        ask.1
    );
}

#[test]
fn an_over_cap_level_is_reported_and_never_refused() {
    let world = World::default();
    let slug = "o/cap";
    for n in 1..=6 {
        world.add(slug, issue(n, &[HIGH, "loom:issue"]));
    }
    let r = Host::new("h").pass(&world, &[repo_input(slug)], Vec::new(), t(20, 0));
    assert_eq!(r.over_cap.len(), 1);
    assert_eq!((r.over_cap[0].count, r.over_cap[0].cap), (6, 5));
    assert_eq!(r.over_cap[0].label, HIGH);
    for n in 1..=6 {
        assert!(has(&world, slug, n, HIGH), "#{n}: never refused");
    }
    let lines = render::lines(Some(&r));
    assert!(lines[1].contains("over cap: 6 open issues carry loom:operator-high-priority (cap 5)"));

    // Configurable per level.
    let s = Settings::from_block(Some(&serde_json::json!({"levelCaps": {"2": 10}})));
    assert_eq!(s.level_caps.cap(2), Some(10));
    let r = Host::new("h").pass_with(
        &world,
        &[repo_input(slug)],
        Vec::new(),
        t(20, 1),
        Settings {
            level_caps: s.level_caps,
            ..settings()
        },
    );
    assert!(r.over_cap.is_empty());
}

#[test]
fn an_incomplete_walk_adds_but_never_removes() {
    let world = World::default();
    world.add("o/x", issue(2, &["loom:issue", INH]));
    world.add("o/y", issue(1, &[HIGH]));
    world.repo("o/y").fail_listing = true;
    Host::new("h").pass(&world, &[repo_input("o/x"), repo_input("o/y")], Vec::new(), t(20, 0));
    assert!(has(&world, "o/x", 2, INH), "a source may be in the unreadable repo");
    world.repo("o/y").fail_listing = false;
    Host::new("h").pass(&world, &[repo_input("o/x"), repo_input("o/y")], Vec::new(), t(20, 1));
    assert!(!has(&world, "o/x", 2, INH), "a complete pass removes a stale label");
}

#[test]
fn a_pr_keeps_a_copied_inherited_label_only_while_its_issue_is_reached() {
    let world = World::default();
    let slug = "o/pr";
    world.add(slug, issue_with_body(1, &[HIGH, "loom:blocked"], "Blocked by #2\n"));
    world.add(slug, issue(2, &["loom:building"]));
    world.add(slug, pr(20, 2, &["loom:review-requested", INH]));
    let repos = [repo_input(slug)];
    Host::new("h").pass(&world, &repos, Vec::new(), t(20, 0));
    assert!(has(&world, slug, 20, INH));
    world
        .repo(slug)
        .items
        .get_mut(&1)
        .unwrap()
        .labels
        .retain(|l| l != HIGH);
    Host::new("h").pass(&world, &repos, Vec::new(), t(20, 1));
    assert!(!has(&world, slug, 20, INH));
    assert!(!has(&world, slug, 2, INH));
}

#[test]
fn with_writes_off_the_work_finder_still_sees_the_level_in_memory() {
    let world = World::default();
    let slug = "o/mem";
    world.add(slug, issue_with_body(1, &[HIGH, "loom:blocked"], "Blocked by #2\n"));
    world.add(slug, issue(2, &["loom:issue"]));
    let input = repo_input(slug);
    let off = Settings {
        escalate: false,
        ..settings()
    };
    Host::new("h").pass_with(&world, std::slice::from_ref(&input), Vec::new(), t(20, 0), off);
    assert!(!has(&world, slug, 2, INH), "no forge write");
    let mut items = vec![crate::work_finder::WorkItem::new(
        2,
        vec!["loom:issue".into()],
    )];
    inherit::apply(Some(&input.root), &mut items);
    assert_eq!(items[0].operator_level(), 2, "the in-memory twin carries level 2");
    inherit::publish_levels(&input.root, Vec::new());
}

/// #10307 AC: adding a level-3 row to the table is enough to inherit at
/// level 3. A node reached by levels 2 and 3 carries only the level-3
/// inherited label.
#[test]
fn a_level_three_row_is_enough_to_inherit_at_level_three() {
    let mut table: Vec<PriorityLevel> = LEVELS.to_vec();
    table.push(PriorityLevel {
        level: 3,
        glyph: "⭐⭐⭐",
        name: "operator top priority",
        operator_label: "loom:operator-top-priority",
        inherited_label: Some("loom:top-priority-inherited"),
        default_cap: Some(1),
    });
    let world = World::default();
    let slug = "o/top";
    world.add(
        slug,
        issue_with_body(1, &["loom:operator-top-priority", "loom:blocked"], "Blocked by #3\n"),
    );
    world.add(
        slug,
        issue_with_body(2, &[HIGH, "loom:blocked"], "Blocked by #3\nBlocked by #4\n"),
    );
    world.add(slug, issue(3, &[INH]));
    world.add(slug, issue(4, &[]));
    world.add(slug, issue(5, &["loom:operator-top-priority"]));
    let refs: Vec<levels::RepoRef> = vec![(slug.to_string(), repo_input(slug).root)];
    let w = world.clone();
    let mut open = move |_: &Path, s: &str| w.forge(s);
    let caps = |level: u8| crate::operator_levels::row(&table, level).and_then(|r| r.default_cap);
    let out = levels::run_with(
        &table,
        &refs,
        &mut open,
        &HashMap::<Key, Vec<u32>>::new(),
        &HashMap::new(),
        &caps,
        true,
        "h",
    );
    assert!(out.complete);
    assert!(has(&world, slug, 3, "loom:top-priority-inherited"), "level 3 wins at #3");
    assert!(!has(&world, slug, 3, INH), "the lower inherited label is taken off");
    assert!(has(&world, slug, 4, INH), "#4 only blocks level 2");
    assert!(!has(&world, slug, 4, "loom:top-priority-inherited"));
    assert_eq!(out.over_cap.len(), 1, "two level-3 sources over a cap of 1");
    assert_eq!(out.over_cap[0].level, 3);
    let p = provenance(&world, slug, 3);
    assert!(p[0].contains("level=3") && p[0].contains("⭐⭐⭐"), "{}", p[0]);
}

#[test]
fn provenance_ids_are_marker_safe_and_stable() {
    let id = levels::provenance_id(2, &("2AMLogic/loom-ui".into(), 9), &("o/x--y".into(), 1681));
    assert!(!id.contains("--") && !id.contains('/'), "{id}");
    assert_eq!(
        id,
        levels::provenance_id(2, &("2AMLogic/loom-ui".into(), 9), &("o/x--y".into(), 1681))
    );
}

#[test]
fn the_native_dependency_answer_keeps_open_issues_in_any_repo() {
    let body = r#"[
        {"number":7,"state":"open","html_url":"https://github.com/o/m/issues/7"},
        {"number":8,"state":"closed","html_url":"https://github.com/o/m/issues/8"},
        {"number":9,"state":"open","html_url":"https://github.com/o/n/pull/9","pull_request":{}},
        {"number":3,"state":"open","html_url":"https://github.com/O/N/issues/3"}
    ]"#;
    assert_eq!(
        crate::star_liveness::forge::parse_blocked_by(body, "o/n"),
        vec![("o/m".to_string(), 7), ("o/n".to_string(), 3)]
    );
    assert!(crate::star_liveness::forge::parse_blocked_by("{\"message\":\"x\"}", "o/n").is_empty());
}
