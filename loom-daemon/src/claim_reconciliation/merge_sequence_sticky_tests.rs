//! Fixtures for #10398: a tree-identical re-date keeps a PR's sequencing
//! state, and an operator's removal of `loom:sequenced` is a sticky release.

use super::super::{
    hold_action, reconcile_merge_sequences_with, HoldAction, MergeSequenceStats,
    MERGE_SEQUENCE_ENABLED_ENV, SEQUENCE_LABEL,
};
use super::*;
use crate::merge_pr::sequence::{
    marker_text, parse, parse_live, release_marker_text, PredecessorState,
};

fn sha(n: u32) -> String {
    format!("{n:040x}")
}

/// The re-dated head of PR #2: a different commit, the same tree.
fn redated() -> String {
    sha(0x902)
}

fn pr(number: u32, head: &str, labels: &[&str]) -> SequencePr {
    SequencePr {
        number,
        created_at: format!("2026-10-02T00:00:0{number}Z"),
        updated_at: "2026-10-02T00:00:00Z".to_string(),
        head_sha: Some(head.to_string()),
        head_ref: format!("feature/issue-{number}"),
        base_ref: "main".to_string(),
        draft: false,
        labels: labels.iter().map(|l| (*l).to_string()).collect(),
    }
}

/// #2's soft marker after #1, pinned at #2's ORIGINAL head.
fn marker_2_after_1() -> SequenceMarker {
    SequenceMarker {
        after: 1,
        pred_head: sha(1),
        follower_head: sha(2),
        plan: "seq-aaaa0000".to_string(),
        source: Some("pass".to_string()),
    }
}

fn edge_2_after_1(follower_head: &str) -> SequenceEdge {
    SequenceEdge {
        follower: 2,
        after: 1,
        pred_head: sha(1),
        follower_head: follower_head.to_string(),
        plan: "seq-bbbb1111".to_string(),
        reason: super::super::EdgeReason::SharedFiles,
    }
}

fn label_event(event: &str, actor: Option<&str>, at: &str) -> serde_json::Value {
    let mut e = serde_json::json!({
        "event": event,
        "created_at": at,
        "label": {"name": SEQUENCE_LABEL},
    });
    if let Some(login) = actor {
        e["actor"] = serde_json::json!({ "login": login });
    }
    e
}

fn timeline(events: &[serde_json::Value]) -> Vec<u8> {
    serde_json::Value::Array(events.to_vec())
        .to_string()
        .into_bytes()
}

fn fleet(login: &str) -> bool {
    crate::forge_identity::FleetLogins::current().contains(login)
}

// --- Rule 1: tree-identical follower head moves (pure) ---------------------

#[test]
fn a_tree_identical_follower_move_is_evaluated_at_the_pinned_head() {
    let m = marker_2_after_1();
    let live = pr(2, &redated(), &[SEQUENCE_LABEL]);
    let held = effective_follower(&m, &live, |a, b| a == sha(2) && b == redated());
    assert_eq!(held.head_sha.as_deref(), Some(sha(2).as_str()));
    let pred = PredecessorState {
        open: true,
        merged: false,
        head_sha: Some(sha(1)),
        updated_at: None,
    };
    assert_eq!(
        hold_action(&m, Some(&pred), held.head_sha.as_deref(), true, 72.0),
        HoldAction::HoldSoft,
        "kept, not voided"
    );
    // Without the proof the head move still voids, exactly as before.
    assert_eq!(
        hold_action(&m, Some(&pred), live.head_sha.as_deref(), true, 72.0),
        HoldAction::VoidAndReplan
    );
}

#[test]
fn a_changed_or_unknown_tree_keeps_the_listed_head() {
    let m = marker_2_after_1();
    let live = pr(2, &redated(), &[SEQUENCE_LABEL]);
    let asked = std::cell::Cell::new(0);
    let changed = effective_follower(&m, &live, |_, _| {
        asked.set(asked.get() + 1);
        false
    });
    assert_eq!(changed.head_sha.as_deref(), Some(redated().as_str()));
    assert_eq!(asked.get(), 1);
    // An unmoved head never asks the forge.
    let unmoved = pr(2, &sha(2), &[SEQUENCE_LABEL]);
    let same = effective_follower(&m, &unmoved, |_, _| panic!("no compare for an unmoved head"));
    assert_eq!(same.head_sha.as_deref(), Some(sha(2).as_str()));
}

// --- Rule 2: the record and the label history (pure) -----------------------

#[test]
fn the_record_is_neither_a_marker_nor_a_tombstone() {
    let marker = marker_text(&marker_2_after_1());
    let bodies = vec![marker.clone(), record_text(1, &sha(2))];
    assert_eq!(parse(&bodies), Some(marker_2_after_1()));
    assert_eq!(parse_live(&bodies), Some(marker_2_after_1()));
    assert_eq!(parse_record_in_force(&bodies), Some((1, sha(2))));
}

