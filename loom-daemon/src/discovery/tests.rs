use std::io::{Read as _, Write as _};
use std::net::{Ipv4Addr, TcpListener};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde_json::{json, Value};
use serial_test::serial;

use super::mirror::Fetched;
use super::*;
use crate::forge_mirror::client::{Conditional, MirrorError};

fn now() -> DateTime<Utc> {
    DateTime::parse_from_rfc3339("2026-10-16T12:00:00Z")
        .unwrap()
        .with_timezone(&Utc)
}

fn target(slug: &str) -> Target {
    Target {
        slug: slug.to_string(),
        gh_bin: PathBuf::from("gh"),
        root_dir: PathBuf::from("."),
    }
}

fn worklist_body() -> Value {
    json!({ "repos": {
        "acme/widgets": {
            "watermark": {
                "known": true, "repo": "acme/widgets", "latest_seq": 9,
                "last_event_at": "2026-10-16T11:59:00Z",
                "last_reconciled_at": "2026-10-16T11:58:00Z",
            },
            "items": [
                { "number": 7, "type": "issue", "state": "open", "title": "an issue",
                  "labels": ["loom:issue"], "labels_at": { "loom:issue": "2026-10-16T10:00:00Z" },
                  "assignees": ["bob"], "author": "alice", "author_association": "MEMBER",
                  "created_at": "2026-10-15T00:00:00Z", "updated_at": "2026-10-16T10:00:00Z",
                  "sub_issues_total": 3, "sub_issues_completed": 1,
                  "blocked_by_total": 2, "blocked_by_open": 1 },
                { "number": 8, "type": "pr", "state": "open", "labels": ["loom:review-requested"],
                  "author": "carol", "author_association": "NONE", "body": "Closes #7",
                  "draft": true, "head_ref": "feature/issue-7", "head_sha": "abc123",
                  "head_repo": "acme/widgets", "base_ref": "main",
                  "checks": { "state": "pending", "started_at": "2026-10-16T11:00:00Z" } },
                { "number": 9, "type": "issue", "state": "closed", "labels": [] },
            ],
        },
        "acme/old": {
            "watermark": { "known": true, "repo": "acme/old", "latest_seq": 1,
                           "last_reconciled_at": "2026-10-16T08:00:00Z" },
            "items": [],
        },
    }})
}

fn modified(etag: &str) -> Fetched {
    Fetched {
        outcome: Ok(Conditional::Modified {
            etag: Some(etag.to_string()),
            body: worklist_body(),
        }),
        latency: Duration::from_millis(12),
    }
}

fn failed(status: u16) -> Fetched {
    Fetched {
        outcome: Err(MirrorError::Status(status)),
        latency: Duration::from_millis(3),
    }
}

#[test]
fn the_backend_is_direct_unless_config_says_mirror() {
    let kind = |v: Value| BackendKind::from_effective(&v);
    assert_eq!(kind(json!({})), BackendKind::Direct);
    assert_eq!(
        kind(json!({"autonomous": {"workFinder": {"discovery": "mirror"}}})),
        BackendKind::Mirror
    );
    assert_eq!(
        kind(json!({"autonomous": {"workFinder": {"discovery": "Direct"}}})),
        BackendKind::Direct
    );
    assert_eq!(
        kind(json!({"autonomous": {"workFinder": {"discovery": 3}}})),
        BackendKind::Direct
    );
}

#[test]
fn a_worklist_converts_to_rows_pulls_and_extras() {
    let mut d = Discovery::default();
    let snap = d.begin_mirror_tick_with(now(), |etag| {
        assert_eq!(etag, None, "the first request is unconditional");
        modified("\"w1\"")
    });
    let repo = snap.repo(&target("Acme/Widgets")).unwrap();
    assert_eq!(repo.source, Source::Worklist);
    // The closed item is not a row.
    let numbers: Vec<u32> = repo.rows.iter().map(|r| r.number).collect();
    assert_eq!(numbers, vec![7, 8]);
    let issue = &repo.rows[0];
    assert_eq!(issue.open_sub_issues, Some(2));
    assert_eq!(issue.author_association.as_deref(), Some("MEMBER"));
    assert_eq!(
        issue
            .labels_at
            .as_ref()
            .unwrap()
            .get("loom:issue")
            .map(String::as_str),
        Some("2026-10-16T10:00:00Z")
    );
    assert!(repo.rows[1].is_pull_request);
    let pulls = repo.pulls.as_ref().unwrap();
    assert_eq!(pulls.len(), 1);
    assert!(pulls[0].draft);
    assert_eq!(pulls[0].head_sha.as_deref(), Some("abc123"));
    assert_eq!(pulls[0].head_ref.as_deref(), Some("feature/issue-7"));
    assert_eq!(pulls[0].base_ref.as_deref(), Some("main"));
    assert_eq!(pulls[0].head_repo.as_deref(), Some("acme/widgets"));
    let extras = &repo.extras;
    assert_eq!(extras[&7].assignees, vec!["bob".to_string()]);
    assert_eq!(extras[&7].blocked_by_total, Some(2));
    assert_eq!(extras[&7].blocked_by_open, Some(1));
    assert_eq!(extras[&8].checks.as_ref().unwrap().state, "pending");
    assert!(extras[&7].checks.is_none());
}

