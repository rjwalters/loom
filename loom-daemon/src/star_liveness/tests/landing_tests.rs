//! The classifier alone: every stage, and every non-agent state's ask.

use crate::star_liveness::landing::{
    classify, BlockerRef, Capacity, ItemFacts, MergeRefusal, PrFacts, StarFacts,
};
use crate::types::{AskKind, LandingStage};

fn item(n: u32, labels: &[&str]) -> ItemFacts {
    ItemFacts {
        number: n,
        labels: labels.iter().map(|l| (*l).to_string()).collect(),
        open: true,
        ..ItemFacts::default()
    }
}

fn facts(labels: &[&str]) -> StarFacts {
    StarFacts {
        repo: "o/r".into(),
        managed: true,
        issue: item(10, labels),
        host: "host-a".into(),
        ..StarFacts::default()
    }
}

fn with_pr(mut f: StarFacts, labels: &[&str]) -> StarFacts {
    f.pr = Some(PrFacts {
        item: item(20, labels),
        refusal: None,
    });
    f
}

fn ask_kind(f: &StarFacts) -> Option<AskKind> {
    let l = classify(f);
    assert_eq!(l.ask.is_some(), l.stage == LandingStage::NeedsOperator, "{l:?}");
    l.ask.map(|a| a.kind)
}

#[test]
fn agent_stages_have_owners_and_no_ask() {
    let cases: Vec<(StarFacts, LandingStage, &str)> = vec![
        (facts(&["loom:operator-priority"]), LandingStage::Curating, "curator"),
        (facts(&["loom:triage"]), LandingStage::Curating, "curator"),
        (facts(&["loom:issue"]), LandingStage::Ready, "work-finder"),
        (facts(&["loom:building"]), LandingStage::Building, "builder"),
        (
            with_pr(facts(&["loom:building"]), &["loom:review-requested"]),
            LandingStage::InReview,
            "judge",
        ),
        (
            with_pr(facts(&["loom:building"]), &["loom:changes-requested"]),
            LandingStage::ChangesRequested,
            "doctor",
        ),
        (
            with_pr(facts(&["loom:building"]), &["loom:pr"]),
            LandingStage::Mergeable,
            "champion",
        ),
    ];
    for (f, stage, actor) in cases {
        let l = classify(&f);
        assert_eq!((l.stage, l.next_actor.as_str()), (stage, actor), "{f:?}");
        assert!(l.ask.is_none());
    }
    let mut merging = with_pr(facts(&["loom:building"]), &["loom:pr"]);
    merging.live_sweep = true;
    assert_eq!(classify(&merging).stage, LandingStage::Merging);
    let mut deferred = facts(&["loom:issue"]);
    deferred.capacity = Capacity::Deferred(crate::types::CapacityWait {
        gate: "capacity".into(),
        limiter: None,
        position: None,
        total: None,
        cap: None,
        configured_cap: None,
    });
    let l = classify(&deferred);
    assert_eq!(l.stage, LandingStage::NoCapacity);
    assert_eq!(l.no_capacity.as_deref(), Some("waiting (concurrency cap full)"));
    assert!(l.capacity_wait.is_some());
}

#[test]
fn every_non_agent_state_is_needs_operator_with_one_concrete_ask() {
    let decision = with_pr(facts(&["loom:building"]), &["loom:pr", "loom:operator-decision"]);
    assert_eq!(ask_kind(&decision), Some(AskKind::OperatorDecision));
    let l = classify(&decision);
    assert_eq!(l.ask.as_ref().unwrap().key, "operator-decision:pr-20");
    assert!(l.ask.unwrap().text.contains("PR #20"));

    assert_eq!(ask_kind(&facts(&["loom:operator-only"])), Some(AskKind::OperatorOnly));

    let mut refused = with_pr(facts(&["loom:building"]), &["loom:pr"]);
    let raw = r#"Error: Failed to merge PR #20: {"message":"Merge commits are not allowed on this repository.","status":"405"}"#;
    refused.pr.as_mut().unwrap().refusal = Some(MergeRefusal {
        reason: "merge commits are not allowed on this repository (HTTP 405)",
        raw: raw.into(),
        incident: Some(30),
    });
    let l = classify(&refused);
    assert_eq!(l.ask.as_ref().map(|a| a.kind), Some(AskKind::MergeRefused));
    assert!(l.ask.as_ref().unwrap().text.contains("Tracked in #30"));
    assert_eq!(l.inherits, vec![30], "the incident inherits the star");
    // No open incident: still an ask, now carrying the forge's own words.
    refused
        .pr
        .as_mut()
        .unwrap()
        .refusal
        .as_mut()
        .unwrap()
        .incident = None;
    let l = classify(&refused);
    let text = &l.ask.as_ref().unwrap().text;
    assert!(text.contains("No open incident issue tracks it"), "{text}");
    assert!(text.contains(&format!("`{raw}`")), "{text}");
    assert!(l.inherits.is_empty());

    let mut pool = facts(&["loom:issue"]);
    pool.capacity = Capacity::PoolExhausted {
        detail: "all 3 token(s) exhausted".into(),
    };
    let l = classify(&pool);
    assert_eq!(l.ask.as_ref().map(|a| a.kind), Some(AskKind::PoolsExhausted));
    assert!(l.ask.unwrap().text.contains("host `host-a`"));

    let mut unmanaged = facts(&[]);
    unmanaged.managed = false;
    assert_eq!(ask_kind(&unmanaged), Some(AskKind::UnmanagedRepo));

    let hold = with_pr(facts(&["loom:building"]), &["loom:pr", "loom:operator"]);
    assert_eq!(ask_kind(&hold), Some(AskKind::MergeRiskHold));

    assert_eq!(ask_kind(&facts(&["loom:blocked"])), Some(AskKind::BlockedUnnamed));
}

