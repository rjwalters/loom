//! Standing down for repos the fleet captain reports star-free (W12 part 2):
//! the skipped pass is the empty local pass, local evidence beats the
//! captain's report, and the escalation marker dedupe holds across a fleet
//! where only some hosts stand down.

use std::collections::HashMap;

use chrono::{DateTime, Duration, Utc};

use super::fake::{issue, issue_with_body, pr, repo_input, t, tick_row, Host, World, STAR};
use crate::star_liveness::captain::{StandDown, EVIDENCE_SLACK};
use crate::star_liveness::escalate::MARKER_PREFIX;
use crate::star_liveness::inherit::{self, Inherited};
use crate::star_liveness::intents::StarIntent;
use crate::types::{LandingStage, QueueDisposition};
use crate::work_finder::WorkItem;

/// The captain reports `slugs` star-free as of `as_of`.
fn free(slugs: &[&str], as_of: DateTime<Utc>) -> HashMap<String, DateTime<Utc>> {
    slugs
        .iter()
        .map(|s| (s.to_ascii_lowercase(), as_of))
        .collect()
}

/// A starred issue parked on an operator decision: escalates on first sight.
fn star_needing_the_operator(world: &World, slug: &str) {
    world.add(slug, issue(1, &[STAR, "loom:building"]));
    world.add(slug, pr(2, 1, &["loom:pr", "loom:operator-decision"]));
}

fn escalations(world: &World, slug: &str) -> usize {
    world
        .posted(slug)
        .iter()
        .filter(|(_, body)| body.contains(MARKER_PREFIX))
        .count()
}

#[test]
fn an_unconfigured_host_evaluates_every_repo_as_before() {
    let world = World::default();
    let slug = "c/plain";
    world.add(slug, issue(3, &["loom:issue"]));
    let mut host = Host::new("host-a");
    host.pass(&world, &[repo_input(slug)], Vec::new(), t(10, 0));
    assert_eq!(world.repo(slug).listings, 2, "one listing per operator label, as today");
}

#[test]
fn standing_down_is_the_empty_local_pass_without_the_listing() {
    let world = World::default();
    let slug = "C/Idle";
    world.add(slug, issue(3, &["loom:issue"]));
    let repos = vec![repo_input(slug)];
    // An inheritance left over from an earlier local pass must not survive.
    inherit::publish(
        &repos[0].root,
        vec![Inherited {
            number: 3,
            from: 9,
            starred_at: None,
            item: WorkItem::with_created_at(3, Vec::new(), None),
        }],
    );
    let mut host = Host::new("host-a");
    host.state.set_captain_star_free(free(&[slug], t(9, 58)));
    let report = host.pass(&world, &repos, Vec::new(), t(10, 0));
    assert_eq!(world.repo(slug).listings, 0, "no forge read at all for the repo");
    assert!(report.rows.is_empty());
    assert!(report.failed_repos.is_empty(), "a stood-down repo is a successful pass");
    assert!(inherit::current(&repos[0].root).is_empty(), "as an empty local pass publishes");

    // The captain's report is gone (stale, withdrawn, switched off): local.
    host.state.set_captain_star_free(HashMap::new());
    host.pass(&world, &repos, Vec::new(), t(10, 2));
    assert_eq!(world.repo(slug).listings, 2);
}

#[test]
fn only_the_reported_repos_are_skipped() {
    let world = World::default();
    star_needing_the_operator(&world, "c/starred");
    world.add("c/quiet", issue(3, &["loom:issue"]));
    let repos = vec![repo_input("c/starred"), repo_input("c/quiet")];
    let mut host = Host::new("host-a");
    // The captain covers both, and reports only the quiet one star-free.
    host.state
        .set_captain_star_free(free(&["c/quiet"], t(9, 58)));
    let report = host.pass(&world, &repos, Vec::new(), t(10, 0));
    assert_eq!(world.repo("c/quiet").listings, 0);
    assert!(world.repo("c/starred").listings > 0, "a repo with a star is evaluated here");
    assert_eq!(report.rows.len(), 1);
    assert_eq!(report.rows[0].stage, LandingStage::NeedsOperator);
    assert_eq!(escalations(&world, "c/starred"), 1, "and escalated here, as before");
}

