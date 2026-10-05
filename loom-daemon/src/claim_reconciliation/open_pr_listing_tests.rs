//! #10349: the PR-side reconciliation passes list open PRs through the ETag'd
//! REST listing — never GraphQL `gh pr list` — and an unchanged workspace is
//! answered entirely by `304`s.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::test_support::{listing, mergeable_arm, pulls_arm, pulls_arm_cmd, row};
use super::*;
use crate::claim_reconciliation::pass_loop::run_reconciliation_pass_over_roots;
use crate::claim_reconciliation::STALE_REVIEWING_MINUTES_ENV;
use crate::write_scope_test_support::WritableRoot;
use serial_test::serial;
use std::path::PathBuf;

const SHA_A: &str = "1111111111111111111111111111111111111111";
const SHA_B: &str = "2222222222222222222222222222222222222222";

#[test]
fn rest_mergeable_maps_onto_the_graphql_tristate() {
    assert_eq!(Mergeable::from_rest(Some(true)), Mergeable::Mergeable);
    assert_eq!(Mergeable::from_rest(Some(false)), Mergeable::Conflicting);
    assert_eq!(Mergeable::from_rest(None), Mergeable::Unknown);
}

#[test]
fn with_label_filters_client_side_and_keeps_the_old_limit() {
    let rows: Vec<RestPull> = (1..=150)
        .map(|n| {
            let labels: &[&str] = if n % 2 == 0 { &["loom:pr"] } else { &[] };
            crate::forge_pull_listing::parse_rest_pulls(&listing(&[row(n, labels)]))
                .unwrap()
                .remove(0)
        })
        .collect();
    let got = with_label(rows.clone(), "loom:pr");
    assert_eq!(got.len(), 75);
    assert!(got.iter().all(|r| r.number % 2 == 0));
    let many: Vec<RestPull> = rows
        .into_iter()
        .map(|mut r| {
            r.labels = vec!["loom:pr".to_string()];
            r
        })
        .collect();
    assert_eq!(with_label(many, "loom:pr").len(), MAX_ISSUES_PER_WORKSPACE as usize);
}

fn write_script(path: &Path, body: &str) {
    std::fs::write(path, body).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
}

/// Restore an env var on drop.
struct EnvGuard(&'static str, Option<String>);
impl EnvGuard {
    fn set(key: &'static str, value: impl AsRef<std::ffi::OsStr>) -> Self {
        let prev = std::env::var(key).ok();
        std::env::set_var(key, value);
        Self(key, prev)
    }
}
impl Drop for EnvGuard {
    fn drop(&mut self) {
        match &self.1 {
            Some(v) => std::env::set_var(self.0, v),
            None => std::env::remove_var(self.0),
        }
    }
}

fn run_pass(root: &Path, gh: &Path) {
    let stats =
        run_reconciliation_pass_over_roots(&[root.to_path_buf()], gh, false, || false);
    assert_eq!(stats.roots_processed, 1);
}

/// AC1: a whole pass over a root with a stale claim, a stale verdict and a
/// base-conflicting review-queue PR still makes every decision — while any
/// GraphQL `gh pr list` fails the stub outright.
#[cfg(unix)]
#[test]
#[serial]
fn a_pass_decides_everything_without_a_graphql_pr_list() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("repo");
    std::fs::create_dir_all(&root).unwrap();
    let _journal = EnvGuard::set(
        crate::sweep_journal::JOURNAL_PATH_ENV,
        dir.path().join("sweeps.json"),
    );
    let _stale = EnvGuard::set(STALE_REVIEWING_MINUTES_ENV, "30");
    let log = dir.path().join("gh.log");
    let old = (chrono::Utc::now() - chrono::Duration::minutes(180)).to_rfc3339();
    let rows = [
        row(500, &["loom:reviewing"]).head("some-random-branch").updated(&old),
        row(192, &["loom:pr"]).sha(SHA_B),
        row(300, &["loom:review-requested"]),
    ];
    let comments = format!(
        r#"[{{"user":{{"login":"loom-fleet-dispatch[bot]","type":"Bot"}},"author_association":"NONE","created_at":"2026-08-23T06:00:00Z","body":"Reviewed.\n\n<!-- loom:verdict-sha sha={SHA_A} verdict=approved -->"}}]"#
    );
    let script = format!(
        r#"#!/bin/sh
printf '%s\n' "$*" >> '{log}'
case "$*" in "pr list"*)
  echo 'GraphQL pr list is forbidden on the periodic path' 1>&2
  exit 97 ;;
esac
{pulls}{merge}case "$*" in api*'issues?labels='*)
  printf 'HTTP/2.0 200 OK\r\n\r\n'
  echo '[]'
  exit 0 ;;