#[test]
fn a_named_open_blocker_is_blocked_by_and_inherits() {
    let mut f = facts(&["loom:curated", "loom:blocked"]);
    f.blockers = vec![
        BlockerRef {
            display: "#5".into(),
            number: Some(5),
            open: Some(false),
            cross_repo_managed: None,
        },
        BlockerRef {
            display: "#6".into(),
            number: Some(6),
            open: Some(true),
            cross_repo_managed: None,
        },
    ];
    let l = classify(&f);
    assert_eq!(l.stage, LandingStage::BlockedBy);
    assert_eq!(l.blocked_by.as_deref(), Some("#6"));
    assert_eq!(l.inherits, vec![6]);
    assert_eq!(l.next_actor, "blocker #6");
    // All named blockers closed: nobody can unblock it but a human.
    f.blockers.truncate(1);
    assert_eq!(classify(&f).ask.map(|a| a.kind), Some(AskKind::BlockedUnnamed));
}

#[test]
fn red_main_blocks_only_undispatched_work_and_the_fix_inherits() {
    let mut f = facts(&["loom:issue"]);
    f.red_main_fix = Some(99);
    let l = classify(&f);
    assert_eq!((l.stage, l.inherits), (LandingStage::BlockedBy, vec![99]));
    let mut building = facts(&["loom:building"]);
    building.red_main_fix = Some(99);
    assert_eq!(classify(&building).stage, LandingStage::Building);
}

#[test]
fn operator_states_outrank_agent_ones() {
    // Parked on a decision AND in review: the decision is what holds it.
    let f =
        with_pr(facts(&["loom:building", "loom:operator-decision"]), &["loom:review-requested"]);
    assert_eq!(classify(&f).ask.unwrap().key, "operator-decision:issue");
    // A peer claim is building, never pool-exhausted.
    let mut peer = facts(&["loom:issue"]);
    peer.peer_claimed = true;
    peer.capacity = Capacity::PoolExhausted { detail: "x".into() };
    assert_eq!(classify(&peer).stage, LandingStage::Building);
}

#[test]
fn a_cross_repo_blocker_is_an_operator_ask_never_a_silent_state() {
    let cross = |managed: bool| BlockerRef {
        display: "other/repo#7".into(),
        number: None,
        open: None,
        cross_repo_managed: Some(managed),
    };
    let mut f = facts(&["loom:curated", "loom:blocked"]);
    f.blockers = vec![cross(false)];
    let l = classify(&f);
    assert_eq!(l.stage, LandingStage::NeedsOperator);
    let ask = l.ask.unwrap();
    assert_eq!(ask.kind, AskKind::BlockedCrossRepo);
    assert_eq!(ask.key, "blocked-cross-repo:other/repo#7");
    assert!(ask.text.contains("in a repo this host can't act on"), "{}", ask.text);
    assert_eq!(l.blocked_by.as_deref(), Some("other/repo#7"));
    assert!(l.inherits.is_empty());

    f.blockers = vec![cross(true)];
    let ask = classify(&f).ask.unwrap();
    assert!(ask.text.contains("star other/repo#7"), "{}", ask.text);

    // A same-repo open blocker still wins: it inherits and agents move it.
    f.blockers.push(BlockerRef {
        display: "#8".into(),
        number: Some(8),
        open: Some(true),
        cross_repo_managed: None,
    });
    let l = classify(&f);
    assert_eq!((l.stage, l.inherits), (LandingStage::BlockedBy, vec![8]));
}

/// #10012 AC 3: every open same-repo blocker inherits, not only the first
/// one named. `#20` sorts before `#9` as text; neither may be dropped.
#[test]
fn every_open_same_repo_blocker_inherits_not_only_the_first() {
    let blocker = |n: u32, open: bool| BlockerRef {
        display: format!("#{n}"),
        number: Some(n),
        open: Some(open),
        cross_repo_managed: None,
    };
    let mut f = facts(&["loom:curated", "loom:blocked"]);
    f.blockers = vec![blocker(20, true), blocker(9, true), blocker(21, false)];
    let l = classify(&f);
    assert_eq!(l.stage, LandingStage::BlockedBy);
    assert_eq!(l.inherits, vec![9, 20], "both open blockers inherit; the closed one does not");
}
