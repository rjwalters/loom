//! loom-ui star intents: validation, idempotent apply, unstar, the unmanaged
//! drop, and the starred-at seam.

use super::fake::{issue, repo_input, t, Host, World, STAR};
use crate::star_liveness::intents::{
    parse_ack_intents, recorded_starred_at, starred_at_from_timeline, IntentQueue, IntentStarredAt,
    StarIntent,
};
use crate::types::{AskKind, LandingStage};
use crate::work_finder::operator_priority::StarredAtSource;

fn intent(id: &str, repo: &str, number: u32, action: &str) -> StarIntent {
    StarIntent {
        id: id.into(),
        repo: repo.into(),
        number,
        action: action.into(),
        label: STAR.into(),
        requested_at: Some("2026-09-28T08:15:00+00:00".into()),
        requested_by: Some("joseph".into()),
    }
}

#[test]
fn a_star_is_applied_once_with_one_audit_comment_across_passes_and_hosts() {
    let world = World::default();
    let slug = "ui/apply";
    world.add(slug, issue(281, &["loom:triage"]));
    let repos = vec![repo_input(slug)];
    let mut a = Host::new("host-a");
    let r = a.pass(&world, &repos, vec![intent("i-1", "UI/Apply", 281, "star")], t(10, 0));

    let labels = world.repo(slug).items[&281].labels.clone();
    assert!(
        labels.iter().any(|l| l == STAR),
        "label applied (repo matched case-insensitively)"
    );
    let posted = world.posted(slug);
    assert_eq!(posted.len(), 1);
    assert!(posted[0].1.contains(
        "<!-- loom:operator-priority-intent=i-1 action=star requested_at=2026-09-28T08:15:00Z -->"
    ));
    assert!(posted[0].1.contains("by `joseph` via loom-ui"));
    // The same pass already sees the star it applied.
    assert_eq!(r.rows.iter().filter(|r| r.issue == 281).count(), 1);
    assert_eq!(
        recorded_starred_at(&repos[0].root, 281).as_deref(),
        Some("2026-09-28T08:15:00Z"),
        "requested_at is the starred-at"
    );

    // The backend resends it: same host, then another host. No second comment.
    a.pass(&world, &repos, vec![intent("i-1", "ui/apply", 281, "star")], t(10, 2));
    let mut b = Host::new("host-b");
    b.pass(&world, &repos, vec![intent("i-1", "ui/apply", 281, "star")], t(10, 3));
    let intent_comments = world
        .posted(slug)
        .iter()
        .filter(|(_, body)| body.contains("operator-priority-intent=i-1"))
        .count();
    assert_eq!(intent_comments, 1, "a duplicate intent posts no second comment");
}

#[test]
fn unstar_removes_the_label_and_the_recorded_starred_at() {
    let world = World::default();
    let slug = "ui/unstar";
    world.add(slug, issue(5, &["loom:issue"]));
    let repos = vec![repo_input(slug)];
    let mut a = Host::new("host-a");
    a.pass(&world, &repos, vec![intent("s-1", slug, 5, "star")], t(10, 0));
    assert!(recorded_starred_at(&repos[0].root, 5).is_some());
    let r = a.pass(&world, &repos, vec![intent("u-1", slug, 5, "unstar")], t(10, 5));
    assert!(!world.repo(slug).items[&5].labels.iter().any(|l| l == STAR));
    assert!(world
        .posted(slug)
        .iter()
        .any(|(_, b)| b.contains("intent=u-1 action=unstar")));
    assert!(recorded_starred_at(&repos[0].root, 5).is_none());
    assert!(r.rows.is_empty(), "no longer tracked");
}

#[test]
fn invalid_intents_are_dropped_with_a_reason() {
    let world = World::default();
    let slug = "ui/valid";
    world.add(slug, issue(5, &["loom:issue"]));
    let repos = vec![repo_input(slug)];
    let mut wrong_label = intent("a", slug, 5, "star");
    wrong_label.label = "loom:issue".into();
    let mut no_actor = intent("b", slug, 5, "star");
    no_actor.requested_by = Some("  ".into());
    let mut bad_id = intent("c--d", slug, 5, "star");
    bad_id.id = "x --> <script>".into();
    let batch = vec![
        wrong_label,
        no_actor,
        intent("e", slug, 5, "toggle"),
        bad_id,
        intent("f", "someone/else", 9, "star"),
    ];
    let mut a = Host::new("host-a");
    let r = a.pass(&world, &repos, batch, t(10, 0));
    let reasons: Vec<&str> = r
        .dropped_intents
        .iter()
        .map(|d| d.reason.as_str())
        .collect();
    assert_eq!(
        reasons,
        vec![
            "wrong-label",
            "missing-requested-by",
            "bad-action",
            "malformed",
            "unmanaged-repo"
        ]
    );
    assert!(world.posted(slug).is_empty(), "nothing applied");
    assert!(!world.repo(slug).items[&5].labels.iter().any(|l| l == STAR));
    assert!(world.repo("someone/else").items.is_empty(), "never touches an unmanaged repo");
}

