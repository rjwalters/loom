//! A blocker named only in a `## Dependencies` checklist is a named blocker
//! (#10024).
//!
//! Replays 2AMLogic/loom-ui#1016 (2026-10-02/03): starred, `loom:blocked`, and
//! its body's only blocker an unchecked `- [ ] 2AMLogic/2am#1911 …` checklist
//! item. Star-liveness read only the `Blocked by …` phrase form, so it kept
//! posting the "name the blocker" ask until a Curator added a redundant phrase
//! line. It must land `BlockedCrossRepo` from the checklist alone.

use super::fake::{issue_with_body, repo_input, t, Host, World, STAR};
use crate::types::{AskKind, LandingStage};

const UI: &str = "2AMLogic/loom-ui";
const UPSTREAM: &str = "2AMLogic/2am";

const LOOM_UI_1016_BODY: &str = "## Summary\n\nRender the new panel.\n\n\
     ## Dependencies\n\n\
     - [ ] 2AMLogic/2am#1911 P1/P2 — upstream API the panel reads\n\n\
     ## Acceptance criteria\n\n- [ ] #1017 follow-up is not a blocker\n";

#[test]
fn a_checklist_only_cross_repo_blocker_lands_blocked_cross_repo() {
    let world = World::default();
    world.add(
        UI,
        issue_with_body(1016, &[STAR, "loom:curated", "loom:blocked"], LOOM_UI_1016_BODY),
    );
    let repos = vec![repo_input(UI), repo_input(UPSTREAM)];
    let r = Host::new("host-a").pass(&world, &repos, Vec::new(), t(10, 0));

    let row = r
        .rows
        .iter()
        .find(|row| row.repo == UI && row.issue == 1016)
        .expect("loom-ui#1016 has a landing row");
    let ask = row.ask.as_ref().expect("an operator ask");
    assert_eq!(ask.kind, AskKind::BlockedCrossRepo, "{}", ask.text);
    assert_ne!(ask.kind, AskKind::BlockedUnnamed);
    assert_eq!(row.blocked_by.as_deref(), Some("2AMLogic/2am#1911"));
    assert!(ask.text.contains("2AMLogic/2am#1911"), "{}", ask.text);
}

#[test]
fn a_checklist_only_same_repo_blocker_takes_the_inherit_path() {
    let world = World::default();
    let slug = "i/checklist";
    world.add(
        slug,
        issue_with_body(
            10,
            &[STAR, "loom:curated", "loom:blocked"],
            "## Dependencies\n\n- [ ] #11: the prerequisite\n- [x] #12: already done\n",
        ),
    );
    world.add(slug, issue_with_body(11, &["loom:issue"], ""));
    world.add(slug, issue_with_body(12, &["loom:issue"], ""));
    let repos = vec![repo_input(slug)];
    let r = Host::new("host-a").pass(&world, &repos, Vec::new(), t(10, 0));

    assert_eq!((r.rows[0].issue, r.rows[0].stage), (10, LandingStage::BlockedBy));
    assert_eq!(r.rows[0].blocked_by.as_deref(), Some("#11"));
    assert!(r.rows[0].ask.is_none());
    let inherited: Vec<u32> = r
        .rows
        .iter()
        .filter(|row| row.inherited_from == Some(10))
        .map(|row| row.issue)
        .collect();
    assert_eq!(inherited, vec![11], "only the unchecked item inherits the star");
}

#[test]
fn a_checked_only_checklist_still_asks_and_lists_the_accepted_forms() {
    let world = World::default();
    let slug = "i/checked";
    world.add(
        slug,
        issue_with_body(
            10,
            &[STAR, "loom:curated", "loom:blocked"],
            "## Dependencies\n\n- [x] #11: done\n",
        ),
    );
    world.add(slug, issue_with_body(11, &["loom:issue"], ""));
    // Curator was already handed it once (#10151), and it came back blocked.
    world.comment(slug, 10, crate::star_liveness::stale::HANDOFF_MARKER);
    let repos = vec![repo_input(slug)];
    let r = Host::new("host-a").pass(&world, &repos, Vec::new(), t(10, 0));

    let ask = r.rows[0].ask.as_ref().expect("an operator ask");
    assert_eq!(ask.kind, AskKind::BlockedUnnamed);
    for form in [
        "Blocked by #N",
        "Depends on owner/repo#N",
        "- [ ] #N",
        "## Dependencies",
    ] {
        assert!(ask.text.contains(form), "ask should list `{form}`: {}", ask.text);
    }
}
