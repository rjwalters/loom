//! Unit tests for the #10827 promotion author gate.

use super::*;
use crate::forge_identity::{FleetLogins, Roster};
use serde_json::json;

fn policy() -> TrustPolicy {
    TrustPolicy::new(FleetLogins::of(&Roster::default()), None, vec!["partner".to_string()])
}

fn issue(login: &str, kind: &str, assoc: &str) -> Value {
    json!({"user": {"login": login, "type": kind}, "author_association": assoc, "number": 7})
}

#[test]
fn trusted_authors_pass_by_association_and_allowlist() {
    let p = policy();
    assert_eq!(classify(&p, Some(&issue("rjwalters", "User", "OWNER"))), Gate::Trusted);
    assert_eq!(classify(&p, Some(&issue("dev", "User", "COLLABORATOR"))), Gate::Trusted);
    assert_eq!(classify(&p, Some(&issue("partner", "User", "NONE"))), Gate::Trusted);
}

#[test]
fn untrusted_authors_are_held_with_their_login() {
    let p = policy();
    let gate = classify(&p, Some(&issue("drive-by", "User", "NONE")));
    assert_eq!(gate, Gate::Untrusted(Some("drive-by".to_string())));
    assert_eq!(gate.token(), "UNTRUSTED drive-by");
    assert!(matches!(
        classify(&p, Some(&issue("outside[bot]", "Bot", "NONE"))),
        Gate::Untrusted(_)
    ));
    assert!(matches!(
        classify(&p, Some(&issue("c", "User", "CONTRIBUTOR"))),
        Gate::Untrusted(_)
    ));
}

#[test]
fn a_deleted_author_is_untrusted_not_trusted() {
    let p = policy();
    assert_eq!(classify(&p, Some(&json!({"number": 7}))), Gate::Untrusted(None));
}

#[test]
fn unreadable_evidence_is_unknown_never_trusted() {
    let p = policy();
    let gate = classify(&p, None);
    assert!(matches!(gate, Gate::Unknown(_)));
    assert!(gate.token().starts_with("UNKNOWN "));
    let mut pr = issue("rjwalters", "User", "OWNER");
    pr["pull_request"] = json!({});
    assert!(matches!(classify(&p, Some(&pr)), Gate::Unknown(_)));
}

#[test]
fn only_the_trusted_gate_has_no_notice_and_retries_render_identically() {
    let held = Gate::Untrusted(Some("drive-by".to_string()));
    assert!(notice_body(&Gate::Trusted, None).is_empty());
    let body = notice_body(&held, None);
    assert!(body.starts_with(NOTICE_MARKER));
    assert!(body.contains("drive-by"));
    assert_eq!(body, notice_body(&held, None), "a retry posts the same, marker-anchored text");
    assert!(notice_body(&Gate::Unknown("x".into()), None).contains("could not be verified"));
}

#[test]
fn a_stamped_notice_round_trips_and_a_legacy_one_has_no_stamp() {
    let held = Gate::Untrusted(Some("drive-by".to_string()));
    let stamp = Stamp {
        rev: revision("t", "b"),
        trust: "0123456789abcdef".to_string(),
    };
    let body = notice_body(&held, Some(&stamp));
    assert!(body.starts_with("<!-- loom:promotion-author-gate rev="));
    assert_eq!(Stamp::parse(&body), Some(stamp));
    assert_eq!(Stamp::parse(&notice_body(&held, None)), None);
    assert_eq!(Stamp::parse("no marker here"), None);
}

#[test]
fn the_revision_moves_with_an_edit_and_ignores_trailing_newlines() {
    assert_eq!(revision("t", "b"), revision("t", "b\n"));
    assert_ne!(revision("t", "b"), revision("t", "b2"));
    assert_ne!(revision("t", "b"), revision("t2", "b"));
}

const TRACKER_BODY: &str = "x\n\nThis issue is opened, updated and closed by \
                            `.github/workflows/ci-daily.yml`. It closes itself.";

fn tracker(login: &str, title: &str, body: &str) -> Value {
    let mut v = issue(login, "Bot", "NONE");
    v["title"] = json!(title);
    v["body"] = json!(body);
    v
}

#[test]
fn a_ci_daily_tracker_from_the_actions_bot_passes() {
    let v = tracker("github-actions[bot]", "[ci-daily] build is red", TRACKER_BODY);
    assert_eq!(classify(&policy(), Some(&v)), Gate::Trusted);
}

