//! Whole passes: a star reaches every child the edge resolver links, in the
//! in-memory inherit path (#10012 AC 1's ordering half, AC 3, 6, 7).

use super::fake::{issue, issue_with_body, repo_input, settings, t, tick_row, Host, World, STAR};
use crate::star_liveness::{inherit, Settings};
use crate::types::{QueueDisposition, StarLivenessReport};

fn starred(input: &mut crate::star_liveness::task::RepoInput, n: u32, at: &str) {
    let mut row = tick_row(&input.root, n, QueueDisposition::Parked);
    row.operator_priority_at = Some(at.into());
    input.tick_rows.push(row);
}

fn inherited(r: &StarLivenessReport) -> Vec<(u32, Option<u32>, Option<String>)> {
    let mut v: Vec<_> = r
        .rows
        .iter()
        .filter(|row| row.inherited_from.is_some())
        .map(|row| (row.issue, row.inherited_from, row.operator_priority_at.clone()))
        .collect();
    v.sort();
    v
}

const AT: &str = "2026-09-28T09:00:00Z";

/// loom-ui#1177's shape: a starred P park-records three children and goes
/// `loom:blocked`. All three inherit P's star at P's starred-at.
#[test]
fn a_park_recorded_decomposition_passes_the_star_to_every_child() {
    let world = World::default();
    let slug = "i/park";
    let body = "Decomposed.\n<!-- loom:park Blocked by: #11 by=builder -->\n\
        <!-- loom:park Blocked by: #12 by=builder -->\n<!-- loom:park Blocked by: #13 by=builder -->\n";
    world.add(slug, issue_with_body(10, &[STAR, "loom:blocked"], body));
    for n in 11..=13 {
        world.add(slug, issue(n, &["loom:triage"]));
    }
    let mut input = repo_input(slug);
    starred(&mut input, 10, AT);
    let r = Host::new("host-a").pass(&world, &[input.clone()], Vec::new(), t(10, 0));
    let a = Some(AT.to_string());
    assert_eq!(
        inherited(&r),
        vec![
            (11, Some(10), a.clone()),
            (12, Some(10), a.clone()),
            (13, Some(10), a)
        ]
    );
    let mut items = Vec::new();
    inherit::apply(Some(&input.root), &mut items);
    let mut got: Vec<u32> = items.iter().map(|i| i.number).collect();
    got.sort_unstable();
    assert_eq!(got, vec![11, 12, 13], "the work finder sees all three");
}

/// AC 3: blocked by #9 and #10 means both inherit (text order picked #10).
#[test]
fn both_blockers_inherit_regardless_of_text_order() {
    let world = World::default();
    let slug = "i/two";
    world.add(slug, issue_with_body(5, &[STAR, "loom:blocked"], "Blocked by #9 and #10\n"));
    world.add(slug, issue(9, &["loom:issue"]));
    world.add(slug, issue(10, &["loom:issue"]));
    let mut input = repo_input(slug);
    starred(&mut input, 5, AT);
    let r = Host::new("host-a").pass(&world, &[input], Vec::new(), t(10, 0));
    let got: Vec<u32> = inherited(&r).into_iter().map(|(n, _, _)| n).collect();
    assert_eq!(got, vec![9, 10]);
}

/// AC 7 (and the propagate switch): a task-list chain four deep reaches
/// depth 3 only, a cycle terminates, a closed or cross-repo child is untouched.
#[test]
fn a_task_list_chain_stops_at_depth_three_and_ignores_other_repos() {
    let world = World::default();
    let slug = "i/chain";
    world.add(
        slug,
        issue_with_body(1, &[STAR, "loom:issue"], "- [ ] #2\n- [ ] other/repo#7\n- [ ] #6\n"),
    );
    world.add(slug, issue_with_body(2, &["loom:issue"], "- [ ] #3\n"));
    world.add(slug, issue_with_body(3, &["loom:issue"], "- [ ] #4\n- [ ] #1\n"));
    world.add(slug, issue_with_body(4, &["loom:issue"], "- [ ] #5\n"));
    world.add(slug, issue(5, &["loom:issue"]));
    let mut closed = issue(6, &["loom:issue"]);
    closed.state = "closed".into();
    world.add(slug, closed);
    let mut input = repo_input(slug);
    starred(&mut input, 1, AT);
    let r = Host::new("host-a").pass(&world, &[input.clone()], Vec::new(), t(10, 0));
    let got: Vec<(u32, Option<u32>)> = inherited(&r).into_iter().map(|(n, f, _)| (n, f)).collect();
    assert_eq!(got, vec![(2, Some(1)), (3, Some(2)), (4, Some(3))]);

    let off = Settings {
        propagate: false,
        ..settings()
    };
    let r = Host::new("host-b").pass_with(&world, &[input], Vec::new(), t(10, 0), off);
    assert!(inherited(&r).is_empty(), "propagate off: only liveness blockers inherit");
}

