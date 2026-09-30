use std::cell::RefCell;

use serde_json::json;

use super::*;
use crate::fleet_store::fetch::Reply;

/// A fake [`WriteTransport`] that answers a scripted status per call, or the
/// happy-path default, and records every call it saw.
struct FakeWriter {
    /// `(status, body)` to answer with, keyed by the API path prefix that
    /// triggers it (checked with `starts_with`); the happy-path default
    /// otherwise.
    overrides: Vec<(&'static str, u16, &'static str)>,
    calls: RefCell<Vec<(String, String, Value)>>,
}

impl FakeWriter {
    fn new() -> Self {
        Self {
            overrides: Vec::new(),
            calls: RefCell::new(Vec::new()),
        }
    }

    fn failing(prefix: &'static str, status: u16, body: &'static str) -> Self {
        Self {
            overrides: vec![(prefix, status, body)],
            calls: RefCell::new(Vec::new()),
        }
    }
}

impl WriteTransport for FakeWriter {
    fn write(&self, method: &str, api_path: &str, body: &Value) -> anyhow::Result<Reply> {
        self.calls
            .borrow_mut()
            .push((method.to_string(), api_path.to_string(), body.clone()));
        for (prefix, status, resp_body) in &self.overrides {
            if api_path.starts_with(prefix) {
                return Ok(Reply {
                    status: *status,
                    etag: None,
                    body: (*resp_body).to_string(),
                });
            }
        }
        let body = if api_path.contains("/pulls") {
            json!({"number": 42, "html_url": "https://example.test/pr/42"}).to_string()
        } else {
            String::new()
        };
        Ok(Reply {
            status: 201,
            etag: None,
            body,
        })
    }
}

fn sample_proposal() -> Proposal {
    Proposal {
        branch: branch_name("state", "20260930T000000Z"),
        title: "fleet-config: set build-1 state to paused".to_string(),
        body: "body text".to_string(),
        files: vec![FileChange {
            path: "fleet/state.yml".to_string(),
            before: Some("fleet:\n  state: running\n".to_string()),
            before_sha: Some("deadbeef".to_string()),
            after: "fleet:\n  state: paused\n".to_string(),
        }],
    }
}

#[test]
fn submit_creates_a_branch_writes_the_file_then_opens_the_pr() {
    let wt = FakeWriter::new();
    let proposal = sample_proposal();
    let pr =
        submit(&wt, "acme/fleet", "main", "0123456789abcdef0123456789abcdef01234567", &proposal)
            .unwrap();
    assert_eq!(pr.number, Some(42));
    assert_eq!(pr.url.as_deref(), Some("https://example.test/pr/42"));

    let calls = wt.calls.borrow();
    assert_eq!(calls.len(), 3);

    assert_eq!(calls[0].0, "POST");
    assert_eq!(calls[0].1, "repos/acme/fleet/git/refs");
    assert_eq!(calls[0].2["sha"], json!("0123456789abcdef0123456789abcdef01234567"));
    assert_eq!(calls[0].2["ref"], json!(format!("refs/heads/{}", proposal.branch)));

    assert_eq!(calls[1].0, "PUT");
    assert_eq!(calls[1].1, "repos/acme/fleet/contents/fleet/state.yml");
    assert_eq!(calls[1].2["sha"], json!("deadbeef"));
    assert_eq!(calls[1].2["branch"], json!(proposal.branch));

    assert_eq!(calls[2].0, "POST");
    assert_eq!(calls[2].1, "repos/acme/fleet/pulls");
    assert_eq!(calls[2].2["head"], json!(proposal.branch));
    assert_eq!(calls[2].2["base"], json!("main"));
}

#[test]
fn a_new_file_has_no_sha_in_its_contents_write() {
    let wt = FakeWriter::new();
    let mut proposal = sample_proposal();
    proposal.files[0].before_sha = None;
    submit(&wt, "acme/fleet", "main", "0123456789abcdef0123456789abcdef01234567", &proposal)
        .unwrap();
    let calls = wt.calls.borrow();
    assert!(calls[1].2.get("sha").is_none());
}

