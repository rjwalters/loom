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