#[test]
fn a_later_marker_or_tombstone_supersedes_the_record() {
    let rec = record_text(1, &sha(2));
    let later_marker = vec![rec.clone(), marker_text(&marker_2_after_1())];
    assert_eq!(parse_record_in_force(&later_marker), None);
    let later_release = vec![rec.clone(), release_marker_text("seq-aaaa0000")];
    assert_eq!(parse_record_in_force(&later_release), None);
    let later_replan = vec![rec.clone(), "<!-- loom:sequence replanned -->".to_string()];
    assert_eq!(parse_record_in_force(&later_replan), None);
    // A defer-repair note or prose is not sequencing history.
    let noise = vec![
        rec,
        "<!-- loom:sequence defer-repair plan=seq-1 pred=1 -->".to_string(),
        "`<!-- loom:sequence operator-released after=1 follower_head=<sha> -->`".to_string(),
    ];
    assert_eq!(parse_record_in_force(&noise), Some((1, sha(2))));
}

#[test]
fn only_a_newest_non_fleet_removal_is_an_operator_release() {
    let bot = "loom-fleet-dispatch[bot]";
    let t = |e: &[serde_json::Value]| operator_unlabeled(&timeline(e), fleet);
    let added = label_event("labeled", Some(bot), "2026-10-05T08:00:00Z");
    let by_op = label_event("unlabeled", Some("rjwalters"), "2026-10-05T08:47:00Z");
    let by_bot = label_event("unlabeled", Some(bot), "2026-10-05T08:47:00Z");
    assert_eq!(t(&[added.clone(), by_op.clone()]), Some(true));
    assert_eq!(t(&[added.clone(), by_bot]), Some(false), "a fleet removal is the pass's");
    let readded = label_event("labeled", Some(bot), "2026-10-05T08:54:00Z");
    assert_eq!(t(&[added.clone(), by_op.clone(), readded]), Some(false));
    assert_eq!(t(std::slice::from_ref(&added)), Some(false));
    assert_eq!(t(&[]), Some(false));
    // Newest by time, whatever the listing order.
    assert_eq!(t(&[by_op.clone(), added.clone()]), Some(true));
    // Unknowns: no actor, unparseable body.
    let anon = label_event("unlabeled", None, "2026-10-05T08:47:00Z");
    assert_eq!(t(&[added.clone(), anon]), None);
    assert_eq!(operator_unlabeled(b"not json", fleet), None);
    // `--paginate` concatenates pages.
    let mut paged = timeline(&[added]);
    paged.extend(timeline(&[by_op]));
    assert_eq!(operator_unlabeled(&paged, fleet), Some(true));
}

#[test]
fn the_decision_is_scoped_to_the_pair_and_the_tree() {
    let live = pr(2, &redated(), &["loom:pr"]);
    let bodies = vec![marker_text(&marker_2_after_1())];
    let same = |a: &str, b: &str| a == b || (a == sha(2) && b == redated());
    let edge = edge_2_after_1(&redated());
    assert_eq!(
        decide(&edge, &live, &bodies, same, || Some(true)),
        OperatorRelease::Detected(sha(2))
    );
    // A fleet removal / unknown history / a pass tombstone: no sticky release.
    assert_eq!(decide(&edge, &live, &bodies, same, || Some(false)), OperatorRelease::None);
    assert_eq!(decide(&edge, &live, &bodies, same, || None), OperatorRelease::None);
    let mut released = bodies.clone();
    released.push(release_marker_text("seq-aaaa0000"));
    assert_eq!(
        decide(&edge, &live, &released, same, || panic!("not read")),
        OperatorRelease::None
    );
    // A different predecessor (e.g. a consolidation reservation) is unaffected.
    let mut other = edge.clone();
    other.after = 7;
    assert_eq!(decide(&other, &live, &bodies, same, || Some(true)), OperatorRelease::None);
    // A changed tree ends it.
    let changed = |a: &str, b: &str| a == b;
    assert_eq!(decide(&edge, &live, &bodies, changed, || Some(true)), OperatorRelease::None);
    // Once recorded, the record answers without a history read.
    let mut recorded = bodies.clone();
    recorded.push(record_comment_body(1, &sha(2)));
    assert_eq!(
        decide(&edge, &live, &recorded, same, || panic!("not read")),
        OperatorRelease::Recorded
    );
    assert_eq!(
        decide(&edge, &live, &recorded, changed, || panic!("not read")),
        OperatorRelease::None,
        "the release ends when the PR's tree changes"
    );
}

// --- The whole pass (acceptance criteria) -----------------------------------