#[test]
fn a_tick_makes_one_request_and_reads_no_forge_listing() {
    let mut d = Discovery::default();
    let mut requests = 0;
    let snap = d.begin_mirror_tick_with(now(), |_| {
        requests += 1;
        modified("\"w1\"")
    });
    snap.repo(&target("acme/widgets")).unwrap();
    snap.repo(&target("acme/old")).unwrap();
    assert_eq!(requests, 1, "one worklist request serves every repo");
    let stats = snap.stats();
    assert_eq!(stats.backend, "mirror");
    assert_eq!(stats.discovery_forge_reads, 0);
    assert_eq!(stats.worklist_latency_ms, Some(12));
    assert_eq!(stats.not_modified, Some(false));
    assert_eq!(stats.repos, 2);
    assert_eq!(stats.items, 2);
}

#[test]
fn an_unchanged_etag_reuses_the_previous_snapshot() {
    let mut d = Discovery::default();
    d.begin_mirror_tick_with(now(), |_| modified("\"w1\""));
    let snap = d.begin_mirror_tick_with(now(), |etag| {
        assert_eq!(etag, Some("\"w1\""), "the next request is conditional");
        Fetched {
            outcome: Ok(Conditional::NotModified),
            latency: Duration::from_millis(2),
        }
    });
    assert_eq!(snap.repo(&target("acme/widgets")).unwrap().rows.len(), 2);
    let stats = snap.stats();
    assert_eq!(stats.not_modified, Some(true));
    assert!(!stats.reused_previous);
    assert_eq!(stats.discovery_forge_reads, 0);
}

#[test]
fn a_failed_request_uses_the_previous_snapshot_and_never_the_forge() {
    let mut d = Discovery::default();
    d.begin_mirror_tick_with(now(), |_| modified("\"w1\""));
    let snap = d.begin_mirror_tick_with(now(), |_| failed(503));
    assert_eq!(snap.repo(&target("acme/widgets")).unwrap().rows.len(), 2);
    let stats = snap.stats();
    assert!(stats.reused_previous);
    assert_eq!(stats.not_modified, Some(false));
    assert_eq!(stats.discovery_forge_reads, 0);
}

#[test]
fn a_failed_request_with_nothing_before_fails_every_repo_closed() {
    let mut d = Discovery::default();
    let snap = d.begin_mirror_tick_with(now(), |_| failed(500));
    let err = snap.repo(&target("acme/widgets")).unwrap_err();
    assert!(format!("{err:#}").contains("no worklist"), "{err:#}");
    assert_eq!(snap.stats().discovery_forge_reads, 0);
}

#[test]
fn a_malformed_worklist_is_a_failed_request() {
    let mut d = Discovery::default();
    let snap = d.begin_mirror_tick_with(now(), |_| Fetched {
        outcome: Ok(Conditional::Modified {
            etag: None,
            body: json!({ "repos": "nope" }),
        }),
        latency: Duration::ZERO,
    });
    assert!(snap.repo(&target("acme/widgets")).is_err());
}

#[test]
fn a_repo_the_key_cannot_read_is_an_error_for_that_repo_only() {
    let mut d = Discovery::default();
    let snap = d.begin_mirror_tick_with(now(), |_| modified("\"w1\""));
    let err = snap.repo(&target("acme/secret")).unwrap_err();
    assert!(format!("{err:#}").contains("not in the worklist"), "{err:#}");
    assert!(snap.repo(&target("acme/widgets")).is_ok());
}

#[test]
fn a_stale_repo_is_served_with_its_watermark_recorded() {
    let mut d = Discovery::default();
    let snap = d.begin_mirror_tick_with(now(), |_| modified("\"w1\""));
    // acme/old was last reconciled four hours ago: past the 600 s bound.
    let old = snap.repo(&target("acme/old")).unwrap();
    assert!(old.rows.is_empty());
    let mark = old.watermark.as_ref().unwrap();
    assert_eq!(mark.age_secs(now()), Some(4 * 3600));
    snap.repo(&target("acme/widgets")).unwrap();
    let stats = snap.stats();
    assert_eq!(stats.oldest_watermark_age_secs, Some(4 * 3600));
    assert_eq!(stats.stale_repos, 1);
    assert_eq!(stats.discovery_forge_reads, 0);
}

#[test]
fn the_repo_is_read_once_per_tick() {
    let mut d = Discovery::default();
    let snap = d.begin_mirror_tick_with(now(), |_| modified("\"w1\""));
    let a = snap.repo(&target("acme/widgets")).unwrap();
    let b = snap.repo(&target("ACME/WIDGETS")).unwrap();
    assert!(Arc::ptr_eq(&a, &b));
    assert_eq!(snap.repos().len(), 1);
}

