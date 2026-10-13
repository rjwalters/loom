//! Tests for [`super`] (#11486): forge-host selection for terminal activity
//! ingestion. Every test that resolves a host reads the process-global
//! `LOOM_FORGE_TYPE` (via `detect_forge`) and so is `#[serial(loom_config_env)]`.

use super::*;
use crate::activity::{AgentInput, InputContext, InputType};
use crate::types::{AgentStatus, TerminalInfo};
use chrono::Utc;
use serde_json::json;
use serial_test::serial;
use std::process::Command;
use tempfile::{tempdir, TempDir};

fn isolate_env() {
    std::env::set_var("LOOM_CONFIG_DEFAULTS_FILE", "");
    std::env::remove_var("LOOM_FORGE_TYPE");
}

fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// A git repo with `origin` = `remote` (if any) and `.loom/config.json`
/// containing `config` (if any).
fn repo(remote: Option<&str>, config: Option<&Value>) -> TempDir {
    let dir = tempdir().unwrap();
    git(dir.path(), &["init", "-q"]);
    if let Some(r) = remote {
        git(dir.path(), &["remote", "add", "origin", r]);
    }
    if let Some(c) = config {
        let loom = dir.path().join(".loom");
        std::fs::create_dir_all(&loom).unwrap();
        std::fs::write(loom.join("config.json"), c.to_string()).unwrap();
    }
    dir
}

fn gitea_repo() -> TempDir {
    repo(
        Some("git@gitea.example.com:owner/repo.git"),
        Some(&json!({"forge": {"type": "gitea", "gitea": {"url": "https://gitea.example.com"}}})),
    )
}

fn terminal(working_dir: Option<&Path>, worktree: Option<&Path>) -> TerminalInfo {
    TerminalInfo {
        id: "t".into(),
        name: "t".into(),
        tmux_session: "t".into(),
        working_dir: working_dir.map(|p| p.display().to_string()),
        created_at: 0,
        role: None,
        worktree_path: worktree.map(|p| p.display().to_string()),
        agent_pid: None,
        agent_status: AgentStatus::default(),
        last_interval_run: None,
    }
}

const OUTPUT: &str = "\
https://gitea.example.com/owner/repo/issues/7
https://gitea.example.com/owner/repo/pulls/42
https://github.com/owner/repo/issues/99
https://github.com/owner/repo/pull/100
";

/// Rows of `prompt_github` as `(event_type, issue, pr, input_id)`.
type Row = (String, Option<i32>, Option<i32>, Option<i64>);

/// Run the ingestion step against a fresh activity DB and read the rows back.
fn ingest(host: Option<&str>, output: &str) -> (Vec<Row>, i64) {
    let dir = tempdir().unwrap();
    let path = dir.path().join("activity.db");
    let db = ActivityDb::new(path.clone()).unwrap();
    let input_id = db
        .record_input(&AgentInput {
            id: None,
            terminal_id: "t".into(),
            timestamp: Utc::now(),
            input_type: InputType::Manual,
            content: "tea pr create".into(),
            agent_role: None,
            context: InputContext::default(),
        })
        .unwrap();
    record_forge_events(&db, output, host, Some(input_id));
    drop(db);
    let conn = rusqlite::Connection::open(&path).unwrap();
    let mut stmt = conn
        .prepare(
            "SELECT event_type, issue_number, pr_number, input_id FROM prompt_github ORDER BY id",
        )
        .unwrap();
    let rows = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
        .unwrap()
        .collect::<Result<Vec<Row>, _>>()
        .unwrap();
    (rows, input_id)
}

#[test]
#[serial(loom_config_env)]
fn gitea_repo_creation_urls_reach_activity_records_with_input_id() {
    isolate_env();
    let r = gitea_repo();
    let host = host_for_terminal(Some(&terminal(Some(r.path()), None)));
    assert_eq!(host.as_deref(), Some("gitea.example.com"));

    let (rows, input_id) = ingest(host.as_deref(), OUTPUT);
    let created = PromptForgeEventType::IssueCreated.as_str().to_string();
    let pr = PromptForgeEventType::PrCreated.as_str().to_string();
    // github.com URLs are host-negative for a Gitea workspace.
    assert_eq!(
        rows,
        vec![
            (created, Some(7), None, Some(input_id)),
            (pr, None, Some(42), Some(input_id))
        ]
    );
}

#[test]
#[serial(loom_config_env)]
fn github_repo_keeps_github_matching_and_host_negative() {
    isolate_env();
    let r = repo(Some("git@github.com:owner/repo.git"), None);
    let res = resolve_forge_host(Some(r.path()));
    assert_eq!(
        res,
        ForgeHostResolution::Resolved {
            host: "github.com".into(),
            warning: None
        }
    );

    let (rows, input_id) = ingest(res.host(), OUTPUT);
    let numbers: Vec<_> = rows.iter().map(|(_, i, p, id)| (*i, *p, *id)).collect();
    assert_eq!(
        numbers,
        vec![
            (Some(99), None, Some(input_id)),
            (None, Some(100), Some(input_id))
        ]
    );
}

