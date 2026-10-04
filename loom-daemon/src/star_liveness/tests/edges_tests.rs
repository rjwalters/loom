//! The parent/child edge resolver and its closure (#10012 §1, AC 2, 3, 6, 7).

use crate::star_liveness::collect::MAX_INHERIT_DEPTH;
use crate::star_liveness::edges::{
    child_edges, descendants, parent_edges, resolve, same_repo_number, Edge, EdgeSource, Node, Root,
};

const SLUG: &str = "o/r";

fn node<'a>(number: u32, title: &'a str, body: &'a str, labels: &'a [String]) -> Node<'a> {
    Node {
        number,
        title,
        body,
        labels,
        is_pull_request: false,
    }
}

fn pairs(edges: &[Edge]) -> Vec<(u32, u32, EdgeSource)> {
    edges
        .iter()
        .map(|e| (e.parent, e.child, e.source))
        .collect()
}

#[test]
fn a_park_record_links_its_children() {
    let body = "Decomposed.\n\
        <!-- loom:park Blocked by: #11 by=builder at=2026-10-01T00:00:00Z reason=\"see #99\" -->\n\
        <!-- loom:park Blocked by: #12, #13 -->\n";
    let e = child_edges(SLUG, &node(10, "", body, &[]));
    assert_eq!(
        pairs(&e),
        vec![
            (10, 11, EdgeSource::ParkRecord),
            (10, 12, EdgeSource::ParkRecord),
            (10, 13, EdgeSource::ParkRecord),
        ],
        "a `#N` inside the quoted reason is prose, not a child"
    );
}

#[test]
fn a_dependency_phrase_links_only_on_a_blocked_parent() {
    let body = "Blocked by #10\nDepends on #9\nRequires #12\n";
    let blocked = vec!["loom:blocked".to_string()];
    let e = child_edges(SLUG, &node(5, "", body, &blocked));
    assert_eq!(
        pairs(&e),
        vec![
            (5, 9, EdgeSource::BlockedBy),
            (5, 10, EdgeSource::BlockedBy),
            (5, 12, EdgeSource::BlockedBy),
        ],
        "ascending by number, not by text (`#10` < `#9` as text)"
    );
    assert!(child_edges(SLUG, &node(5, "", body, &[])).is_empty(), "not blocked: no edge");
}

#[test]
fn a_task_list_entry_links_checked_or_not() {
    let body =
        "## Phases\n- [ ] #21 first\n- [x] #22\n* [X] o/r#23 third\n- [ ] write docs for #24\n";
    let e = child_edges(SLUG, &node(20, "", body, &[]));
    assert_eq!(
        pairs(&e),
        vec![
            (20, 21, EdgeSource::TaskList),
            (20, 22, EdgeSource::TaskList),
            (20, 23, EdgeSource::TaskList),
        ],
        "an entry whose first token is not a reference is prose"
    );
}

#[test]
fn a_ticked_dependencies_item_is_not_a_child() {
    // #10024: `## Dependencies` is a dependency list, not containment — a
    // ticked item there is satisfied, so only the unchecked one links. The
    // same ticked number listed again outside the section is still a child.
    let body = "## Dependencies\n- [ ] #31 prerequisite\n- [x] #32 done\n- [X] #33 done\n\n\
        ## Phases\n- [x] #33 phase\n- [x] #34 phase\n";
    let e = child_edges(SLUG, &node(30, "", body, &[]));
    assert_eq!(
        pairs(&e),
        vec![
            (30, 31, EdgeSource::TaskList),
            (30, 33, EdgeSource::TaskList),
            (30, 34, EdgeSource::TaskList),
        ]
    );
}

#[test]
fn an_epic_phase_marker_links_to_its_epic() {
    let body = "Phase two.\n<!-- loom:epic:300:phase:2 -->\n";
    assert_eq!(
        pairs(&parent_edges(SLUG, &node(301, "", body, &[]))),
        vec![(300, 301, EdgeSource::EpicPhase)]
    );
}

#[test]
fn a_parent_marker_links_to_its_parent() {
    let body = "Child.\n<!-- loom:parent #40 -->\n";
    assert_eq!(
        pairs(&parent_edges(SLUG, &node(41, "", body, &[]))),
        vec![(40, 41, EdgeSource::ParentMarker)]
    );
}

#[test]
fn a_part_of_line_links_issues_but_not_prs() {
    let body = "Part of #9983\n\nMore text.\n**Part of:** #9984\n";
    let n = node(9987, "", body, &[]);
    assert_eq!(
        pairs(&parent_edges(SLUG, &n)),
        vec![
            (9983, 9987, EdgeSource::PartOf),
            (9984, 9987, EdgeSource::PartOf)
        ]
    );
    let pr = Node {
        is_pull_request: true,
        ..n
    };
    assert!(parent_edges(SLUG, &pr).is_empty(), "a PR's `Part of` is §5's link, not an edge");
}