#[test]
fn the_direct_backend_reports_no_worklist_facts_until_it_lists() {
    let mut d = Discovery::default();
    let snap = d.begin_tick(BackendKind::Direct, now());
    let stats = snap.stats();
    assert_eq!(stats.backend, "direct");
    assert_eq!(stats.worklist_latency_ms, None);
    assert_eq!(stats.not_modified, None);
    assert_eq!(stats.discovery_forge_reads, 0);
}

#[test]
fn the_mirror_backend_never_reaches_the_per_call_listing_path() {
    for file in ["mirror.rs", "worklist.rs"] {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("src/discovery")
            .join(file);
        let text = std::fs::read_to_string(path).unwrap();
        let code: String = text
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(!code.contains("list_open"), "{file} must not call a per-repo listing");
        assert!(!code.contains("forge_mirror::discovery"), "{file}");
        assert!(!code.contains("forge_listing::list"), "{file}");
    }
}

// ----------------------------------------------------------- over HTTP

type Seen = Arc<Mutex<Vec<(String, Option<String>)>>>;

/// Answers each connection in turn: `(status, etag, body)`. Records the
/// request line and its `If-None-Match`.
fn serve(responses: Vec<(u16, Option<&'static str>, String)>) -> (String, Seen) {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let addr = listener.local_addr().unwrap();
    let seen: Seen = Arc::new(Mutex::new(Vec::new()));
    let seen_t = seen.clone();
    std::thread::spawn(move || {
        for (i, stream) in listener.incoming().enumerate() {
            let Ok(mut stream) = stream else { break };
            let mut buf = Vec::new();
            let mut b = [0u8; 1];
            while buf.len() < 16_384 && !buf.ends_with(b"\r\n\r\n") {
                match stream.read(&mut b) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => buf.push(b[0]),
                }
            }
            let head = String::from_utf8_lossy(&buf).into_owned();
            let mut lines = head.lines();
            let line = lines.next().unwrap_or_default().to_string();
            let inm = lines
                .find(|l| l.to_ascii_lowercase().starts_with("if-none-match:"))
                .map(|l| l[14..].trim().to_string());
            seen_t.lock().unwrap().push((line, inm));
            let (status, etag, body) = responses
                .get(i)
                .or_else(|| responses.last())
                .cloned()
                .unwrap();
            let etag = etag.map_or(String::new(), |e| format!("ETag: {e}\r\n"));
            let head = format!(
                "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\n{etag}Content-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let _ = stream.write_all(head.as_bytes());
            let _ = stream.write_all(body.as_bytes());
        }
    });
    (format!("http://{addr}"), seen)
}

#[test]
#[serial]
fn against_a_fake_server_a_tick_is_one_request_and_the_next_is_a_304() {
    let (base, seen) = serve(vec![
        (200, Some("\"w1\""), worklist_body().to_string()),
        (304, None, String::new()),
        (503, None, "{}".to_string()),
    ]);
    crate::forge_mirror::set_active(Some(Arc::new(crate::forge_mirror::Active::for_test(
        &base,
        Duration::from_secs(600),
    ))));
    let mut d = Discovery::default();

    let first = d.begin_tick(BackendKind::Mirror, now());
    assert_eq!(first.repo(&target("acme/widgets")).unwrap().rows.len(), 2);
    assert_eq!(first.stats().not_modified, Some(false));

    let second = d.begin_tick(BackendKind::Mirror, now());
    assert_eq!(second.repo(&target("acme/widgets")).unwrap().rows.len(), 2);
    assert_eq!(second.stats().not_modified, Some(true));
    assert_eq!(second.stats().discovery_forge_reads, 0);

    let third = d.begin_tick(BackendKind::Mirror, now());
    assert_eq!(third.repo(&target("acme/widgets")).unwrap().rows.len(), 2);
    assert!(third.stats().reused_previous, "a 5xx keeps the previous snapshot");

    crate::forge_mirror::set_active(None);
    let seen = seen.lock().unwrap().clone();
    assert_eq!(seen.len(), 3, "one request per tick, nothing else");
    assert!(
        seen.iter()
            .all(|(line, _)| line.starts_with("GET /v1/mirror/worklist ")),
        "{seen:?}"
    );
    assert_eq!(seen[0].1, None);
    assert_eq!(seen[1].1.as_deref(), Some("\"w1\""));
}

#[test]
#[serial]
fn an_inactive_mirror_with_no_previous_snapshot_fails_closed() {
    crate::forge_mirror::set_active(None);
    let mut d = Discovery::default();
    let snap = d.begin_tick(BackendKind::Mirror, now());
    let err = snap.repo(&target("acme/widgets")).unwrap_err();
    assert!(format!("{err:#}").contains("no worklist"), "{err:#}");
}
