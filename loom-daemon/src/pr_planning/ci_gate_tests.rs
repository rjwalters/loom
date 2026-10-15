//! #11971: Judge is not served a PR while CI is pending at its head.
use super::ci_gate::{cached, ci_pending, clear_cache, CI_SCREENED_ROWS};
use super::*;
use chrono::{Duration, TimeZone, Utc};

const HEAD: &str = "0123456789abcdef0123456789abcdef01234567";

fn runs(status: &str, started: Option<String>) -> Value {
    json!({"total_count": 1, "check_runs": [{"status": status, "started_at": started}]})
}

fn no_statuses() -> Value {
    json!({"state": "pending", "statuses": []})
}

/// `ci_pending` over one check-run page and one combined status.
fn pending(page: Value, combined: Value) -> bool {
    let now = Utc.with_ymd_and_hms(2026, 10, 14, 12, 0, 0).unwrap();
    ci_pending(HEAD, now, |path| {
        Some(if path.contains("/status?") {
            combined.clone()
        } else {
            page.clone()
        })
    })
}

#[test]
fn pending_means_a_young_unconcluded_run_or_status() {
    let at = |h: i64| {
        Some(
            (Utc.with_ymd_and_hms(2026, 10, 14, 12, 0, 0).unwrap() - Duration::hours(h))
                .to_rfc3339(),
        )
    };
    assert!(pending(runs("in_progress", at(1)), no_statuses()), "running");
    assert!(pending(runs("queued", at(0)), no_statuses()), "queued");
    assert!(!pending(runs("completed", at(1)), no_statuses()), "concluded");
    assert!(
        !pending(runs("in_progress", at(7)), no_statuses()),
        "stuck past the 6 h cap is served"
    );
    assert!(!pending(runs("queued", None), no_statuses()), "no stamp counts as stale");
    let status = json!({"state": "pending",
        "statuses": [{"state": "pending", "created_at": at(1)}]});
    assert!(pending(runs("completed", at(1)), status), "a pending legacy status");
    assert!(!ci_pending(HEAD, Utc::now(), |_| None), "unreadable is not pending");
}

#[test]
#[serial_test::serial(loom_config_env)]
fn an_answer_is_reused_per_head() {
    let root = Path::new("/nonexistent/ci-gate-cache");
    let mut reads = 0;
    let mut ask = |sha: &str, answer: bool| {
        cached(root, sha, || {
            reads += 1;
            answer
        })
    };
    assert!(ask("a1", true));
    assert!(ask("a1", false), "the cached answer for the same head");
    assert!(!ask("b2", false), "a new head is a new read");
    assert_eq!(reads, 2);
}

/// A fake `gh` serving `n` labeled PRs (heads `HEAD`-derived), whose check
/// runs come from `checks.json`, logging every call to `calls.log`.
#[cfg(unix)]
fn fake_repo(n: u64) -> (tempfile::TempDir, std::path::PathBuf) {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::create_dir_all(root.join(".loom/scripts")).unwrap();
    let gh = root.join("gh");
    std::fs::write(
        &gh,
        "#!/bin/sh\ncd \"$(dirname \"$0\")\"\necho \"$*\" >> calls.log\nprintf 'HTTP/2 200 OK\\r\\n\\r\\n'\ncase \"$*\" in\n  *check-runs*) cat checks.json;;\n  */status*) cat status.json;;\n  *comments*) echo '[]';;\n  *) cat pulls.json;;\nesac\n",
    )
    .unwrap();
    std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o755)).unwrap();
    let body = "<!-- loom:provenance v1 build=unknown unknown unknown prompts=unknown unknown sweep=none story=none trace=unknown host=unknown base=unknown run=none origin=autonomous -->";
    let rows: Vec<Value> = (1..=n)
        .map(|i| {
            let head = format!("feature/issue-{i}");
            json!({"number":i,"state":"open","draft":false,"title":"test",
                "created_at":format!("2026-09-{i:02}"),"body":body,
                "user":{"login":"session-app[bot]","type":"Bot"},"author_association":"NONE",
                "labels":[{"name":"loom:review-requested"}],
                "head":{"ref":head,"sha":format!("{HEAD}{i}"),"label":format!("acme:{head}"),
                    "repo":{"id":100,"full_name":"acme/widgets"}},
                "base":{"ref":"main","repo":{"id":100,"full_name":"acme/widgets"}}})
        })
        .collect();
    std::fs::write(root.join("pulls.json"), serde_json::to_vec(&rows).unwrap()).unwrap();
    std::fs::write(root.join("status.json"), serde_json::to_vec(&no_statuses()).unwrap()).unwrap();
    (dir, gh)
}

#[cfg(unix)]
fn set_checks(root: &Path, page: &Value) {
    std::fs::write(root.join("checks.json"), serde_json::to_vec(page).unwrap()).unwrap();
}

/// The acceptance case, through the real `pr-queue --role judge` path: pending at the head is neither served nor gated; the
/// same head is served once CI concludes; CI stuck past the cap is served.
#[cfg(unix)]
#[test]
#[serial_test::serial(loom_config_env)]
fn a_pr_with_ci_pending_at_its_head_is_not_served_to_judge() {
    let (dir, gh) = fake_repo(1);
    let root = dir.path();
    let served = || {
        clear_cache();
        fetch_queue(root, &gh, PrRole::Judge).unwrap().len()
    };
    let ago = |h: i64| Some((Utc::now() - Duration::hours(h)).to_rfc3339());

    set_checks(root, &runs("in_progress", ago(1)));
    assert_eq!(served(), 0, "CI pending at the head");
    set_checks(root, &runs("completed", ago(1)));
    assert_eq!(served(), 1, "CI concluded at the same head");
    set_checks(root, &runs("queued", ago(7)));
    assert_eq!(served(), 1, "pending past the 6 h cap");
}

/// Only the head of the queue is read: past [`CI_SCREENED_ROWS`] CI-clear
/// rows, the rest are served without a CI read.
#[cfg(unix)]
#[test]
#[serial_test::serial(loom_config_env)]
fn only_the_rows_about_to_be_served_are_read() {
    let (dir, gh) = fake_repo(5);
    let root = dir.path();
    set_checks(root, &runs("completed", Some(Utc::now().to_rfc3339())));
    clear_cache();
    assert_eq!(fetch_queue(root, &gh, PrRole::Judge).unwrap().len(), 5);
    let log = std::fs::read_to_string(root.join("calls.log")).unwrap();
    assert_eq!(log.matches("check-runs").count(), CI_SCREENED_ROWS);
}