#[test]
fn a_parent_title_prefix_links_to_its_parent() {
    let n = node(813, "[Parent #812] Part 1: Core functionality", "", &[]);
    assert_eq!(pairs(&parent_edges(SLUG, &n)), vec![(812, 813, EdgeSource::TitlePrefix)]);
}

#[test]
fn a_native_sub_issue_links_and_dedupes_by_precedence() {
    let park = "<!-- loom:park Blocked by: #2 -->";
    let child = "<!-- loom:parent #1 -->";
    let nodes = [node(1, "", park, &[]), node(2, "", child, &[])];
    let e = resolve(SLUG, &nodes, &[(1, 2), (1, 3)]);
    assert_eq!(
        pairs(&e),
        vec![(1, 2, EdgeSource::ParkRecord), (1, 3, EdgeSource::SubIssue)],
        "one edge per pair, the highest-precedence source kept"
    );
}

#[test]
fn a_prose_mention_never_links() {
    let body = "This is like #12, see #13 and o/r#14.\nPart of the fix for #15 is here.\n\
        Mentioned: Blocked by #16 (but not loom:blocked).\n";
    let n = node(10, "Follow-up to #17", body, &[]);
    assert!(child_edges(SLUG, &n).is_empty());
    assert!(parent_edges(SLUG, &n).is_empty());
}

#[test]
fn a_cross_repo_reference_never_links() {
    let body = "<!-- loom:park Blocked by: other/repo#11 -->\n- [ ] other/repo#12\n\
        - [ ] https://github.com/other/repo/issues/13\n- [ ] https://github.com/o/r/issues/14\n";
    let e = child_edges(SLUG, &node(10, "", body, &[]));
    assert_eq!(pairs(&e), vec![(10, 14, EdgeSource::TaskList)]);
    let up = node(
        20,
        "[Parent other/repo#1]",
        "Part of other/repo#2\n<!-- loom:parent other/repo#3 -->",
        &[],
    );
    assert!(parent_edges(SLUG, &up).is_empty());
    assert_eq!(same_repo_number("O/R#7", SLUG), Some(7), "the slug compares case-insensitively");
}

fn edge(parent: u32, child: u32) -> Edge {
    Edge {
        parent,
        child,
        source: EdgeSource::ParkRecord,
    }
}

fn root(number: u32, at: &str) -> Root {
    Root {
        number,
        starred_at: Some(at.to_string()),
    }
}

#[test]
fn every_child_inherits_not_only_the_first() {
    let d = descendants(&[root(1, "2026-10-01T00:00:00Z")], &[edge(1, 10), edge(1, 9)], 3);
    assert_eq!(d.keys().copied().collect::<Vec<_>>(), vec![9, 10]);
    assert!(d
        .values()
        .all(|i| i.root == 1 && i.via == 1 && i.depth == 1));
}

#[test]
fn a_four_deep_chain_stops_at_depth_three_and_a_cycle_terminates() {
    let chain = [edge(1, 2), edge(2, 3), edge(3, 4), edge(4, 5), edge(5, 2)];
    let d = descendants(&[root(1, "2026-10-01T00:00:00Z")], &chain, MAX_INHERIT_DEPTH);
    assert_eq!(d.keys().copied().collect::<Vec<_>>(), vec![2, 3, 4]);
    assert_eq!(d[&4].depth, 3);
    assert_eq!(d[&4].via, 3);
    // A pure cycle back to the root: the root keeps its own star.
    let d = descendants(&[root(1, "2026-10-01T00:00:00Z")], &[edge(1, 2), edge(2, 1)], 3);
    assert_eq!(d.keys().copied().collect::<Vec<_>>(), vec![2]);
}

#[test]
fn a_child_of_two_starred_ancestors_takes_the_earliest_star() {
    let roots = [
        root(1, "2026-10-02T00:00:00Z"),
        root(2, "2026-10-01T00:00:00Z"),
    ];
    let d = descendants(&roots, &[edge(1, 5), edge(2, 6), edge(6, 5)], 3);
    let five = &d[&5];
    assert_eq!(
        (five.root, five.via, five.depth),
        (2, 6, 2),
        "earlier star wins over shallower path"
    );
    assert_eq!(five.starred_at.as_deref(), Some("2026-10-01T00:00:00Z"));
    // With one ancestor unstarred (gone from the roots) it still inherits.
    let d = descendants(&roots[..1], &[edge(1, 5), edge(2, 6), edge(6, 5)], 3);
    assert_eq!(d[&5].root, 1);
    // A starred issue is never another's descendant.
    let d = descendants(&roots, &[edge(1, 2)], 3);
    assert!(d.is_empty());
}
