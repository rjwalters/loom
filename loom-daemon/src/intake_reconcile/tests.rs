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

#[test]
fn parse_rows_reads_tsv() {
    let rows =
        parse_rows("5\t2026-09-01T00:00:00Z\tbug,loom:issue\n6\t2026-09-02T00:00:00Z\t\nbad\n");
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].labels, vec!["bug", "loom:issue"]);
    assert!(rows[1].labels.is_empty());
}

#[test]
fn run_once_labels_only_unlabeled_and_skips_prs() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path().display();
    // The fake emulates the jq output (PR already dropped by the jq filter).
    let script = format!(
        r#"#!/bin/bash
case "$*" in
  *"-X POST"*) echo "$*" >> {d}/posts.log ;;
  *--paginate*) printf '1\t2026-09-01T00:00:00Z\t\n2\t2026-09-01T00:00:00Z\tbug\n3\t2026-09-01T00:00:00Z\tloom:curated\n' ;;
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
    assert!(posts.contains("labels[]=loom:triage"));
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
    let script = format!(
        r#"#!/bin/bash
case "$*" in
  *"-X POST"*"/comments"*) echo "comment $*" >> {d}/writes.log ;;
  *"-X POST"*) echo "post $*" >> {d}/writes.log; echo '[{{"name": "loom:triage"}}, {{"name": "loom:issue"}}]' ;;
  *"-X DELETE"*) echo "delete $*" >> {d}/writes.log ;;
  *issues/7/events*) printf 'loom:issue\talice\t2026-10-03T10:00:00Z\n' ;;
  *issues/8/events*) printf 'loom:issue\tchampion\t2026-10-03T11:00:00Z\n' ;;
  *issues/7*) echo '{{"state": "open", "labels": [{{"name": "loom:issue"}}]}}' ;;
  *--paginate*) printf '7\t2026-10-03T10:00:00Z\tloom:issue\talice\n8\t2026-10-03T10:00:00Z\tloom:issue\talice\n' ;;
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

#[test]
fn parse_rows_reads_the_author_column() {
    let rows =
        parse_rows("5\t2026-09-01T00:00:00Z\tloom:issue\talice\n6\t2026-09-01T00:00:00Z\t\t\n");
    assert_eq!(rows[0].author.as_deref(), Some("alice"));
    assert_eq!(rows[1].author, None);
}