#[test]
fn a_403_on_the_branch_create_is_a_clear_missing_scope_error_not_a_crash() {
    let wt = FakeWriter::failing(
        "repos/acme/fleet/git/refs",
        403,
        r#"{"message":"Resource not accessible by integration"}"#,
    );
    let err = submit(
        &wt,
        "acme/fleet",
        "main",
        "0123456789abcdef0123456789abcdef01234567",
        &sample_proposal(),
    )
    .unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("contents: write"), "{msg}");
    assert!(msg.contains("pull_requests: write"), "{msg}");
    assert!(msg.contains("Resource not accessible by integration"), "{msg}");
    // Never opens the PR after a failed branch create.
    assert_eq!(wt.calls.borrow().len(), 1);
}

#[test]
fn a_404_writing_a_file_is_also_reported_as_a_missing_scope() {
    let wt = FakeWriter::failing("repos/acme/fleet/contents/", 404, r#"{"message":"Not Found"}"#);
    let err = submit(
        &wt,
        "acme/fleet",
        "main",
        "0123456789abcdef0123456789abcdef01234567",
        &sample_proposal(),
    )
    .unwrap_err();
    assert!(err.to_string().contains("contents: write"));
    // The branch create ran; the PR create never did.
    assert_eq!(wt.calls.borrow().len(), 2);
}

#[test]
fn an_unrelated_server_error_is_reported_plainly_without_the_scope_hint() {
    let wt = FakeWriter::failing("repos/acme/fleet/git/refs", 500, r#"{"message":"boom"}"#);
    let err = submit(
        &wt,
        "acme/fleet",
        "main",
        "0123456789abcdef0123456789abcdef01234567",
        &sample_proposal(),
    )
    .unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("HTTP 500"));
    assert!(!msg.contains("contents: write"), "{msg}");
}

#[test]
fn provenance_marker_carries_the_base_commit_and_is_one_hidden_line() {
    let dir = tempfile::tempdir().unwrap();
    let sha = "0123456789abcdef0123456789abcdef01234567";
    let line = provenance_marker(dir.path(), sha);
    assert!(line.starts_with("<!-- loom:provenance v1 "));
    assert!(line.ends_with("-->"));
    assert_eq!(line.matches("-->").count(), 1);
    assert!(line.contains(&format!("base={sha}")));
    // No linked issue behind this action: `none`, not `unknown` (D33).
    assert!(line.contains("story=none"));
}

#[test]
fn drop_unchanged_keeps_real_edits_and_new_files_but_drops_no_ops() {
    let unchanged = FileChange {
        path: "repos.yml".to_string(),
        before: Some("repos:\n".to_string()),
        before_sha: Some("aaa".to_string()),
        after: "repos:\n".to_string(),
    };
    let edited = sample_proposal().files.remove(0);
    let created = FileChange {
        path: "fleet/hosts/build-9/local.json".to_string(),
        before: None,
        before_sha: None,
        after: "{}\n".to_string(),
    };
    let kept = drop_unchanged(vec![unchanged, edited.clone(), created.clone()]);
    assert_eq!(kept, vec![edited, created]);
}

#[test]
fn drop_unchanged_on_an_all_no_op_set_is_empty_so_no_pr_is_opened() {
    let noop = FileChange {
        path: "repos.yml".to_string(),
        before: Some("x\n".to_string()),
        before_sha: Some("aaa".to_string()),
        after: "x\n".to_string(),
    };
    assert!(drop_unchanged(vec![noop]).is_empty());
}

#[test]
fn branch_names_are_namespaced_and_carry_the_kind_and_stamp() {
    assert_eq!(
        branch_name("priority", "20260930T000000Z"),
        "loom/fleet-propose/priority-20260930T000000Z"
    );
}

#[test]
fn proposal_report_includes_the_branch_title_and_every_files_diff() {
    let proposal = sample_proposal();
    let report = proposal.report();
    assert!(report.contains(&proposal.branch));
    assert!(report.contains(&proposal.title));
    assert!(report.contains("-  state: running"));
    assert!(report.contains("+  state: paused"));
}
