//! Fixtures for #10634: one landing-order comment per follower, upserted by
//! its order key, with duplicates cleaned up and one writer per workspace.

use super::super::{
    apply_comment_body, reconcile_merge_sequences_gated, EdgeReason, MergeSequenceStats,
    MERGE_SEQUENCE_ENABLED_ENV,
};
use super::*;
use crate::merge_pr::sequence::{marker_text, release_marker_text};

const BOT: &str = "loom-fleet-dispatch[bot]";

fn sha(n: u32) -> String {
    format!("{n:040x}")
}

/// #2 after #1 at the given heads and plan id.
fn marker(follower_head: &str, plan: &str) -> SequenceMarker {
    SequenceMarker {
        after: 1,
        pred_head: sha(1),
        follower_head: follower_head.to_string(),
        plan: plan.to_string(),
        source: Some("pass".to_string()),
    }
}

fn comment(id: u64, author: &str, body: String) -> ThreadComment {
    ThreadComment {
        id: Some(id),
        author: Some(author.to_string()),
        body,
    }
}

fn keyed(id: u64, m: &SequenceMarker) -> ThreadComment {
    comment(id, BOT, landing_comment_body(m, EdgeReason::SharedFiles))
}

/// A comment written before #10634: the #9686 body with no key line.
fn legacy(id: u64, m: &SequenceMarker) -> ThreadComment {
    comment(id, BOT, apply_comment_body(m, EdgeReason::SharedFiles))
}

fn ours(login: &str) -> bool {
    login == BOT
}

// --- The pure decision -------------------------------------------------------

#[test]
fn the_key_line_round_trips_and_is_not_a_sequence_marker() {
    let m = marker(&sha(2), "seq-aaaa0000");
    let body = landing_comment_body(&m, EdgeReason::SharedFiles);
    let line = landing_marker_text(&LandingKey::of(&m));
    assert!(body.starts_with(&line), "{body}");
    assert_eq!(
        line,
        format!(
            "<!-- loom:landing-order v1 after=1 after_head={} follower_head={} -->",
            sha(1),
            sha(2)
        )
    );
    let span = html_comment_spans(&line)[0];
    assert_eq!(parse_key_span(span), Some(LandingKey::of(&m)));
    // The gate's marker is still the one the body carries.
    assert_eq!(parse(&[body]), Some(m));
    assert_eq!(parse(&[line]), None);
}

#[test]
fn no_landing_comment_creates_one() {
    let want = marker(&sha(2), "seq-bbbb1111");
    let thread = vec![comment(7, "rjwalters", "LGTM".to_string())];
    let p = plan(&thread, &want, ours);
    assert_eq!(p.write, LandingWrite::Create);
    assert_eq!(p.marker, want);
    assert!(p.delete.is_empty());
}

/// The incident: the plan id churned while the order did not.
#[test]
fn the_same_order_under_a_new_plan_id_writes_nothing() {
    let old = marker(&sha(2), "seq-aaaa0000");
    let want = marker(&sha(2), "seq-bbbb1111");
    for existing in [keyed(10, &old), legacy(10, &old)] {
        let p = plan(std::slice::from_ref(&existing), &want, ours);
        assert_eq!(p.write, LandingWrite::Keep, "{}", existing.body);
        assert_eq!(p.marker, old, "the thread's own marker stays the marker of record");
        assert!(p.delete.is_empty());
    }
}

#[test]
fn a_changed_order_patches_our_comment_in_place() {
    let old = marker(&sha(2), "seq-aaaa0000");
    let moved = marker(&sha(0x902), "seq-bbbb1111");
    let p = plan(&[keyed(10, &old)], &moved, ours);
    assert_eq!(p.write, LandingWrite::Patch(10));
    assert_eq!(p.marker, moved);
    // A pre-#10634 comment is upgraded the same way.
    assert_eq!(plan(&[legacy(10, &old)], &moved, ours).write, LandingWrite::Patch(10));
    // Never someone else's words, and never an id-less row.
    let theirs = comment(10, "rjwalters", landing_comment_body(&old, EdgeReason::SharedFiles));
    assert_eq!(plan(&[theirs], &moved, ours).write, LandingWrite::Create);
    let no_id = ThreadComment {
        id: None,
        ..keyed(10, &old)
    };
    assert_eq!(plan(&[no_id], &moved, ours).write, LandingWrite::Create);
}

/// Editing a comment history has moved past would put the live marker
/// before a tombstone, and `parse_live` would read it as ended.
#[test]
fn a_later_tombstone_or_marker_forces_a_new_comment() {
    let old = marker(&sha(2), "seq-aaaa0000");
    let want = marker(&sha(2), "seq-bbbb1111");
    let moved = marker(&sha(0x902), "seq-cccc2222");
    for after in [
        release_marker_text(&old.plan),
        "<!-- loom:sequence replanned -->".to_string(),
        marker_text(&marker(&sha(2), "seq-dddd3333")),
        super::super::sticky::record_text(1, &sha(2)),
    ] {
        let thread = vec![keyed(10, &old), comment(11, BOT, after.clone())];
        assert_eq!(plan(&thread, &want, ours).write, LandingWrite::Create, "{after}");
        assert_eq!(plan(&thread, &moved, ours).write, LandingWrite::Create, "{after}");
    }
    // Unrelated chatter after it changes nothing.
    let thread = vec![
        keyed(10, &old),
        comment(11, "rjwalters", "ping".to_string()),
    ];
    assert_eq!(plan(&thread, &want, ours).write, LandingWrite::Keep);
}