/// AC 6: a child of two starred parents inherits the earlier star.
#[test]
fn a_shared_child_takes_the_earlier_parents_star() {
    let world = World::default();
    let slug = "i/shared";
    world.add(slug, issue_with_body(1, &[STAR, "loom:issue"], "- [ ] #3\n"));
    world.add(slug, issue_with_body(2, &[STAR, "loom:issue"], "- [ ] #3\n"));
    world.add(slug, issue(3, &["loom:issue"]));
    let mut input = repo_input(slug);
    starred(&mut input, 1, "2026-09-28T10:00:00Z");
    starred(&mut input, 2, "2026-09-28T08:00:00Z");
    let r = Host::new("host-a").pass(&world, &[input], Vec::new(), t(10, 0));
    assert_eq!(inherited(&r), vec![(3, Some(2), Some("2026-09-28T08:00:00Z".to_string()))]);
}

/// The walk's cap counts forge reads, not open children found: a task list
/// of 60 closed children costs at most `MAX_WALK_READS_PER_PASS` reads, and
/// the blocker reads of a blocked child count against it too (#10073 review).
#[test]
fn the_walk_caps_forge_reads_counting_closed_children_and_blocker_reads() {
    use crate::star_liveness::collect::MAX_WALK_READS_PER_PASS;
    let world = World::default();
    let slug = "i/cap";
    // Open, blocked children first (lower numbers), each blocked by one
    // closed issue, then 60 closed children.
    let mut list = String::new();
    for n in 10..=12 {
        list.push_str(&format!("- [ ] #{n}\n"));
        let blocked = issue_with_body(n, &["loom:blocked"], &format!("Blocked by #{}\n", n + 290));
        world.add(slug, blocked);
        let mut b = issue(n + 290, &["loom:issue"]);
        b.state = "closed".into();
        world.add(slug, b);
    }
    for n in 100..160 {
        list.push_str(&format!("- [x] #{n}\n"));
        let mut c = issue(n, &["loom:issue"]);
        c.state = "closed".into();
        world.add(slug, c);
    }
    world.add(slug, issue_with_body(1, &[STAR, "loom:issue"], &list));
    let mut input = repo_input(slug);
    starred(&mut input, 1, AT);
    let r = Host::new("host-a").pass(&world, &[input], Vec::new(), t(10, 0));
    let got: Vec<u32> = inherited(&r).into_iter().map(|(n, _, _)| n).collect();
    assert_eq!(got, vec![10, 11, 12], "the open children are reached first and inherit");
    let reads = world.repo(slug).issue_reads;
    assert_eq!(
        reads, MAX_WALK_READS_PER_PASS,
        "3 children + 3 blocker reads + 44 closed children, then the walk stops"
    );
}

/// A child whose blocker reads would cross the cap waits; the cap still holds.
#[test]
fn a_child_whose_blocker_reads_would_cross_the_cap_waits() {
    use crate::star_liveness::collect::MAX_WALK_READS_PER_PASS;
    let world = World::default();
    let slug = "i/cap2";
    let mut list = String::new();
    // 49 closed children, then one open child with two unread blockers.
    for n in 100..149 {
        list.push_str(&format!("- [x] #{n}\n"));
        let mut c = issue(n, &["loom:issue"]);
        c.state = "closed".into();
        world.add(slug, c);
    }
    list.push_str("- [ ] #200\n");
    world.add(slug, issue_with_body(200, &["loom:blocked"], "Blocked by #301 and #302\n"));
    world.add(slug, issue(301, &["loom:issue"]));
    world.add(slug, issue(302, &["loom:issue"]));
    world.add(slug, issue_with_body(1, &[STAR, "loom:issue"], &list));
    let mut input = repo_input(slug);
    starred(&mut input, 1, AT);
    let r = Host::new("host-a").pass(&world, &[input], Vec::new(), t(10, 0));
    assert!(inherited(&r).is_empty(), "#200 waits for a later pass");
    assert_eq!(world.repo(slug).issue_reads, MAX_WALK_READS_PER_PASS);
}

