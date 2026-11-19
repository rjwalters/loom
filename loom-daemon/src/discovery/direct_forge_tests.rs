//! The direct backend on both forges (#12312): a GitHub fixture and a Gitea
//! fixture through the same `list_repo`, the ETag rule, Gitea's 50-row pages,
//! and the fleet `gh` tier's isolated environment.

use std::sync::atomic::AtomicU64;

use super::*;
use crate::forge_cmd::ForgeType;
use crate::gh_invocation::discovery_isolation as isolation;

fn stub(dir: &std::path::Path, script: &str) -> std::path::PathBuf {
    let gh = dir.join("gh");
    std::fs::write(&gh, format!("#!/bin/sh\n{script}")).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    gh
}

fn target(dir: &std::path::Path, gh: std::path::PathBuf, tag: &str) -> Target {
    Target {
        slug: format!("acme/direct-{tag}-{}", std::process::id()),
        gh_bin: gh,
        root_dir: dir.to_path_buf(),
    }
}

/// Recorded Gitea REST shapes: `type=issues` holds issues only, `type=pulls`
/// holds the pull requests (with a `pull_request` object), `/pulls` the PR
/// facts. No `sub_issues_summary`, no `author_association`, no check runs.
const GITEA: &str = r#"
echo "$*" >> "$CALLS"
case "$*" in
  *'/pulls?'*)
    printf 'HTTP/1.1 200 OK\r\nETag: "p1"\r\n\r\n'
    echo '[{"number": 12, "state": "open", "title": "pr", "labels": [{"name": "loom:review-requested"}], "user": {"login": "carol"}, "head": {"ref": "feature/issue-7", "sha": "abc123"}, "base": {"ref": "main"}, "created_at": "2026-10-16T09:00:00Z"}]' ;;
  *'type=pulls'*)
    printf 'HTTP/1.1 200 OK\r\nETag: "r1"\r\n\r\n'
    echo '[{"number": 12, "state": "open", "title": "pr", "labels": [{"name": "loom:review-requested"}], "user": {"login": "carol"}, "pull_request": {"merged": false}, "created_at": "2026-10-16T09:00:00Z"}]' ;;
  *)
    printf 'HTTP/1.1 200 OK\r\nETag: "i1"\r\n\r\n'
    echo '[{"number": 7, "state": "open", "title": "a", "labels": [{"name": "loom:issue"}], "user": {"login": "alice"}, "created_at": "2026-10-15T00:00:00Z"}, {"number": 8, "state": "open", "title": "b", "labels": [], "user": {"login": "bob"}, "created_at": "2026-10-15T01:00:00Z"}]' ;;
esac
"#;

#[test]
fn a_gitea_repo_snapshots_issues_and_pulls_with_optional_fields_none() {
    let dir = tempfile::tempdir().unwrap();
    let calls = dir.path().join("calls.log");
    let gh = stub(dir.path(), &format!("CALLS='{}'\n{GITEA}", calls.display()));
    let t = target(dir.path(), gh, "gitea");
    let reads = AtomicU64::new(0);
    let snap = list_repo(ForgeType::Gitea, &t, &reads, Tier::Gh).unwrap();

    let numbers: Vec<_> = snap.rows.iter().map(|r| r.number).collect();
    assert_eq!(numbers, [7, 8, 12], "issues, then pulls");
    assert!(snap.rows[2].is_pull_request && !snap.rows[0].is_pull_request);
    // What the draw needs is there; what Gitea cannot give is `None`.
    assert_eq!(snap.rows[0].labels, ["loom:issue"]);
    assert!(snap.rows.iter().all(|r| r.labels_at.is_none()));
    assert!(snap.rows.iter().all(|r| r.open_sub_issues.is_none()));
    assert!(snap.rows.iter().all(|r| r.author_association.is_none()));
    let pulls = snap.pulls.expect("the PR facts listed");
    assert_eq!(pulls[0].head_sha.as_deref(), Some("abc123"));
    assert_eq!(pulls[0].base_ref.as_deref(), Some("main"));
    assert_eq!(snap.tier, Tier::Gh);

    let log = std::fs::read_to_string(&calls).unwrap();
    assert_eq!(log.lines().count(), 3, "{log}");
    assert!(log.contains("type=issues") && log.contains("type=pulls"), "{log}");
    assert!(log.contains("limit=50"), "Gitea pages by limit: {log}");
    assert!(!log.contains("per_page"), "{log}");
}