/// Two hosts posted in the same second: the older copy goes, the newer stays.
#[test]
fn a_duplicate_of_our_key_is_deleted_and_the_newest_kept() {
    let a = marker(&sha(2), "seq-aaaa0000");
    let b = marker(&sha(2), "seq-bbbb1111");
    let thread = vec![keyed(10, &a), legacy(11, &b), keyed(12, &a)];
    let p = plan(&thread, &a, ours);
    assert_eq!(p.write, LandingWrite::Keep);
    assert_eq!(p.delete, vec![10, 11]);
    // A different key is history, not a duplicate; a human's copy is never deleted.
    let other = marker(&sha(0x902), "seq-cccc2222");
    let human = comment(9, "rjwalters", landing_comment_body(&a, EdgeReason::SharedFiles));
    let thread = vec![human, keyed(10, &other), keyed(12, &a)];
    assert!(plan(&thread, &a, ours).delete.is_empty());
}

#[test]
fn deletions_are_capped_per_pass() {
    let a = marker(&sha(2), "seq-aaaa0000");
    let thread: Vec<_> = (0..12).map(|i| legacy(100 + i, &a)).collect();
    let p = plan(&thread, &a, ours);
    assert_eq!(p.delete, (100..100 + MAX_DELETES_PER_PR as u64).collect::<Vec<_>>());
    assert_eq!(p.write, LandingWrite::Keep);
}

#[test]
fn thread_comments_reads_id_author_and_body() {
    let v = serde_json::json!([
        {"id": 5, "user": {"login": BOT}, "body": "x"},
        {"id": 6, "body": "y"},
        {"id": 7, "user": {"login": BOT}}
    ]);
    let rows = thread_comments(v.as_array().unwrap());
    assert_eq!(rows.len(), 2, "a row without a body is skipped");
    assert_eq!(rows[0], comment(5, BOT, "x".to_string()));
    assert_eq!((rows[1].id, rows[1].author.as_deref()), (Some(6), None));
}

// --- The whole pass ----------------------------------------------------------