#[test]
#[serial(loom_config_env)]
fn worktree_resolves_to_canonical_repo_configuration() {
    isolate_env();
    let r = gitea_repo();
    git(
        r.path(),
        &[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "commit",
            "-q",
            "--allow-empty",
            "-m",
            "i",
        ],
    );
    let parent = tempdir().unwrap();
    let wt = parent.path().join("wt");
    git(r.path(), &["worktree", "add", "-q", wt.to_str().unwrap()]);
    // The worktree has no `.loom/config.json`; the canonical root's applies.
    assert!(!wt.join(".loom/config.json").exists());
    let res = resolve_forge_host(Some(&wt));
    assert_eq!(res.host(), Some("gitea.example.com"));
    // worktree_path wins over working_dir, as for commit correlation.
    let other = repo(Some("git@github.com:o/r.git"), None);
    let t = terminal(Some(other.path()), Some(&wt));
    assert_eq!(host_for_terminal(Some(&t)).as_deref(), Some("gitea.example.com"));
}

#[test]
#[serial(loom_config_env)]
fn two_terminal_contexts_do_not_cross_contaminate() {
    isolate_env();
    let gitea = gitea_repo();
    let github = repo(Some("https://github.com/o/r.git"), None);
    let other_gitea = repo(
        Some("https://ci@git.internal.test:3000/o/r.git"),
        Some(&json!({"forge": {"type": "gitea"}})),
    );
    let t1 = terminal(Some(gitea.path()), None);
    let t2 = terminal(Some(github.path()), None);
    let t3 = terminal(Some(other_gitea.path()), None);
    // Interleave twice so the second round is served from the cache.
    for _ in 0..2 {
        assert_eq!(host_for_terminal(Some(&t1)).as_deref(), Some("gitea.example.com"));
        assert_eq!(host_for_terminal(Some(&t2)).as_deref(), Some("github.com"));
        // No gitea.url: the origin host (userinfo stripped, port kept).
        assert_eq!(host_for_terminal(Some(&t3)).as_deref(), Some("git.internal.test:3000"));
    }
}

#[test]
#[serial(loom_config_env)]
fn missing_context_falls_back_to_github() {
    isolate_env();
    assert_eq!(resolve_forge_host(None), ForgeHostResolution::NoContext);
    assert_eq!(host_for_terminal(None).as_deref(), Some(DEFAULT_FORGE_HOST));
    assert_eq!(
        host_for_terminal(Some(&terminal(None, None))).as_deref(),
        Some(DEFAULT_FORGE_HOST)
    );
    // A workspace that is not a git repository resolves like GitHub too.
    let plain = tempdir().unwrap();
    assert_eq!(resolve_forge_host(Some(plain.path())).host(), Some(DEFAULT_FORGE_HOST));
}

#[test]
#[serial(loom_config_env)]
fn explicit_gitea_without_resolvable_host_is_unresolved_not_github() {
    isolate_env();
    for config in [
        json!({"forge": {"type": "gitea"}}),
        json!({"forge": {"type": "gitea", "gitea": {"url": "not-a-url"}}}),
    ] {
        let r = repo(None, Some(&config));
        let res = resolve_forge_host(Some(r.path()));
        let ForgeHostResolution::Unresolved { reason } = &res else {
            panic!("expected Unresolved, got {res:?}");
        };
        assert!(reason.contains("forge.gitea.url"), "{reason}");
        assert_eq!(res.host(), None);
        assert_eq!(res.warning(), Some(reason.as_str()));
    }
    // With no host, creation URLs are skipped; host-independent events still parse.
    let (rows, _) = ingest(None, &format!("{OUTPUT}Merged pull request #5\n"));
    let types: Vec<_> = rows.iter().map(|(t, ..)| t.as_str()).collect();
    assert_eq!(types, vec![PromptForgeEventType::PrMerged.as_str()]);
}

#[test]
#[serial(loom_config_env)]
fn malformed_config_tier_is_reported() {
    isolate_env();
    let r = repo(Some("git@github.com:o/r.git"), None);
    std::fs::create_dir_all(r.path().join(".loom")).unwrap();
    std::fs::write(r.path().join(".loom/config.json"), "{ not json").unwrap();
    let res = resolve_forge_host(Some(r.path()));
    assert_eq!(res.host(), Some("github.com"));
    let warning = res.warning().expect("unreadable tier must be diagnosable");
    assert!(warning.contains("config.json"), "{warning}");
}