#[test]
fn the_tracker_exception_needs_login_prefix_and_body_sentence() {
    let p = policy();
    let t = "[ci-daily] build is red";
    for v in [
        tracker("github-actions[bot]", "[ci-daily]", TRACKER_BODY),
        tracker("github-actions[bot]", "ci-daily build is red", TRACKER_BODY),
        tracker("github-actions[bot]", t, "no sentence"),
        tracker("github-actions[bot]", t, ""),
        tracker("other[bot]", t, TRACKER_BODY),
        tracker("github-actions", t, TRACKER_BODY),
        tracker("drive-by", t, TRACKER_BODY),
    ] {
        assert!(matches!(classify(&p, Some(&v)), Gate::Untrusted(_)), "{v}");
    }
    let mut no_fields = issue("github-actions[bot]", "Bot", "NONE");
    assert!(matches!(classify(&p, Some(&no_fields)), Gate::Untrusted(_)));
    no_fields["pull_request"] = json!({});
    no_fields["title"] = json!(t);
    no_fields["body"] = json!(TRACKER_BODY);
    assert!(matches!(classify(&p, Some(&no_fields)), Gate::Unknown(_)), "a PR stays unknown");
}

#[test]
fn a_fleet_listed_app_author_passes_but_its_bare_user_does_not() {
    let roster = crate::fleet_store::admins::Admins {
        apps: vec!["gateway[bot]".to_string()],
        state: "loaded 0".to_string(),
        ..Default::default()
    };
    let p = policy().with_admins(roster);
    assert_eq!(classify(&p, Some(&issue("gateway[bot]", "Bot", "NONE"))), Gate::Trusted);
    assert_ne!(classify(&p, Some(&issue("gateway", "User", "NONE"))), Gate::Trusted);
}

fn labeled(actor: &str, kind: &str, label: &str, at: &str) -> Value {
    json!({"event": "labeled", "created_at": at, "label": {"name": label},
           "actor": {"login": actor, "type": kind}})
}

fn edit(editor: &str, kind: &str, at: &str) -> Value {
    json!({"createdAt": at, "editedAt": at, "editor": {"login": editor, "__typename": kind}})
}

/// The collaborator-permission lookup: only `rjwalters` has write access.
fn access(login: &str) -> Option<String> {
    (login == "rjwalters").then(|| "COLLABORATOR".to_string())
}

const T1: &str = "2026-11-01T10:00:00Z";
const T2: &str = "2026-11-01T11:00:00Z";
const T3: &str = "2026-11-01T12:00:00Z";
const T4: &str = "2026-11-01T13:00:00Z";

fn stands(events: &[Value], edits: &[Value]) -> Option<String> {
    standing_adoption(&policy(), events, edits, access)
}

/// #13578: adopt -> partial merge -> re-curate -> promote. The adoption event
/// stays on the timeline through the later triage/curated label churn.
#[test]
fn a_trusted_adoption_survives_recuration() {
    let events = vec![
        labeled("rjwalters", "User", "loom:issue", T1),
        labeled("loom-bot[bot]", "Bot", "loom:triage", T2),
        labeled("loom-bot[bot]", "Bot", "loom:curated", T3),
    ];
    assert_eq!(stands(&events, &[]), Some("rjwalters".to_string()));
}

#[test]
fn never_adopted_or_adopted_by_an_untrusted_actor_is_not_adoption() {
    let curated = vec![labeled("loom-bot[bot]", "Bot", "loom:curated", T1)];
    assert_eq!(stands(&curated, &[]), None);
    let outsider = vec![labeled("drive-by", "User", "loom:issue", T1)];
    assert_eq!(stands(&outsider, &[]), None);
    assert_eq!(stands(&[], &[]), None);
}

#[test]
fn an_allowlisted_actor_needs_no_association_lookup() {
    let events = vec![labeled("partner", "User", "loom:issue", T1)];
    assert_eq!(
        standing_adoption(&policy(), &events, &[], |_| None),
        Some("partner".to_string())
    );
}

#[test]
fn adopted_then_edited_by_the_untrusted_author_is_untrusted() {
    let events = vec![labeled("rjwalters", "User", "loom:issue", T1)];
    assert_eq!(stands(&events, &[edit("drive-by", "User", T2)]), None);
}