/// A fake `gh` answering from files under `dir` and logging every call.
#[cfg(unix)]
fn fake_gh(dir: &std::path::Path) -> (std::path::PathBuf, std::path::PathBuf) {
    use std::os::unix::fs::PermissionsExt;
    let log = dir.join("gh.log");
    let bin = dir.join("fake-gh-landing.sh");
    let d = dir.display();
    let script = format!(
        "#!/usr/bin/env bash\nprintf '%s\\n' \"$*\" >> \"{log}\"\n\
         case \"$1 $2\" in\n\
         'api '*/issues/comments/*) exit 0 ;;\n\
         'api '*/issues/*/comments*) n=\"${{2#*/issues/}}\"; n=\"${{n%%/*}}\"; cat \"{d}/comments-$n.json\" 2>/dev/null || echo '[]' ;;\n\
         'api '*/issues/*/timeline*) exit 1 ;;\n\
         'api '*/pulls/*) cat \"{d}/pull-${{2##*/}}.json\" || exit 1 ;;\n\
         'api --include') n=\"${{3%/files*}}\"; n=\"${{n##*/}}\"; [ -f \"{d}/files-$n.json\" ] || exit 1;\
           printf 'HTTP/2.0 200 OK\\r\\n\\r\\n'; cat \"{d}/files-$n.json\" ;;\n\
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

/// #1 (approved) and #2 share a file; #3 lifts the open count over the
/// trigger. #2 is at `head2`, unlabeled, with `thread` as its comments.
#[cfg(unix)]
fn setup(
    d: &std::path::Path,
    head2: &str,
    thread: &[ThreadComment],
) -> Vec<super::super::super::open_pr_listing::RestPull> {
    use super::super::super::open_pr_listing::test_support::{listing, row};
    let now = chrono::Utc::now().to_rfc3339();
    let rows = vec![
        row(1, &["loom:pr"])
            .created("2026-10-02T00:00:01Z")
            .updated(&now),
        row(2, &[])
            .sha(head2)
            .created("2026-10-02T00:00:02Z")
            .updated(&now),
        row(3, &[]).created("2026-10-02T00:00:03Z").updated(&now),
    ];
    let write = |name: &str, v: serde_json::Value| {
        std::fs::write(d.join(name), v.to_string()).unwrap();
    };
    write(
        "pull-1.json",
        serde_json::json!({"state": "open", "merged": false, "head": {"sha": sha(1)}, "updated_at": now}),
    );
    for (n, path) in [(1, "lib.rs"), (2, "lib.rs"), (3, "other.rs")] {
        write(&format!("files-{n}.json"), serde_json::json!([{"filename": path}]));
    }
    let rows_json: Vec<_> = thread
        .iter()
        .map(|c| {
            serde_json::json!({
                "id": c.id,
                "body": c.body,
                "author_association": "OWNER",
                "user": {"login": c.author, "type": "Bot"}
            })
        })
        .collect();
    write("comments-2.json", serde_json::Value::Array(rows_json));
    crate::forge_pull_listing::parse_rest_pulls(&listing(&rows)).unwrap()
}

#[cfg(unix)]
fn tick(
    d: &std::path::Path,
    listing: &[super::super::super::open_pr_listing::RestPull],
    owned: bool,
) -> (MergeSequenceStats, String) {
    let root = d.join("repo");
    std::fs::create_dir_all(&root).unwrap();
    let (gh, log) = fake_gh(d);
    std::fs::write(&log, "").unwrap();
    let prev = std::env::var(MERGE_SEQUENCE_ENABLED_ENV).ok();
    std::env::remove_var(MERGE_SEQUENCE_ENABLED_ENV);
    let stats = reconcile_merge_sequences_gated(&gh, &root, Some(listing), owned);
    if let Some(v) = prev {
        std::env::set_var(MERGE_SEQUENCE_ENABLED_ENV, v);
    }
    (stats, std::fs::read_to_string(&log).unwrap())
}

#[cfg(unix)]
fn count(calls: &str, needle: &str) -> usize {
    calls.lines().filter(|l| l.contains(needle)).count()
}

/// Same order already on the thread under an older plan id: no comment write
/// (the missing label is still healed).
#[cfg(unix)]
#[test]
#[serial_test::serial]
fn same_plan_posts_and_edits_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    let listing = setup(d, &sha(2), &[keyed(10, &marker(&sha(2), "seq-00000000"))]);
    let (stats, calls) = tick(d, &listing, true);
    assert_eq!(count(&calls, "pr comment"), 0, "{calls}");
    assert_eq!(count(&calls, "issues/comments/"), 0, "{calls}");
    assert_eq!(count(&calls, "--add-label loom:sequenced"), 1, "{calls}");
    assert_eq!(count(&calls, "issues/2/comments"), 1, "one listing read:\n{calls}");
    assert_eq!(stats.applied, 1, "{calls}");
}

#[cfg(unix)]
#[test]
#[serial_test::serial]
fn a_changed_plan_is_one_edit() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    // #2 was pushed since the comment pinned it.
    let listing = setup(d, &sha(0x902), &[keyed(10, &marker(&sha(2), "seq-00000000"))]);
    let (_, calls) = tick(d, &listing, true);
    assert_eq!(count(&calls, "pr comment"), 0, "{calls}");
    assert_eq!(count(&calls, "issues/comments/10 --method PATCH"), 1, "{calls}");
    assert!(calls.contains(&format!("follower_head={}", sha(0x902))), "{calls}");
    assert_eq!(count(&calls, "issues/2/comments"), 1, "{calls}");
}

#[cfg(unix)]
#[test]
#[serial_test::serial]
fn no_comment_is_one_create() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    let listing = setup(d, &sha(2), &[]);
    let (stats, calls) = tick(d, &listing, true);
    assert_eq!(count(&calls, "pr comment 2"), 1, "{calls}");
    assert!(calls.contains("loom:landing-order v1 after=1"), "{calls}");
    assert_eq!(count(&calls, "issues/comments/"), 0, "{calls}");
    assert_eq!(stats.applied, 1);
}

/// The same-second race left two copies: the older one is deleted and no
/// third is posted.
#[cfg(unix)]
#[test]
#[serial_test::serial]
fn a_duplicate_marker_is_cleaned_up() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    let m = marker(&sha(2), "seq-00000000");
    let listing = setup(d, &sha(2), &[keyed(10, &m), keyed(11, &m)]);
    let (_, calls) = tick(d, &listing, true);
    assert_eq!(count(&calls, "issues/comments/10 --method DELETE"), 1, "{calls}");
    assert_eq!(count(&calls, "issues/comments/11"), 0, "the newest stays:\n{calls}");
    assert_eq!(count(&calls, "pr comment"), 0, "{calls}");
}

/// Single writer: a host outside the workspace's shard makes no forge call.
#[cfg(unix)]
#[test]
#[serial_test::serial]
fn a_host_that_does_not_own_the_workspace_stays_silent() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    let listing = setup(d, &sha(2), &[]);
    let (stats, calls) = tick(d, &listing, false);
    assert!(calls.is_empty(), "{calls}");
    assert_eq!(stats, MergeSequenceStats::default());
}

/// An unreadable thread is not an empty one: no post on a failed read.
#[cfg(unix)]
#[test]
#[serial_test::serial]
fn a_failed_comments_read_skips_the_edge() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    let listing = setup(d, &sha(2), &[]);
    std::fs::write(d.join("comments-2.json"), "not json").unwrap();
    let (stats, calls) = tick(d, &listing, true);
    assert_eq!(count(&calls, "pr comment"), 0, "{calls}");
    assert_eq!(count(&calls, "pr edit"), 0, "{calls}");
    assert_eq!(stats.applied, 0);
}