#[test]
fn a_starred_tick_row_overrides_the_captains_report() {
    // The work finder's own starred listing already shows a star the
    // captain's last listing predates: evaluate, do not wait for the captain.
    let world = World::default();
    let slug = "c/tick";
    world.add(slug, issue(7, &[STAR, "loom:issue"]));
    let mut input = repo_input(slug);
    input.tick_rows = vec![tick_row(&input.root, 7, QueueDisposition::DeferredCapacity)];
    let mut host = Host::new("host-a");
    host.state.set_captain_star_free(free(&[slug], t(9, 58)));
    let report = host.pass(&world, &[input], Vec::new(), t(10, 0));
    assert!(world.repo(slug).listings > 0);
    assert_eq!(report.rows.len(), 1);

    // The tick row is gone but the star is not, and the captain's report is
    // still the old one: this host's own evidence is newer, so it keeps
    // evaluating.
    let report = host.pass(&world, &[repo_input(slug)], Vec::new(), t(10, 2));
    assert_eq!(report.rows.len(), 1, "local evidence outlives the tick row");
}

#[test]
fn a_star_this_host_applied_keeps_the_repo_local_until_the_captain_catches_up() {
    let world = World::default();
    let slug = "c/intent";
    world.add(slug, issue_with_body(4, &["loom:triage"], "needs a look"));
    let repos = vec![repo_input(slug)];
    let star = StarIntent {
        id: "w12-1".into(),
        repo: slug.into(),
        number: 4,
        action: "star".into(),
        label: STAR.into(),
        requested_at: Some("2026-09-28T09:59:00+00:00".into()),
        requested_by: Some("operator".into()),
    };
    let mut host = Host::new("host-a");
    host.state.set_captain_star_free(free(&[slug], t(9, 58)));
    let report = host.pass(&world, &repos, vec![star], t(10, 0));
    assert_eq!(report.rows.len(), 1, "the pass that applies the star already sees it");
}

#[test]
fn the_captains_report_must_be_newer_than_local_evidence_by_the_slack() {
    let input = repo_input("c/evidence");
    let seen = t(10, 0);
    let mut stand = StandDown::default();
    stand.note_star("C/Evidence", seen);
    // Older than, equal to, or inside the slack after the evidence: local.
    for as_of in [
        seen - Duration::minutes(1),
        seen,
        seen + EVIDENCE_SLACK - Duration::seconds(1),
    ] {
        stand.set_free(free(&["c/evidence"], as_of));
        assert!(!stand.stands_down(&input, t(10, 30)), "{as_of}");
    }
    // A listing made after the evidence, beyond the slack: believed.
    stand.set_free(free(&["c/evidence"], seen + EVIDENCE_SLACK));
    assert!(stand.stands_down(&input, t(10, 30)));
    // Evidence only moves forward.
    stand.note_star("c/evidence", seen - Duration::hours(1));
    assert!(stand.stands_down(&input, t(10, 30)));
    // No report for the repo: local.
    stand.set_free(free(&["c/other"], t(10, 29)));
    assert!(!stand.stands_down(&input, t(10, 30)));
}

#[test]
fn a_star_seen_locally_is_not_dropped_for_an_older_report() {
    // The switch is turned on (or the captain's heartbeat is read again)
    // while this host is already tracking a star the captain's last listing
    // predates. The old "star-free" must not withdraw the landing row.
    let world = World::default();
    let slug = "c/older";
    star_needing_the_operator(&world, slug);
    let repos = vec![repo_input(slug)];
    let mut host = Host::new("host-a");
    let first = host.pass(&world, &repos, Vec::new(), t(10, 0));
    assert_eq!(first.rows.len(), 1);
    host.state.set_captain_star_free(free(&[slug], t(9, 59)));
    let second = host.pass(&world, &repos, Vec::new(), t(10, 2));
    assert_eq!(second.rows.len(), 1, "the row survives the stale report");
    assert_eq!(escalations(&world, slug), 1);
}