esac
case "$*" in api*'/comments'*) echo '{comments}' ;; esac
exit 0
"#,
        log = log.display(),
        pulls = pulls_arm(&rows),
        merge = mergeable_arm("false"),
    );
    let inner = dir.path().join("fake-gh.sh");
    write_script(&inner, &script);
    let ws = WritableRoot::register_with_gh(&root, &inner);

    run_pass(&root, &ws.gh);

    let calls = std::fs::read_to_string(&log).unwrap();
    assert!(
        calls.lines().all(|l| !l.starts_with("pr list")),
        "no GraphQL pr list on the periodic path:\n{calls}"
    );
    assert!(calls.contains("pulls?state=open"), "{calls}");
    assert!(
        calls.contains("pr edit 500 --remove-label loom:reviewing"),
        "the stale claim is reclaimed:\n{calls}"
    );
    assert!(
        calls
            .lines()
            .any(|l| l.starts_with("pr edit 192") && l.contains("--remove-label loom:pr")),
        "the stale verdict is cleared:\n{calls}"
    );
    assert!(
        calls
            .lines()
            .any(|l| l.starts_with("pr edit 300") && l.contains("--add-label loom:merge-conflict")),
        "the base conflict is flagged:\n{calls}"
    );
}

/// A stub serving a quiet workspace — a fresh review-queue PR that merges
/// cleanly and a fresh `loom:reviewing` claim — with `200 + ETag` and a `304`
/// (exit 1, like gh) when that ETag is presented. Every `200` is counted.
fn write_etag_stub(dir: &Path) -> (PathBuf, PathBuf, PathBuf) {
    let log = dir.join("gh.log");
    let oks = dir.join("oks.log");
    let now = chrono::Utc::now().to_rfc3339();
    let rows = [
        row(11, &["loom:review-requested"]).updated(&now),
        row(12, &["loom:reviewing"]).updated(&now),
    ];
    let script = format!(
        r#"#!/bin/sh
printf '%s\n' "$*" >> '{log}'
case "$*" in *'If-None-Match'*)
  printf 'HTTP/2.0 304 Not Modified\r\n\r\n'
  echo 'gh: Not Modified (HTTP 304)' 1>&2
  exit 1 ;;
esac
case "$*" in api*'pulls?state=open'*)
  echo list >> '{oks}'
  printf 'HTTP/2.0 200 OK\r\nEtag: W/"pulls1"\r\n\r\n'
  echo '{body}'
  exit 0 ;;
esac
case "$*" in api*'/pulls/'[0-9]*)
  echo mergeable >> '{oks}'
  printf 'HTTP/2.0 200 OK\r\nEtag: W/"m1"\r\n\r\n'
  echo '{{"mergeable": true}}'
  exit 0 ;;
esac
case "$*" in api*'issues?labels='*)
  printf 'HTTP/2.0 200 OK\r\nEtag: W/"issues1"\r\n\r\n'
  echo '[]'
  exit 0 ;;
esac
exit 0
"#,
        log = log.display(),
        oks = oks.display(),
        body = listing(&rows),
    );
    let bin = dir.join("fake-gh-etag.sh");
    write_script(&bin, &script);
    (bin, log, oks)
}

