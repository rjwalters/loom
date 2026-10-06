//! #9548 (H18), Judge #9593: [`read_roster_comments`] end to end through a
//! fake `gh`. An outsider's roster record must stay out of the ring (it would
//! otherwise shard this fleet's work onto a host that does not exist), the
//! same record from a trusted author must join it, and a record whose author
//! the forge did not report is untrusted.

use super::read_roster_comments;
use serial_test::serial;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

const OUTSIDER: &str =
    r#""user":{"login":"drive-by","type":"User"},"author_association":"CONTRIBUTOR""#;
const OWNER: &str = r#""user":{"login":"rjwalters","type":"User"},"author_association":"OWNER""#;
const FLEET_APP: &str =
    r#""user":{"login":"loom-fleet-dispatch[bot]","type":"Bot"},"author_association":"NONE""#;

/// One NDJSON line as the `--jq` projection prints it. `author` is the raw
/// author fields (without braces), or `""` for none at all.
fn line(id: u64, host: &str, author: &str) -> String {
    let body = format!("<!-- loom:roster host={host} serves=00000000000000aa -->");
    let sep = if author.is_empty() { "" } else { "," };
    format!(
        r#"{{"id":{id},"created_at":"2026-09-29T00:00:00Z","updated_at":"2026-09-29T00:00:00Z","body":{body:?}{sep}{author}}}"#
    )
}

/// A fake `gh` that prints `stdout` and exits 0.
fn fake_gh(dir: &Path, stdout: &str) -> PathBuf {
    let out = dir.join("out.ndjson");
    std::fs::write(&out, stdout).unwrap();
    let gh = dir.join("gh");
    std::fs::write(&gh, format!("#!/bin/sh\ncat '{}'\n", out.display())).unwrap();
    std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o755)).unwrap();
    gh
}

fn hosts(stdout: &str) -> Vec<String> {
    let dir = tempfile::tempdir().unwrap();
    let gh = fake_gh(dir.path(), stdout);
    let mut hosts: Vec<String> = read_roster_comments(&gh, dir.path(), "o", "r", 1)
        .expect("a successful read")
        .into_iter()
        .map(|c| c.host)
        .collect();
    hosts.sort();
    hosts
}

#[test]
#[serial]
fn an_untrusted_roster_record_stays_out_of_the_ring() {
    let stdout = [
        line(1, "fleet-host", FLEET_APP),
        line(2, "spoofed-host", OUTSIDER),
    ]
    .join("\n");
    assert_eq!(hosts(&stdout), vec!["fleet-host".to_string()]);
}

#[test]
#[serial]
fn a_trusted_roster_record_joins_the_ring() {
    let stdout = [
        line(1, "fleet-host", FLEET_APP),
        line(2, "owner-host", OWNER),
    ]
    .join("\n");
    assert_eq!(hosts(&stdout), vec!["fleet-host".to_string(), "owner-host".to_string()]);
}

#[test]
#[serial]
fn a_roster_record_with_no_author_is_untrusted() {
    let stdout = [
        line(1, "fleet-host", FLEET_APP),
        line(2, "anonymous-host", ""),
        line(3, "null-user-host", r#""user":null,"author_association":null"#),
    ]
    .join("\n");
    assert_eq!(hosts(&stdout), vec!["fleet-host".to_string()]);
}

/// #10089: one heartbeat read is one counted `roster.comments` call, and asks
/// for 100 comments a page (the REST default of 30 tripled the page walk on a
/// busy roster issue).
#[test]
#[serial]
fn a_roster_read_is_one_counted_100_per_page_walk() {
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("argv.log");
    let gh = dir.path().join("gh-logging");
    std::fs::write(&gh, format!("#!/bin/sh\necho \"$*\" >> '{}'\n", log.display())).unwrap();
    std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o755)).unwrap();
    let sink = tempfile::tempdir().unwrap();
    crate::forge_call_stats::set_test_sink_dir(Some(sink.path().to_path_buf()));
    let read = read_roster_comments(&gh, dir.path(), "o", "r", 1);
    let report = crate::forge_call_stats::status_report(chrono::Utc::now(), None);
    crate::forge_call_stats::set_test_sink_dir(None);
    assert_eq!(read.map(|c| c.len()), Some(0));
    let argv = std::fs::read_to_string(&log).unwrap();
    assert_eq!(argv.lines().count(), 1, "{argv}");
    // W5: page 1 of the facade's page walk (`--paginate` at the site).
    assert!(argv.contains("repos/o/r/issues/1/comments?per_page=100 --include"), "{argv}");
    let rows = report.host_window.unwrap_or_default();
    let counted: u64 = rows
        .iter()
        .filter(|r| r.caller == "roster.comments")
        .map(|r| r.ok + r.error)
        .sum();
    assert_eq!(counted, 1, "{rows:?}");
}
