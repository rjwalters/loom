use super::*;
use std::os::unix::fs::PermissionsExt;

fn now() -> DateTime<Utc> {
    "2026-10-03T12:00:00Z".parse().unwrap()
}

fn row(n: u32, created: &str, labels: &[&str]) -> IntakeRow {
    IntakeRow {
        number: n,
        created_at: Some(created.parse().unwrap()),
        labels: labels.iter().map(|s| (*s).to_string()).collect(),
        author: None,
    }
}

#[test]
fn unlabeled_selected_lifecycle_untouched() {
    let rows = vec![
        row(1, "2026-09-01T00:00:00Z", &[]),
        row(2, "2026-09-02T00:00:00Z", &["bug", "enhancement"]),
        row(3, "2026-09-03T00:00:00Z", &["loom:curated"]),
        row(4, "2026-09-04T00:00:00Z", &["loom:building", "bug"]),
        row(5, "2026-09-05T00:00:00Z", &["loom:epic"]),
        row(6, "2026-09-06T00:00:00Z", &["loom:triage"]),
    ];
    assert_eq!(select_unlabeled(&rows, now(), 50), vec![1, 2]);
}

#[test]
fn brand_new_issue_is_left_for_its_filer_and_cap_applies_oldest_first() {
    let rows = vec![
        row(9, "2026-10-03T11:59:30Z", &[]),
        row(8, "2026-09-02T00:00:00Z", &[]),
        row(7, "2026-09-01T00:00:00Z", &[]),
    ];
    assert_eq!(select_unlabeled(&rows, now(), 50), vec![7, 8]);
    assert_eq!(select_unlabeled(&rows, now(), 1), vec![7]);
}

/// The listing rows the pass reads (#11897): pull requests dropped, the
/// creation time parsed, the author kept.
#[test]
fn rows_from_listing_keeps_issues_with_their_facts() {
    let rows = crate::forge_listing::parse_rest_issues(
        r#"[
          {"number": 5, "state": "open", "created_at": "2026-09-01T00:00:00Z",
           "labels": [{"name": "bug"}, {"name": "loom:issue"}], "user": {"login": "alice"}},
          {"number": 6, "state": "open", "labels": [], "pull_request": {}},
          {"number": 7, "state": "open", "created_at": "bad", "labels": []}
        ]"#,
    )
    .unwrap();
    let rows = rows_from_listing(rows);
    assert_eq!(rows.iter().map(|r| r.number).collect::<Vec<_>>(), [5, 7]);
    assert_eq!(rows[0].labels, vec!["bug", "loom:issue"]);
    assert_eq!(rows[0].author.as_deref(), Some("alice"));
    assert_eq!(rows[0].created_at, Some("2026-09-01T00:00:00Z".parse().unwrap()));
    assert_eq!(rows[1].created_at, None);
    assert_eq!(rows[1].author, None);
}

/// A fake `gh` answer for the shared open-items walk: one `200` page of the
/// REST rows in `items` (a JSON array body), which is written under `dir`.
fn listing_reply(dir: &Path, items: &str) -> String {
    let body = dir.join("listing.json");
    std::fs::write(&body, items).unwrap();
    format!(
        r#"printf 'HTTP/2.0 200 OK\r\nEtag: W/"intake"\r\n\r\n'; cat '{}'"#,
        body.display()
    )
}

#[test]
fn run_once_labels_only_unlabeled_and_skips_prs() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path().display();
    // The shared walk (#11897): one page of REST rows, a PR among them.
    let listing = listing_reply(
        dir.path(),
        r#"[
 {"number": 1, "state": "open", "created_at": "2026-09-01T00:00:00Z", "labels": []},
 {"number": 2, "state": "open", "created_at": "2026-09-01T00:00:00Z", "labels": [{"name": "bug"}]},
 {"number": 3, "state": "open", "created_at": "2026-09-01T00:00:00Z", "labels": [{"name": "loom:curated"}]},
 {"number": 4, "state": "open", "created_at": "2026-09-01T00:00:00Z", "labels": [], "pull_request": {}}
]"#,
    );
    let script = format!(
        r#"#!/bin/bash
case "$*" in
  *"-X POST"*) echo "$*" >> {d}/posts.log ;;
  *"issues?state=open"*) echo "$*" >> {d}/lists.log; {listing} ;;
  *) exit 1 ;;
esac
"#
    );
    let gh = dir.path().join("gh");
    std::fs::write(&gh, script).unwrap();
    std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o755)).unwrap();

    assert_eq!(run_once(&gh, dir.path(), now(), 50), 2);
    let posts = std::fs::read_to_string(dir.path().join("posts.log")).unwrap();
    assert!(posts.contains("issues/1/labels") && posts.contains("issues/2/labels"));
    assert!(!posts.contains("issues/3/labels"));
    assert!(!posts.contains("issues/4/labels"), "a pull request is never labeled");
    assert!(posts.contains("labels[]=loom:triage"));
    // One conditional REST read of the unfiltered, oldest-first listing; no
    // raw `--paginate` (#11897).
    let lists = std::fs::read_to_string(dir.path().join("lists.log")).unwrap();
    assert_eq!(lists.lines().count(), 1, "{lists}");
    assert!(lists.contains("sort=created&direction=asc"), "{lists}");
    assert!(!lists.contains("--paginate"), "{lists}");
}