/// A fake `gh` answering from files under `dir` and logging every call.
#[cfg(unix)]
fn fake_gh(dir: &std::path::Path) -> (std::path::PathBuf, std::path::PathBuf) {
    use std::os::unix::fs::PermissionsExt;
    let log = dir.join("gh.log");
    let bin = dir.join("fake-gh-sticky.sh");
    let d = dir.display();
    let script = format!(
        "#!/usr/bin/env bash\nprintf '%s\\n' \"$*\" >> \"{log}\"\n\
         case \"$1 $2\" in\n\
         'api '*/issues/*/comments*) n=\"${{2#*/issues/}}\"; n=\"${{n%%/*}}\"; cat \"{d}/comments-$n.json\" 2>/dev/null || echo '[]' ;;\n\
         'api '*/issues/*/timeline*) n=\"${{2#*/issues/}}\"; n=\"${{n%%/*}}\"; cat \"{d}/timeline-$n.json\" || exit 1 ;;\n\
         'api '*/compare/*) cat \"{d}/compare.json\" || exit 1 ;;\n\
         'api '*/pulls/*) cat \"{d}/pull-${{2##*/}}.json\" || exit 1 ;;\n\
         'pr view') if [ \"$5\" = files ]; then cat \"{d}/files-$3.json\" || exit 1; else cat \"{d}/labels-$3.txt\" 2>/dev/null || true; fi ;;\n\
         'pr comment'|'pr edit') exit 0 ;;\n\
         *) exit 1 ;;\nesac\n",
        log = log.display(),
    );
    std::fs::write(&bin, script).unwrap();
    let mut perms = std::fs::metadata(&bin).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&bin, perms).unwrap();
    (bin, log)
}

#[cfg(unix)]
fn write(dir: &std::path::Path, name: &str, v: &serde_json::Value) {
    std::fs::write(dir.join(name), v.to_string()).unwrap();
}

#[cfg(unix)]
fn comments(bodies: &[String]) -> serde_json::Value {
    serde_json::Value::Array(
        bodies
            .iter()
            .map(|b| {
                serde_json::json!({
                    "body": b,
                    "author_association": "OWNER",
                    "user": {"login": "op", "type": "User"}
                })
            })
            .collect(),
    )
}

/// The #10370 shape: #1 (approved predecessor) and #2 share a file; #2 was
/// sequenced after #1 at head `sha(2)` and is now at `head2` with `labels2`;
/// #3 is unrelated (it only lifts the open count over the trigger).
/// `compare` answers the `sha(2)...head2` tree comparison.
#[cfg(unix)]
fn setup(
    d: &std::path::Path,
    head2: &str,
    labels2: &[&str],
    compare_files: usize,
) -> Vec<super::super::super::open_pr_listing::RestPull> {
    use super::super::super::open_pr_listing::test_support::{listing, row};
    let now = chrono::Utc::now().to_rfc3339();
    let rows = vec![
        row(1, &["loom:pr"])
            .created("2026-10-02T00:00:01Z")
            .updated(&now),
        row(2, labels2)
            .sha(head2)
            .created("2026-10-02T00:00:02Z")
            .updated(&now),
        row(3, &[]).created("2026-10-02T00:00:03Z").updated(&now),
    ];
    write(
        d,
        "pull-1.json",
        &serde_json::json!({"state": "open", "merged": false, "head": {"sha": sha(1)}, "updated_at": now}),
    );
    for (n, path) in [(1, "lib.rs"), (2, "lib.rs"), (3, "other.rs")] {
        write(d, &format!("files-{n}.json"), &serde_json::json!({"files": [{"path": path}]}));
    }
    let files: Vec<_> = (0..compare_files)
        .map(|i| serde_json::json!({"filename": format!("f{i}.rs")}))
        .collect();
    write(d, "compare.json", &serde_json::json!({"status": "ahead", "files": files}));
    let apply = super::super::apply_comment_body(
        &marker_2_after_1(),
        super::super::EdgeReason::SharedFiles,
    );
    write(d, "comments-2.json", &comments(&[apply]));
    crate::forge_pull_listing::parse_rest_pulls(&listing(&rows)).unwrap()
}

#[cfg(unix)]
fn tick(
    d: &std::path::Path,
    listing: &[super::super::super::open_pr_listing::RestPull],
) -> (MergeSequenceStats, String) {
    let root = d.join("repo");
    std::fs::create_dir_all(&root).unwrap();
    let (gh, log) = fake_gh(d);
    std::fs::write(&log, "").unwrap();
    let prev = std::env::var(MERGE_SEQUENCE_ENABLED_ENV).ok();
    std::env::remove_var(MERGE_SEQUENCE_ENABLED_ENV);
    let stats = reconcile_merge_sequences_with(&gh, &root, Some(listing));
    if let Some(v) = prev {
        std::env::set_var(MERGE_SEQUENCE_ENABLED_ENV, v);
    }
    (stats, std::fs::read_to_string(&log).unwrap())
}