/// Mixed-version rollout: `old` predates W12 part 2 and always evaluates;
/// `new` stands down while the captain reports the repo star-free. The
/// escalation comment is deduplicated by the marker every evaluating host
/// reads before posting, and a host that stands down posts nothing, so each
/// key is posted exactly once whichever host gets there first.
#[test]
fn the_escalation_marker_dedupe_holds_across_a_mixed_version_rollout() {
    let world = World::default();
    let slug = "m/mixed";
    world.add(slug, issue(9, &["loom:issue"]));
    let repos = vec![repo_input(slug)];
    let mut old = Host::new("host-old");
    let mut new = Host::new("host-new");

    // No star anywhere: the old host lists, the new one does not.
    new.state.set_captain_star_free(free(&[slug], t(9, 58)));
    old.pass(&world, &repos, Vec::new(), t(10, 0));
    let listed_by_old = world.repo(slug).listings;
    new.pass(&world, &repos, Vec::new(), t(10, 0));
    assert_eq!(world.repo(slug).listings, listed_by_old, "the new host made no read");
    assert_eq!(escalations(&world, slug), 0);

    // A star that needs the operator appears. The captain has not listed
    // since, so its report still says star-free.
    star_needing_the_operator(&world, slug);
    let seen_by_old = old.pass(&world, &repos, Vec::new(), t(10, 2));
    assert_eq!(seen_by_old.escalations_posted, 1, "the old host escalates as it always did");
    let stood_down = new.pass(&world, &repos, Vec::new(), t(10, 2));
    assert!(stood_down.rows.is_empty());
    assert_eq!(stood_down.escalations_posted, 0, "a host standing down posts nothing");
    assert_eq!(escalations(&world, slug), 1);

    // The captain's next listing counts the star: no star-free report, so
    // the new host evaluates, reads the marker and stays quiet.
    new.state.set_captain_star_free(HashMap::new());
    let local = new.pass(&world, &repos, Vec::new(), t(10, 10));
    assert_eq!(local.rows.len(), 1);
    assert_eq!(local.rows[0].stage, LandingStage::NeedsOperator);
    assert_eq!(local.escalations_posted, 0, "found the old host's marker on the forge");
    assert_eq!(escalations(&world, slug), 1);

    // Both restart (a rolling deploy): the forge marker still answers.
    old.restart();
    new.restart();
    assert_eq!(
        new.pass(&world, &repos, Vec::new(), t(10, 20))
            .escalations_posted,
        0
    );
    assert_eq!(
        old.pass(&world, &repos, Vec::new(), t(10, 21))
            .escalations_posted,
        0
    );
    assert_eq!(escalations(&world, slug), 1, "one comment for the cause, fleet-wide");
}

#[test]
fn the_new_host_posts_first_when_the_captain_goes_stale_and_the_old_host_defers() {
    let world = World::default();
    let slug = "m/stale";
    star_needing_the_operator(&world, slug);
    let repos = vec![repo_input(slug)];
    let mut old = Host::new("host-old");
    let mut new = Host::new("host-new");

    // The captain died before the star appeared: its last report has aged
    // out, so the new host is handed nothing and evaluates like the old one.
    new.state.set_captain_star_free(HashMap::new());
    assert_eq!(
        new.pass(&world, &repos, Vec::new(), t(10, 0))
            .escalations_posted,
        1
    );
    assert_eq!(
        old.pass(&world, &repos, Vec::new(), t(10, 1))
            .escalations_posted,
        0
    );
    assert_eq!(escalations(&world, slug), 1);

    // The captain comes back with a report that predates this host's own
    // sighting of the star: not believed, and nothing is posted twice.
    new.state.set_captain_star_free(free(&[slug], t(9, 0)));
    let again = new.pass(&world, &repos, Vec::new(), t(10, 2));
    assert_eq!(again.rows.len(), 1);
    assert_eq!(again.escalations_posted, 0);
    assert_eq!(escalations(&world, slug), 1);
}