#[test]
fn a_star_on_an_unmanaged_repo_is_an_operator_ask_on_this_host() {
    let world = World::default();
    let mut a = Host::new("host-a");
    let repos = vec![repo_input("ui/managed")];
    let r = a.pass(&world, &repos, vec![intent("x", "2AMLogic/elsewhere", 12, "star")], t(10, 0));
    let row = r
        .rows
        .iter()
        .find(|r| r.repo == "2AMLogic/elsewhere")
        .unwrap();
    assert_eq!(row.stage, LandingStage::NeedsOperator);
    assert_eq!(row.ask.as_ref().map(|a| a.kind), Some(AskKind::UnmanagedRepo));
    assert!(
        world.repo("2AMLogic/elsewhere").posted.is_empty(),
        "no write to a repo we do not manage"
    );
    // Deduped across ticks: still one row; an unstar clears it.
    let r = a.pass(&world, &repos, vec![intent("x", "2AMLogic/elsewhere", 12, "star")], t(10, 2));
    assert_eq!(r.rows.iter().filter(|r| r.issue == 12).count(), 1);
    let r = a.pass(&world, &repos, vec![intent("y", "2AMLogic/elsewhere", 12, "unstar")], t(10, 3));
    assert!(r.rows.iter().all(|r| r.issue != 12));
}

#[test]
fn the_queue_dedupes_by_id_and_parse_tolerates_old_backends() {
    assert!(parse_ack_intents(None).is_none());
    assert_eq!(parse_ack_intents(Some(&[])), Some(Vec::new()));
    let q = IntentQueue::default();
    q.push_all(vec![intent("a", "o/r", 1, "star"), intent("a", "o/r", 1, "star")]);
    q.push_all(vec![intent("a", "o/r", 1, "star"), intent("b", "o/r", 2, "star")]);
    assert_eq!(q.drain().len(), 2);
    assert!(q.is_empty());
}

#[test]
fn requested_at_outranks_the_labeled_event_unless_relabeled_later() {
    // Label applied by the intent (10:00:05), comment right after: requested_at wins.
    let out = "L 2026-09-28T10:00:05Z\nC 2026-09-28T10:00:07Z 2026-09-28T09:59:00Z\n";
    assert_eq!(starred_at_from_timeline(out).as_deref(), Some("2026-09-28T09:59:00Z"));
    // Unstarred and restarred directly on GitHub later: the later labeling wins.
    let out2 = format!("{out}L 2026-09-28T12:00:00Z\n");
    assert_eq!(starred_at_from_timeline(&out2).as_deref(), Some("2026-09-28T12:00:00Z"));
    // No intent: A's labeled-event behavior; nothing: None.
    assert_eq!(
        starred_at_from_timeline("L 2026-09-28T10:00:05Z\n").as_deref(),
        Some("2026-09-28T10:00:05Z")
    );
    assert_eq!(starred_at_from_timeline("C 2026-09-28T10:00:07Z -\n"), None);
    assert_eq!(
        starred_at_from_timeline("2026-09-01T00:00:00Z\n").as_deref(),
        Some("2026-09-01T00:00:00Z"),
        "a bare timestamp is a labeled event"
    );
}

struct Timeline(Option<String>);
impl StarredAtSource for Timeline {
    fn starred_at(&mut self, _issue: u32) -> anyhow::Result<Option<String>> {
        Ok(self.0.clone())
    }
}

#[test]
fn the_seam_answers_from_an_applied_intent_first() {
    let world = World::default();
    let slug = "ui/seam";
    world.add(slug, issue(3, &["loom:issue"]));
    let repos = vec![repo_input(slug)];
    Host::new("host-a").pass(&world, &repos, vec![intent("z", slug, 3, "star")], t(10, 0));
    let mut inner = Timeline(Some("2026-09-28T10:00:05Z".into()));
    let mut seam = IntentStarredAt {
        root: Some(&repos[0].root),
        inner: &mut inner,
    };
    assert_eq!(seam.starred_at(3).unwrap().as_deref(), Some("2026-09-28T08:15:00Z"));
    assert_eq!(seam.starred_at(4).unwrap().as_deref(), Some("2026-09-28T10:00:05Z"));
}