#[cfg(unix)]
fn operator_removed(d: &std::path::Path, actor: &str) {
    let events = [
        label_event("labeled", Some("loom-fleet-dispatch[bot]"), "2026-10-05T07:00:00Z"),
        label_event("unlabeled", Some(actor), "2026-10-05T08:47:00Z"),
    ];
    write(d, "timeline-2.json", &serde_json::Value::Array(events.to_vec()));
}

/// Acceptance 1: an approved PR released by a non-fleet `unlabeled
/// loom:sequenced` event, then re-dated with a tree-identical commit, is
/// not re-sequenced on the next pass — and stays released after that.
#[cfg(unix)]
#[test]
#[serial_test::serial]
fn an_operator_release_survives_a_tree_identical_re_date() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    let listing = setup(d, &redated(), &["loom:pr"], 0);
    operator_removed(d, "rjwalters");

    let (stats, calls) = tick(d, &listing);
    assert_eq!(stats.applied, 0, "{calls}");
    assert!(!calls.contains("--add-label"), "not re-sequenced:\n{calls}");
    assert!(!calls.contains("Landing order recorded"), "{calls}");
    assert!(calls.contains(&record_text(1, &sha(2))), "the release is recorded:\n{calls}");

    // Next tick, with the record on the thread: no history read, no writes.
    let apply = super::super::apply_comment_body(
        &marker_2_after_1(),
        super::super::EdgeReason::SharedFiles,
    );
    write(d, "comments-2.json", &comments(&[apply, record_comment_body(1, &sha(2))]));
    let (stats, calls) = tick(d, &listing);
    assert_eq!(stats.applied, 0, "{calls}");
    assert!(!calls.contains("pr edit") && !calls.contains("pr comment"), "{calls}");
    assert!(!calls.contains("issues/2/timeline"), "the record answers:\n{calls}");
}

/// The controls for acceptance 1: a fleet removal, a changed tree, and an
/// unreadable label history all keep today's behavior (re-sequenced).
#[cfg(unix)]
#[test]
#[serial_test::serial]
fn a_fleet_removal_a_changed_tree_or_an_unknown_history_re_sequences() {
    for (case, actor, compare_files) in [
        ("fleet actor", Some("loom-fleet-dispatch-2[bot]"), 0),
        ("changed tree", Some("rjwalters"), 1),
        ("unreadable history", None, 0),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        let listing = setup(d, &redated(), &["loom:pr"], compare_files);
        if let Some(a) = actor {
            operator_removed(d, a);
        }
        let (stats, calls) = tick(d, &listing);
        assert_eq!(stats.applied, 1, "{case}:\n{calls}");
        assert!(calls.contains("pr edit 2 --add-label loom:sequenced"), "{case}:\n{calls}");
        assert!(!calls.contains("operator-released"), "{case}:\n{calls}");
    }
}

/// Acceptance 2: a tree-identical re-date of a held PR keeps its existing
/// hold and `pred_head`, and posts no new "Landing order recorded" comment.
#[cfg(unix)]
#[test]
#[serial_test::serial]
fn a_tree_identical_re_date_keeps_the_hold_and_its_pins() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    let listing = setup(d, &redated(), &["loom:pr", SEQUENCE_LABEL], 0);
    let (stats, calls) = tick(d, &listing);
    assert_eq!((stats.voided, stats.released, stats.applied), (0, 0, 0), "{calls}");
    assert_eq!(stats.held, 1, "{calls}");
    assert!(!calls.contains("pr edit"), "the label stays:\n{calls}");
    assert!(!calls.contains("pr comment"), "no replan note, no new plan:\n{calls}");
    // The marker in force is still the original one, `pred_head` included.
    let thread: Vec<String> = serde_json::from_str::<Vec<serde_json::Value>>(
        &std::fs::read_to_string(d.join("comments-2.json")).unwrap(),
    )
    .unwrap()
    .iter()
    .filter_map(|c| c["body"].as_str().map(str::to_string))
    .collect();
    assert_eq!(parse_live(&thread).map(|m| m.pred_head), Some(sha(1)));

    // Control: a re-date that changed the tree voids and re-plans as before.
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    let listing = setup(d, &redated(), &["loom:pr", SEQUENCE_LABEL], 1);
    let (stats, calls) = tick(d, &listing);
    assert_eq!(stats.voided, 1, "{calls}");
    assert!(calls.contains("pr edit 2 --remove-label loom:sequenced"), "{calls}");
    assert!(calls.contains("loom:sequence replanned"), "{calls}");
}
