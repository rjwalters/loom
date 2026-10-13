#![allow(clippy::unwrap_used)]
use super::*;
use loom_daemon::telemetry::{TelemetryEnvelope, TelemetryRecord};
use std::os::unix::fs::PermissionsExt;

const ESTIMATE: &str = "<!-- loom:estimate lines=12 files=1 doctor_risk=0.1 -->";

/// A fake `gh` that logs every invocation and answers by argument shape:
/// label writes succeed, `…/comments` serves `comments.json`, anything else
/// serves `issue.json`.
struct Forge {
    dir: tempfile::TempDir,
}

impl Forge {
    fn new(issue: &serde_json::Value, comments: &serde_json::Value) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let gh = dir.path().join("fake-gh");
        std::fs::write(
            &gh,
            r#"#!/bin/sh
d=$(dirname "$0")
printf '%s\n' "$*" >> "$d/gh.log"
case "$*" in
  *"-X POST"*|*"-X DELETE"*) [ -e "$d/fail-writes" ] && exit 1; exit 0 ;;
  *"/comments"*) cat "$d/comments.json" ;;
  *) sleep 0.05; cat "$d/issue.json" ;;
esac
"#,
        )
        .unwrap();
        std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::write(dir.path().join("issue.json"), issue.to_string()).unwrap();
        std::fs::write(dir.path().join("comments.json"), comments.to_string()).unwrap();
        Self { dir }
    }

    fn args(&self, id: &str) -> CurationCompleteArgs {
        CurationCompleteArgs {
            repo: "fixture/repo".to_string(),
            issue: 42,
            model: "actual-curator-model".to_string(),
            runtime: "codex".to_string(),
            completion_id: id.to_string(),
            workspace_root: self.dir.path().to_path_buf(),
        }
    }

    fn run(&self, id: &str) -> Result<()> {
        let policy = TrustPolicy::default();
        self.args(id)
            .run_with(self.dir.path().join("fake-gh").as_os_str(), &policy, &|| Ok(()))
    }

    /// Run a fresh completion and assert it was refused with nothing written.
    fn assert_refused(&self) -> String {
        let err = self.run(&uuid::Uuid::new_v4().to_string()).unwrap_err();
        assert!(self.label_writes().is_empty(), "{}", self.log());
        assert!(self.journal().is_empty());
        format!("{err:#}")
    }

    fn log(&self) -> String {
        std::fs::read_to_string(self.dir.path().join("gh.log")).unwrap_or_default()
    }

    fn label_writes(&self) -> Vec<String> {
        self.log()
            .lines()
            .filter(|l| l.contains("-X POST") || l.contains("-X DELETE"))
            .map(str::to_string)
            .collect()
    }

    fn journal(&self) -> Vec<std::path::PathBuf> {
        std::fs::read_dir(self.dir.path().join(".loom/logs/issue-estimates"))
            .map(|d| d.flatten().map(|e| e.path()).collect())
            .unwrap_or_default()
    }
}

fn issue(body: &str, association: &str, labels: &[&str]) -> serde_json::Value {
    serde_json::json!({
        "body": body,
        "user": {"login": "filer", "type": "User"},
        "author_association": association,
        "labels": labels.iter().map(|l| serde_json::json!({"name": l})).collect::<Vec<_>>(),
    })
}

fn comment(body: &str, association: &str) -> serde_json::Value {
    serde_json::json!({
        "body": body,
        "user": {"login": "someone", "type": "User"},
        "author_association": association,
    })
}

/// repo#623: a tier marker posted in a trusted comment must not complete.
#[test]
fn repo_623_tier_marker_in_trusted_comment_is_refused() {
    let forge = Forge::new(
        &issue(ESTIMATE, "OWNER", &["loom:curating"]),
        &serde_json::json!([comment("<!-- loom:complexity=3 -->", "MEMBER")]),
    );
    let err = forge.run(&uuid::Uuid::new_v4().to_string()).unwrap_err();
    let message = format!("{err:#}");
    assert!(message.contains("loom:complexity"), "{message}");
    assert!(message.contains("loom:points="), "{message}");
    assert!(forge.label_writes().is_empty(), "{}", forge.log());
    assert!(forge.journal().is_empty());
}

#[test]
fn refuses_missing_bare_malformed_out_of_vocabulary_conflicting_and_mixed_markers() {
    for (comments, expect) in [
        (vec![], "no points estimate marker"),
        (vec!["3"], "no points estimate marker"),
        (vec!["<!-- loom:points=x -->"], "malformed"),
        (vec!["<!-- loom:points=21 -->"], "'21'"),
        (vec!["<!-- loom:points=3 -->\n<!-- loom:points=5 -->"], "conflicting"),
        (vec!["<!-- loom:points=3 -->\n<!-- loom:complexity=routine -->"], "retired"),
    ] {
        let listing: Vec<_> = comments.iter().map(|c| comment(c, "MEMBER")).collect();
        let forge = Forge::new(&issue(ESTIMATE, "OWNER", &[]), &serde_json::json!(listing));
        let message = forge.assert_refused();
        assert!(message.contains(expect), "{comments:?}: {message}");
    }
}

#[test]
fn invalid_newest_marker_never_falls_back_to_an_older_valid_one() {
    let forge = Forge::new(
        &issue(ESTIMATE, "OWNER", &[]),
        &serde_json::json!([
            comment("<!-- loom:points=3 -->", "MEMBER"),
            comment("<!-- loom:points=21 -->", "OWNER"),
        ]),
    );
    forge.assert_refused();
}