#[test]
fn a_github_repo_keeps_todays_one_issues_walk_and_one_pulls_walk() {
    let dir = tempfile::tempdir().unwrap();
    let calls = dir.path().join("calls.log");
    let gh = stub(
        dir.path(),
        &format!(
            r#"echo "$*" >> '{}'
printf 'HTTP/2.0 200 OK\r\nETag: W/"g1"\r\n\r\n'
case "$*" in
  *'/pulls?'*) echo '[{{"number": 5, "state": "open", "head": {{"ref": "f", "sha": "s"}}, "base": {{"ref": "main"}}, "labels": []}}]' ;;
  *) echo '[{{"number": 4, "state": "open", "labels": []}}, {{"number": 5, "state": "open", "labels": [], "pull_request": {{}}}}]' ;;
esac
"#,
            calls.display()
        ),
    );
    let t = target(dir.path(), gh, "github");
    let snap = list_repo(ForgeType::GitHub, &t, &AtomicU64::new(0), Tier::Gh).unwrap();
    assert_eq!(snap.rows.iter().map(|r| r.number).collect::<Vec<_>>(), [4, 5]);
    let log = std::fs::read_to_string(&calls).unwrap();
    assert_eq!(log.lines().count(), 2, "{log}");
    assert!(
        log.contains("issues?state=open&sort=created&direction=asc&per_page=100"),
        "{log}"
    );
    assert!(!log.contains("type="), "{log}");
}

/// A response without an ETag is never a stale cache hit: the second listing
/// of a forge that sends none re-reads and sees the new body.
#[test]
fn a_response_without_an_etag_is_never_served_from_cache() {
    let dir = tempfile::tempdir().unwrap();
    let counter = dir.path().join("n");
    let gh = stub(
        dir.path(),
        &format!(
            r#"echo x >> '{c}'
n=$(wc -l < '{c}' | tr -d ' ')
case "$*" in *'If-None-Match'*) echo "sent a validator" >&2; exit 1;; esac
printf 'HTTP/1.1 200 OK\r\n\r\n'
case "$*" in
  *'type=issues'*) echo "[{{\"number\": $n, \"state\": \"open\", \"labels\": []}}]" ;;
  *) echo '[]' ;;
esac
"#,
            c = counter.display()
        ),
    );
    let t = target(dir.path(), gh, "noetag");
    let first = list_repo(ForgeType::Gitea, &t, &AtomicU64::new(0), Tier::Gh).unwrap();
    let second = list_repo(ForgeType::Gitea, &t, &AtomicU64::new(0), Tier::Gh).unwrap();
    assert_ne!(
        first.rows[0].number, second.rows[0].number,
        "the second listing read the forge again"
    );
}

/// Gitea's page is 50 rows: a full page asks for the next one (the `Link`
/// header names it), where GitHub's 100-row rule would have stopped.
#[test]
fn gitea_pages_by_fifty() {
    let dir = tempfile::tempdir().unwrap();
    let gh = stub(
        dir.path(),
        r#"case "$*" in
  *'type=issues'*'page=2'*)
    printf 'HTTP/1.1 200 OK\r\nETag: "b"\r\n\r\n'; echo '[{"number": 51, "state": "open", "labels": []}]' ;;
  *'type=issues'*)
    printf 'HTTP/1.1 200 OK\r\nETag: "a"\r\nLink: <https://x/api/v1/next?page=2>; rel="next"\r\n\r\n'
    i=1; printf '['; while [ $i -le 50 ]; do [ $i -gt 1 ] && printf ','; printf '{"number": %s, "state": "open", "labels": []}' $i; i=$((i+1)); done; echo ']' ;;
  *) printf 'HTTP/1.1 200 OK\r\nETag: "e"\r\n\r\n'; echo '[]' ;;
esac
"#,
    );
    let t = target(dir.path(), gh, "paging");
    let snap = list_repo(ForgeType::Gitea, &t, &AtomicU64::new(0), Tier::Gh).unwrap();
    assert_eq!(snap.rows.len(), 51);
}

/// The fleet `gh` tier: its own config dir, no token variable, never a fleet
/// profile directory. The ambient token variables are not set here (they would
/// change every concurrent test's credential cache key); that the facade
/// removes each of them is `gh_invocation`'s
/// `without_token_env_removes_every_token_variable`.
#[test]
fn the_fleet_gh_tier_runs_in_an_isolated_environment() {
    static ENV: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _env = ENV
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("env.log");
    let gh = stub(
        dir.path(),
        &format!(
            r#"echo "cfg=${{GH_CONFIG_DIR:-unset}} gh_token=${{GH_TOKEN:-unset}} github_token=${{GITHUB_TOKEN:-unset}} enterprise=${{GH_ENTERPRISE_TOKEN:-unset}}" >> '{}'
printf 'HTTP/2.0 200 OK\r\nETag: W/"x"\r\n\r\n'
echo '[]'
"#,
            log.display()
        ),
    );
    let isolated = dir.path().join("discovery-gh");
    std::env::set_var(isolation::DIR_ENV, &isolated);
    let t = target(dir.path(), gh, "isolated");
    let fleet = read_repo_fleet(&t, &AtomicU64::new(0), Tier::Gh);
    std::env::remove_var(isolation::DIR_ENV);
    fleet.unwrap();

    let seen = std::fs::read_to_string(&log).unwrap();
    assert!(!seen.is_empty());
    for line in seen.lines() {
        assert!(line.contains(&format!("cfg={}", isolated.display())), "{line}");
        assert!(line.contains("gh_token=unset"), "{line}");
        assert!(line.contains("github_token=unset"), "{line}");
        assert!(line.contains("enterprise=unset"), "{line}");
    }
    assert!(!isolation::active(), "the scope ended with the listing");
}