#[test]
fn an_edit_before_the_adoption_does_not_end_it() {
    let events = vec![labeled("rjwalters", "User", "loom:issue", T2)];
    assert_eq!(stands(&events, &[edit("drive-by", "User", T1)]), Some("rjwalters".to_string()));
}

#[test]
fn adopted_edited_and_readopted_is_trusted() {
    let events = vec![
        labeled("rjwalters", "User", "loom:issue", T1),
        labeled("rjwalters", "User", "loom:issue", T3),
    ];
    assert_eq!(stands(&events, &[edit("drive-by", "User", T2)]), Some("rjwalters".to_string()));
    // ... and an edit after the re-adoption ends it again.
    assert_eq!(
        stands(&events, &[edit("drive-by", "User", T2), edit("drive-by", "User", T4)]),
        None
    );
}

#[test]
fn an_edit_by_a_trusted_actor_after_adoption_stays_trusted() {
    let events = vec![labeled("rjwalters", "User", "loom:issue", T1)];
    let edits = [edit("rjwalters", "User", T2), edit("partner", "User", T3)];
    assert_eq!(stands(&events, &edits), Some("rjwalters".to_string()));
}

#[test]
fn an_unreadable_edit_fails_closed() {
    let events = vec![labeled("rjwalters", "User", "loom:issue", T1)];
    // No editor (a deleted account), and no readable time.
    assert_eq!(
        stands(&events, &[json!({"createdAt": T2, "editedAt": null, "editor": null})]),
        None
    );
    assert_eq!(stands(&events, &[json!({"editor": {"login": "rjwalters"}})]), None);
}

// ---- Operator ruling 20261116-r15 q48: who adopts, and what ends it. ----

const FLEET_BOT: &str = "loom-fleet-dispatch[bot]";

/// A policy that trusts the fleet's own App, as the live one does.
fn fleet_policy() -> TrustPolicy {
    policy().with_admins(crate::fleet_store::admins::Admins {
        apps: vec![FLEET_BOT.to_string()],
        state: "loaded 0".to_string(),
        ..Default::default()
    })
}

fn ev(event: &str, actor: &str, kind: &str, label: &str, at: &str) -> Value {
    json!({"event": event, "created_at": at, "label": {"name": label},
           "actor": {"login": actor, "type": kind}})
}

fn bot(event: &str, label: &str, at: &str) -> Value {
    ev(event, FLEET_BOT, "Bot", label, at)
}

fn human(event: &str, who: &str, label: &str, at: &str) -> Value {
    ev(event, who, "User", label, at)
}

fn fleet_stands(events: &[Value]) -> Option<String> {
    standing_adoption(&fleet_policy(), events, &[], access)
}

#[test]
fn a_bot_applied_loom_issue_alone_is_not_an_adoption() {
    let p = fleet_policy();
    assert!(
        p.trusts(&Author::new(Some(FLEET_BOT), None)),
        "the policy trusts the bot's comments"
    );
    assert_eq!(fleet_stands(&[bot("labeled", "loom:issue", T1)]), None);
    // Nor an App whose actor object says `User`, nor an actor with no type.
    assert_eq!(fleet_stands(&[human("labeled", FLEET_BOT, "loom:issue", T1)]), None);
    let untyped = json!({"event": "labeled", "created_at": T1, "label": {"name": "loom:issue"},
                         "actor": {"login": "rjwalters"}});
    assert_eq!(fleet_stands(&[untyped]), None);
    // An allow-listed App is still an App.
    let listed = policy().with_admins(crate::fleet_store::admins::Admins {
        logins: vec!["gateway".to_string()],
        apps: vec!["gateway[bot]".to_string()],
        state: "loaded 1".to_string(),
    });
    let by_app = [ev("labeled", "gateway[bot]", "Bot", "loom:issue", T1)];
    assert_eq!(standing_adoption(&listed, &by_app, &[], |_| None), None);
    // ... while the allow-listed human of the same name adopts.
    let by_human = [human("labeled", "gateway", "loom:issue", T1)];
    assert_eq!(
        standing_adoption(&listed, &by_human, &[], |_| None),
        Some("gateway".to_string())
    );
}

#[test]
fn a_human_without_write_access_does_not_adopt() {
    assert_eq!(fleet_stands(&[human("labeled", "turian", "loom:issue", T1)]), None);
}