#[test]
fn trusted_comment_marker_projects_labels_and_journals_points() {
    let forge = Forge::new(
        &issue(ESTIMATE, "OWNER", &["loom:curating", "points:3"]),
        &serde_json::json!([comment("<!-- loom:points=5 -->", "MEMBER")]),
    );
    let id = uuid::Uuid::new_v4().to_string();
    let before = Utc::now();
    forge.run(&id).unwrap();
    let writes = forge.label_writes();
    assert_eq!(writes.len(), 2, "{writes:?}");
    assert!(writes[0].contains("-X POST") && writes[0].contains("labels[]=points:5"));
    assert!(writes[0].contains("labels[]=loom:curated"));
    assert!(writes[1].contains("-X DELETE") && writes[1].ends_with("labels/points%3A3 --silent"));
    let journal = forge.journal();
    assert_eq!(journal.len(), 1);
    let saved: TelemetryEnvelope =
        serde_json::from_slice(&std::fs::read(&journal[0]).unwrap()).unwrap();
    let TelemetryRecord::IssueEstimate(r) = saved.record else {
        panic!("wrong kind")
    };
    assert_eq!(r.points, Some(5));
    assert_eq!((r.repo.as_str(), r.issue), ("fixture/repo", 42));
    assert_eq!(r.model.as_deref(), Some("actual-curator-model"));
    assert_eq!(r.runtime.as_deref(), Some("codex"));
    assert_eq!(r.estimate.lines, 12);
    // Dated after the forge read completed.
    assert!(r.estimated_at >= before + chrono::Duration::milliseconds(40));
}

#[test]
fn untrusted_comment_marker_reads_as_absent() {
    for association in ["CONTRIBUTOR", "NONE"] {
        let forge = Forge::new(
            &issue(ESTIMATE, "OWNER", &[]),
            &serde_json::json!([comment("<!-- loom:points=5 -->", association)]),
        );
        assert!(forge.assert_refused().contains("no points estimate marker"));
    }
}

#[test]
fn comment_beats_body_and_body_counts_only_for_a_trusted_author() {
    let body = format!("{ESTIMATE}\n<!-- loom:points=2 -->");
    let forge = Forge::new(
        &issue(&body, "OWNER", &[]),
        &serde_json::json!([comment("<!-- loom:points=8 -->", "COLLABORATOR")]),
    );
    forge.run(&uuid::Uuid::new_v4().to_string()).unwrap();
    assert!(forge.label_writes()[0].contains("labels[]=points:8"));

    let forge = Forge::new(&issue(&body, "MEMBER", &[]), &serde_json::json!([]));
    forge.run(&uuid::Uuid::new_v4().to_string()).unwrap();
    assert!(forge.label_writes()[0].contains("labels[]=points:2"));

    let forge = Forge::new(&issue(&body, "NONE", &[]), &serde_json::json!([]));
    assert!(forge.assert_refused().contains("no points estimate marker"));
}

#[test]
fn retry_reapplies_journaled_labels_without_a_second_journal_file() {
    let forge = Forge::new(
        &issue(ESTIMATE, "OWNER", &["points:13"]),
        &serde_json::json!([comment("<!-- loom:points=3 -->", "MEMBER")]),
    );
    let id = uuid::Uuid::new_v4().to_string();
    // The journal lands, then the label write fails.
    std::fs::write(forge.dir.path().join("fail-writes"), b"").unwrap();
    assert!(forge.run(&id).is_err());
    let journal = forge.journal();
    assert_eq!(journal.len(), 1);
    let original = std::fs::read(&journal[0]).unwrap();
    // The marker changes, but a retry projects the journaled points.
    std::fs::write(
        forge.dir.path().join("comments.json"),
        serde_json::json!([comment("<!-- loom:points=8 -->", "MEMBER")]).to_string(),
    )
    .unwrap();
    std::fs::remove_file(forge.dir.path().join("fail-writes")).unwrap();
    forge.run(&id).unwrap();
    assert_eq!(forge.log().matches("/comments").count(), 1, "{}", forge.log());
    let writes = forge.label_writes();
    let last_post = writes.iter().rfind(|w| w.contains("-X POST")).unwrap();
    assert!(last_post.contains("labels[]=points:3"), "{writes:?}");
    assert!(writes
        .last()
        .unwrap()
        .ends_with("labels/points%3A13 --silent"));
    assert_eq!(forge.journal(), journal);
    assert_eq!(std::fs::read(&journal[0]).unwrap(), original);
}

#[test]
fn unwritable_repo_is_refused_before_any_read_or_write() {
    let forge = Forge::new(
        &issue(ESTIMATE, "OWNER", &[]),
        &serde_json::json!([comment("<!-- loom:points=3 -->", "MEMBER")]),
    );
    let gh = forge.dir.path().join("fake-gh");
    let err = forge
        .args(&uuid::Uuid::new_v4().to_string())
        .run_with(gh.as_os_str(), &TrustPolicy::default(), &|| Err("not managed".into()))
        .unwrap_err();
    assert!(format!("{err:#}").contains("not managed"));
    assert!(forge.log().is_empty());
    assert!(forge.journal().is_empty());
}