#[test]
fn due_gates_by_interval() {
    let p = Path::new("/tmp/intake-reconcile-due-test");
    assert!(due(p, 3600));
    assert!(!due(p, 3600));
}

/// The inline path demotes an issue its author opened as `loom:issue`
/// (#11820): one events read, a re-read, `loom:triage` added, `loom:issue`
/// removed, one comment; a later promotion gets one events read and nothing
/// else.
#[test]
fn run_once_demotes_an_issue_opened_past_intake() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path().display();
    let listing = listing_reply(
        dir.path(),
        r#"[
 {"number": 7, "state": "open", "created_at": "2026-10-03T10:00:00Z", "labels": [{"name": "loom:issue"}], "user": {"login": "alice"}},
 {"number": 8, "state": "open", "created_at": "2026-10-03T10:00:00Z", "labels": [{"name": "loom:issue"}], "user": {"login": "alice"}}
]"#,
    );
    let script = format!(
        r#"#!/bin/bash
case "$*" in
  *"-X POST"*"/comments"*) echo "comment $*" >> {d}/writes.log ;;
  *"-X POST"*) echo "post $*" >> {d}/writes.log; echo '[{{"name": "loom:triage"}}, {{"name": "loom:issue"}}]' ;;
  *"-X DELETE"*) echo "delete $*" >> {d}/writes.log ;;
  *issues/7/events*) printf 'loom:issue\talice\t2026-10-03T10:00:00Z\n' ;;
  *issues/8/events*) printf 'loom:issue\tchampion\t2026-10-03T11:00:00Z\n' ;;
  *issues/7*) echo '{{"state": "open", "labels": [{{"name": "loom:issue"}}]}}' ;;
  *"issues?state=open"*) {listing} ;;
  *) exit 1 ;;
esac
"#
    );
    let gh = dir.path().join("gh");
    std::fs::write(&gh, script).unwrap();
    std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o755)).unwrap();

    assert_eq!(run_once(&gh, dir.path(), now(), 50), 1);
    let writes = std::fs::read_to_string(dir.path().join("writes.log")).unwrap();
    let lines: Vec<&str> = writes.lines().collect();
    assert_eq!(lines.len(), 3, "{writes}");
    assert!(lines[0].starts_with("post") && lines[0].contains("issues/7/labels"));
    assert!(lines[0].contains("labels[]=loom:triage"));
    assert!(lines[1].starts_with("delete") && lines[1].contains("issues/7/labels/loom:issue"));
    assert!(lines[2].starts_with("comment") && lines[2].contains("issues/7/comments"));
    assert!(lines[2].contains("Curator admission"));
    assert!(!writes.contains("issues/8/"), "a promotion is never written");
}

/// A failed or shed walk is a skipped pass: nothing is labeled on a
/// listing that could not be read whole.
#[test]
fn run_once_skips_the_pass_when_the_walk_fails() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path().display();
    let script = format!(
        r#"#!/bin/bash
case "$*" in
  *"-X POST"*) echo "$*" >> {d}/posts.log ;;
  *) echo 'gh: Server Error (HTTP 502)' >&2; exit 1 ;;
esac
"#
    );
    let gh = dir.path().join("gh");
    std::fs::write(&gh, script).unwrap();
    std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o755)).unwrap();

    assert_eq!(run_once(&gh, dir.path(), now(), 50), 0);
    assert!(!dir.path().join("posts.log").exists());
}

/// With every reader out of budget the walk is shed (#11897): no request at
/// all, so not one on the writer, and the pass is skipped, not failed.
#[test]
fn run_once_skips_a_shed_walk_without_a_request() {
    use crate::forge_identity::{ExhaustCause, RouteDecision};
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path().display();
    let script = format!("#!/bin/bash\necho \"$*\" >> {d}/calls.log\nexit 1\n");
    let gh = dir.path().join("gh");
    std::fs::write(&gh, script).unwrap();
    std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o755)).unwrap();
    // A checkout whose origin names the repo, so the read is routed.
    for args in [
        &["init", "-q"][..],
        &[
            "remote",
            "add",
            "origin",
            "https://github.com/acme/intake-shed.git",
        ][..],
    ] {
        let ok = std::process::Command::new("git")
            .args(args)
            .current_dir(dir.path())
            .status()
            .unwrap()
            .success();
        assert!(ok, "git {args:?}");
    }
    let _route = crate::forge_etag_store::install_test_route(|_| RouteDecision::Exhausted {
        until: std::time::SystemTime::now() + std::time::Duration::from_secs(600),
        cause: ExhaustCause::Budget,
    });
    assert_eq!(run_once(&gh, dir.path(), now(), 50), 0);
    assert!(!dir.path().join("calls.log").exists(), "a shed walk sends nothing");
}
