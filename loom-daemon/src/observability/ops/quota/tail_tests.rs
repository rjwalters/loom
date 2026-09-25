//! Tests for the incremental line reader (Issue #8930).

use std::io::Write;
use std::path::{Path, PathBuf};

use chrono::{Duration, Utc};

use super::{read_lines_from, TailSet};

fn append(path: &Path, text: &str) {
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .unwrap();
    file.write_all(text.as_bytes()).unwrap();
}

/// Poll `paths` and return `(state, line)` pairs, where each file's state
/// counts the lines it has delivered since its cursor was last reset.
fn poll(set: &mut TailSet<usize>, paths: &[PathBuf]) -> Vec<(usize, String)> {
    let mut lines = Vec::new();
    set.poll(paths.to_vec(), Utc::now() - Duration::hours(1), |seen, line| {
        *seen += 1;
        lines.push((*seen, line.to_string()));
    });
    lines
}

#[test]
fn each_line_is_delivered_once_and_a_partial_line_waits_for_its_newline() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a.jsonl");
    append(&path, "one\ntwo\nthr");
    let mut set = TailSet::default();
    let files = [path.clone()];
    assert_eq!(poll(&mut set, &files), vec![(1, "one".into()), (2, "two".into())]);
    assert!(poll(&mut set, &files).is_empty(), "nothing new, nothing re-read");
    append(&path, "ee\nfour\n");
    assert_eq!(poll(&mut set, &files), vec![(3, "three".into()), (4, "four".into())]);
}

#[test]
fn a_truncated_or_replaced_file_is_read_again_with_fresh_state() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a.jsonl");
    append(&path, "one\ntwo\n");
    let mut set = TailSet::default();
    let files = [path.clone()];
    poll(&mut set, &files);
    std::fs::write(&path, "new\n").unwrap();
    assert_eq!(poll(&mut set, &files), vec![(1, "new".into())], "shorter than the cursor");
    // Replaced by a different file (new inode) that is already longer.
    let replacement = dir.path().join("b.jsonl");
    std::fs::write(&replacement, "r1\nr2\nr3\n").unwrap();
    std::fs::rename(&replacement, &path).unwrap();
    let lines = poll(&mut set, &files);
    if cfg!(unix) {
        assert_eq!(lines.len(), 3, "a new inode is a new file: {lines:?}");
        assert_eq!(lines[0], (1, "r1".into()));
    }
}

#[cfg(unix)]
#[test]
fn a_file_reached_through_a_symlinked_directory_is_read_once() {
    let dir = tempfile::tempdir().unwrap();
    let real = dir.path().join("real");
    std::fs::create_dir(&real).unwrap();
    std::os::unix::fs::symlink(&real, dir.path().join("alias")).unwrap();
    append(&real.join("a.jsonl"), "one\n");
    let mut set = TailSet::default();
    let files = [
        real.join("a.jsonl"),
        dir.path().join("alias").join("a.jsonl"),
    ];
    assert_eq!(poll(&mut set, &files).len(), 1);
    append(&real.join("a.jsonl"), "two\n");
    assert_eq!(poll(&mut set, &files), vec![(2, "two".into())]);
}

#[test]
fn files_not_written_recently_or_no_longer_listed_are_dropped() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a.jsonl");
    append(&path, "one\n");
    let mut set: TailSet<usize> = TailSet::default();
    set.poll([path.clone()], Utc::now() + Duration::hours(1), |_, _| {
        panic!("a file older than the activity bound is not read");
    });
    assert_eq!(set.tracked().count(), 0);
    poll(&mut set, &[path.clone()]);
    assert_eq!(set.tracked().count(), 1);
    poll(&mut set, &[]);
    assert_eq!(set.tracked().count(), 0);
}

#[test]
fn an_over_long_line_is_skipped_through_its_newline() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a.jsonl");
    append(&path, &format!("short\n{}\nafter\n", "x".repeat(50)));
    let mut offset = 0;
    let mut lines = Vec::new();
    read_lines_from(&path, &mut offset, 16, |line| lines.push(line.to_string())).unwrap();
    assert_eq!(lines, vec!["short".to_string(), "after".to_string()]);
    assert_eq!(offset, std::fs::metadata(&path).unwrap().len());
}