#[test]
fn a_maintainer_label_removed_outside_the_state_machine_is_not_an_adoption() {
    let adopted = human("labeled", "rjwalters", "loom:issue", T1);
    assert_eq!(fleet_stands(std::slice::from_ref(&adopted)), Some("rjwalters".to_string()));
    // A bare removal, whoever makes it.
    for who in [
        ("rjwalters", "User"),
        (FLEET_BOT, "Bot"),
        ("turian", "User"),
    ] {
        let events = [
            adopted.clone(),
            ev("unlabeled", who.0, who.1, "loom:issue", T2),
        ];
        assert_eq!(fleet_stands(&events), None, "{who:?}");
    }
    // A successor label from another actor does not make it a transition.
    let other_actor = [
        adopted.clone(),
        human("unlabeled", "rjwalters", "loom:issue", T2),
        bot("labeled", "loom:triage", T2),
    ];
    assert_eq!(fleet_stands(&other_actor), None);
    // Nor a successor label far from the removal.
    let far = [
        adopted.clone(),
        bot("unlabeled", "loom:issue", T2),
        bot("labeled", "loom:building", T3),
    ];
    assert_eq!(fleet_stands(&far), None);
    // Nor a label that is not a successor state.
    let not_successor = [
        adopted.clone(),
        bot("unlabeled", "loom:issue", T2),
        bot("labeled", "loom:blocked", T2),
    ];
    assert_eq!(fleet_stands(&not_successor), None);
    // Nor a "transition" by an actor that is not trusted.
    let outsider = [
        adopted.clone(),
        human("unlabeled", "turian", "loom:issue", T2),
        human("labeled", "turian", "loom:building", T2),
    ];
    assert_eq!(fleet_stands(&outsider), None);
    // A removal whose time cannot be read ends the adoption (fail closed).
    let mut unreadable = bot("unlabeled", "loom:issue", T2);
    unreadable["created_at"] = json!(null);
    assert_eq!(
        fleet_stands(&[
            adopted.clone(),
            unreadable,
            bot("labeled", "loom:building", T2)
        ]),
        None
    );
    // A removal before the adoption is not about it.
    let earlier = [human("unlabeled", "rjwalters", "loom:issue", T1), {
        let mut later = adopted.clone();
        later["created_at"] = json!(T2);
        later
    }];
    assert_eq!(fleet_stands(&earlier), Some("rjwalters".to_string()));
}

#[test]
fn a_withdrawn_adoption_is_restored_only_by_a_maintainer() {
    let withdrawn = vec![
        human("labeled", "rjwalters", "loom:issue", T1),
        human("unlabeled", "rjwalters", "loom:issue", T2),
    ];
    let mut by_bot = withdrawn.clone();
    by_bot.push(bot("labeled", "loom:issue", T3));
    assert_eq!(fleet_stands(&by_bot), None);
    let mut by_author = withdrawn.clone();
    by_author.push(human("labeled", "turian", "loom:issue", T3));
    assert_eq!(fleet_stands(&by_author), None);
    let mut by_maintainer = withdrawn;
    by_maintainer.push(human("labeled", "rjwalters", "loom:issue", T3));
    assert_eq!(fleet_stands(&by_maintainer), Some("rjwalters".to_string()));
}

/// The #13578 sequence, with the label history of rjwalters/loom#13171:
/// adopted by the maintainer, claimed by a Builder (`loom:issue` →
/// `loom:building`), `loom:issue` restored by a partial merge, sent to
/// `loom:triage` by the landed-route, re-curated.
#[test]
fn a_maintainer_label_moved_on_by_the_state_machine_and_recurated_is_trusted() {
    let events = vec![
        human("labeled", "rjwalters", "loom:issue", "2026-11-02T23:24:19Z"),
        human("unlabeled", "rjwalters", "loom:curated", "2026-11-02T23:24:20Z"),
        bot("unlabeled", "loom:issue", "2026-11-02T23:25:33Z"),
        bot("labeled", "loom:building", "2026-11-02T23:25:33Z"),
        bot("unlabeled", "loom:building", "2026-11-02T23:44:50Z"),
        bot("labeled", "loom:issue", "2026-11-02T23:44:50Z"),
        bot("labeled", "loom:triage", "2026-11-03T00:00:32Z"),
        bot("unlabeled", "loom:issue", "2026-11-03T00:00:33Z"),
        bot("labeled", "loom:curating", "2026-11-03T00:02:03Z"),
        bot("labeled", "loom:curated", "2026-11-03T00:02:38Z"),
        bot("unlabeled", "loom:curating", "2026-11-03T00:02:40Z"),
        bot("unlabeled", "loom:triage", "2026-11-03T00:02:41Z"),
    ];
    assert_eq!(fleet_stands(&events), Some("rjwalters".to_string()));
    // The landed-route removing the maintainer's own label (2AMLogic/loom-ui#4259).
    let direct = vec![
        human("labeled", "rjwalters", "loom:issue", T1),
        bot("labeled", "loom:triage", "2026-11-01T10:01:00Z"),
        bot("unlabeled", "loom:issue", "2026-11-01T10:01:01Z"),
        bot("labeled", "loom:curated", T2),
    ];
    assert_eq!(fleet_stands(&direct), Some("rjwalters".to_string()));
    // A maintainer claiming by hand (manual mode) is the same transition.
    let by_hand = vec![
        human("labeled", "rjwalters", "loom:issue", T1),
        human("unlabeled", "rjwalters", "loom:issue", T2),
        human("labeled", "rjwalters", "loom:building", T2),
    ];
    assert_eq!(fleet_stands(&by_hand), Some("rjwalters".to_string()));
    // The r7-q6 rule is unchanged: an untrusted edit after the adoption ends it.
    let edits = [edit("turian", "User", "2026-11-03T00:10:00Z")];
    assert_eq!(standing_adoption(&fleet_policy(), &events, &edits, access), None);
}