/// AC2 + AC4: on an unchanged workspace the second pass is answered only by
/// `304`s (no `200` at all), and every listing call is accounted under a
/// `claim_reconciliation.*` caller.
#[cfg(unix)]
#[test]
#[serial]
fn an_unchanged_workspace_costs_only_304s_and_is_accounted() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("repo");
    std::fs::create_dir_all(&root).unwrap();
    let store_dir = dir.path().join("etag-store");
    let sink = dir.path().join("sink");
    let _journal = EnvGuard::set(
        crate::sweep_journal::JOURNAL_PATH_ENV,
        dir.path().join("sweeps.json"),
    );
    let (inner, log, oks) = write_etag_stub(dir.path());
    let ws = WritableRoot::register_with_gh(&root, &inner);
    crate::forge_etag_store::set_test_daemon_store_dir(Some(store_dir));
    crate::forge_call_stats::set_test_sink_dir(Some(sink));

    run_pass(&root, &ws.gh);
    let oks_after_first = std::fs::read_to_string(&oks).unwrap_or_default();
    let first_len = std::fs::read_to_string(&log).unwrap().lines().count();
    run_pass(&root, &ws.gh);
    let report = crate::forge_call_stats::status_report(chrono::Utc::now(), None);

    crate::forge_call_stats::set_test_sink_dir(None);
    crate::forge_etag_store::set_test_daemon_store_dir(None);

    assert_eq!(
        oks_after_first.lines().filter(|l| *l == "list").count(),
        1,
        "one 200 for the listing on the first pass; the other passes revalidate"
    );
    assert_eq!(
        std::fs::read_to_string(&oks).unwrap(),
        oks_after_first,
        "the second pass gets no 200 at all"
    );
    let calls = std::fs::read_to_string(&log).unwrap();
    let second: Vec<&str> = calls.lines().skip(first_len).collect();
    let pulls: Vec<&&str> = second.iter().filter(|l| l.contains("/pulls")).collect();
    assert!(!pulls.is_empty(), "{calls}");
    assert!(
        pulls.iter().all(|l| l.contains("If-None-Match")),
        "every pass-2 PR read is conditional:\n{calls}"
    );
    assert!(calls.lines().all(|l| !l.starts_with("pr list")), "{calls}");

    let callers: Vec<String> = report
        .host_window
        .expect("sink opted in")
        .into_iter()
        .map(|r| r.caller)
        .collect();
    for want in [
        "claim_reconciliation.pr_list",
        "claim_reconciliation.pr_mergeable",
    ] {
        assert!(callers.iter().any(|c| c == want), "{want} accounted: {callers:?}");
    }
}

/// The listing a stub serves can change between passes (`pulls_arm_cmd`
/// re-reads a file); a changed listing is re-read, never served stale.
#[cfg(unix)]
#[test]
#[serial]
fn each_pass_sees_the_current_listing() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("repo");
    std::fs::create_dir_all(&root).unwrap();
    let body = dir.path().join("listing.json");
    std::fs::write(&body, listing(&[row(1, &["loom:pr"])])).unwrap();
    let script = format!(
        "#!/bin/sh\n{}exit 0\n",
        pulls_arm_cmd(&format!("cat '{}'", body.display()))
    );
    let gh = dir.path().join("fake-gh.sh");
    write_script(&gh, &script);
    let first = list_with_label(&gh, &root, "loom:pr").unwrap();
    std::fs::write(&body, listing(&[row(1, &[]), row(2, &["loom:pr"])])).unwrap();
    let second = list_with_label(&gh, &root, "loom:pr").unwrap();
    assert_eq!(first.iter().map(|r| r.number).collect::<Vec<_>>(), vec![1]);
    assert_eq!(second.iter().map(|r| r.number).collect::<Vec<_>>(), vec![2]);
}