/// Repeated passes over unchanged forge data make progress: closed children
/// the capped walk already read are not read again, so an open child past
/// them is reached on a later pass, with its descendants, and every pass
/// stays within the read cap (#10073 review).
#[test]
fn repeated_passes_reach_a_child_behind_a_cap_of_closed_children() {
    use crate::star_liveness::collect::MAX_WALK_READS_PER_PASS;
    let world = World::default();
    let slug = "i/cap3";
    let mut list = String::new();
    for n in 100..160 {
        list.push_str(&format!("- [x] #{n}\n"));
        let mut c = issue(n, &["loom:issue"]);
        c.state = "closed".into();
        world.add(slug, c);
    }
    list.push_str("- [ ] #200\n");
    world.add(slug, issue_with_body(200, &["loom:issue"], "- [ ] #201\n"));
    world.add(slug, issue(201, &["loom:issue"]));
    world.add(slug, issue_with_body(1, &[STAR, "loom:issue"], &list));
    let mut input = repo_input(slug);
    starred(&mut input, 1, AT);
    let mut host = Host::new("host-a");
    let mut reached = None;
    let mut before = world.repo(slug).issue_reads;
    for pass in 0..4 {
        let r = host.pass(&world, &[input.clone()], Vec::new(), t(10, pass));
        let after = world.repo(slug).issue_reads;
        assert!(after - before <= MAX_WALK_READS_PER_PASS, "pass {pass} stays within the cap");
        before = after;
        let got: Vec<u32> = inherited(&r).into_iter().map(|(n, _, _)| n).collect();
        if got.contains(&200) {
            assert!(got.contains(&201), "the deferred child's descendants inherit too");
            reached = Some(pass);
            break;
        }
    }
    assert_eq!(reached, Some(1), "#200 inherits on the second pass");
}

/// A child whose blocker reads do not fit the budget on one pass fits on a
/// later one, because the closed children before it no longer cost reads.
#[test]
fn repeated_passes_reach_a_child_whose_blocker_reads_did_not_fit() {
    use crate::star_liveness::collect::MAX_WALK_READS_PER_PASS;
    let world = World::default();
    let slug = "i/cap4";
    let mut list = String::new();
    for n in 100..149 {
        list.push_str(&format!("- [x] #{n}\n"));
        let mut c = issue(n, &["loom:issue"]);
        c.state = "closed".into();
        world.add(slug, c);
    }
    list.push_str("- [ ] #200\n");
    world.add(slug, issue_with_body(200, &["loom:blocked"], "Blocked by #301 and #302\n"));
    world.add(slug, issue(301, &["loom:issue"]));
    world.add(slug, issue(302, &["loom:issue"]));
    world.add(slug, issue_with_body(1, &[STAR, "loom:issue"], &list));
    let mut input = repo_input(slug);
    starred(&mut input, 1, AT);
    let mut host = Host::new("host-a");
    let first = host.pass(&world, &[input.clone()], Vec::new(), t(10, 0));
    assert!(inherited(&first).is_empty(), "#200 waits on the first pass");
    let before = world.repo(slug).issue_reads;
    let second = host.pass(&world, &[input], Vec::new(), t(10, 1));
    assert!(world.repo(slug).issue_reads - before <= MAX_WALK_READS_PER_PASS);
    let got: Vec<u32> = inherited(&second).into_iter().map(|(n, _, _)| n).collect();
    assert_eq!(
        got,
        vec![200, 301, 302],
        "#200 inherits once the settled children are free, and so do its open blockers"
    );
}