/// The label history of rjwalters/loom#12909 (outside author `turian`, `read`
/// permission): the fleet bot's `loom:issue` for 72 seconds, then the author's
/// own, and no maintainer `loom:issue` at all. It read `TRUSTED` at 9ec37a381.
#[test]
fn the_12909_history_reads_untrusted() {
    let events = vec![
        human("labeled", "turian", "loom:triage", "2026-10-20T13:43:36Z"),
        human("labeled", "turian", "loom:important", "2026-10-20T13:43:36Z"),
        bot("labeled", "loom:curating", "2026-10-20T13:45:01Z"),
        bot("labeled", "loom:curated", "2026-10-20T13:46:06Z"),
        bot("labeled", "points:3", "2026-10-20T13:46:06Z"),
        bot("unlabeled", "loom:curating", "2026-10-20T13:46:08Z"),
        bot("unlabeled", "loom:triage", "2026-10-20T13:46:09Z"),
        bot("labeled", "loom:evaluating", "2026-10-20T13:48:12Z"),
        bot("labeled", "loom:issue", "2026-10-20T13:48:26Z"),
        bot("unlabeled", "loom:evaluating", "2026-10-20T13:48:26Z"),
        bot("unlabeled", "loom:issue", "2026-10-20T13:49:38Z"),
        bot("labeled", "loom:building", "2026-10-20T13:49:38Z"),
        human("unlabeled", "turian", "loom:building", "2026-10-20T14:46:25Z"),
        human("labeled", "turian", "loom:issue", "2026-10-20T14:46:25Z"),
        bot("labeled", "loom:triage", "2026-10-20T14:48:01Z"),
        bot("unlabeled", "loom:issue", "2026-10-20T14:48:02Z"),
        bot("labeled", "loom:evaluating", "2026-10-20T14:49:35Z"),
        bot("unlabeled", "loom:curated", "2026-10-20T14:49:49Z"),
        bot("unlabeled", "loom:evaluating", "2026-10-20T14:49:49Z"),
        bot("labeled", "loom:curating", "2026-10-20T14:51:55Z"),
        bot("labeled", "loom:operator", "2026-10-20T14:52:15Z"),
        bot("unlabeled", "loom:curating", "2026-10-20T14:52:16Z"),
        bot("unlabeled", "loom:triage", "2026-10-20T14:52:18Z"),
        human("unlabeled", "rjwalters", "loom:operator", "2026-10-22T05:19:17Z"),
        human("labeled", "rjwalters", "loom:blocked", "2026-10-22T05:19:18Z"),
        human("labeled", "rjwalters", "loom:curated", "2026-10-31T19:38:20Z"),
        human("unlabeled", "rjwalters", "loom:blocked", "2026-10-31T19:38:20Z"),
        bot("labeled", "loom:evaluating", "2026-10-31T19:40:22Z"),
        bot("labeled", "loom:blocked", "2026-10-31T19:40:31Z"),
        bot("unlabeled", "loom:evaluating", "2026-10-31T19:40:32Z"),
    ];
    assert_eq!(fleet_stands(&events), None);
}
