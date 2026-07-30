//! Builder checkpoint management — the native port of `loom_tools.checkpoints`
//! (#4275), behind `checkpoint.sh` / the historical `loom-checkpoint` CLI.
//!
//! Checkpoints let a recovering orchestrator detect partial progress when a
//! builder fails, so it can resume instead of always retrying from scratch.
//!
//! Checkpoint file: `<worktree>/.loom-checkpoint` (gitignored — the pattern is
//! written by `init/post_init.rs` and must survive this port).
//!
//! Stages, in order of progression: `planning`, `implementing`, `tested`,
//! `committed`, `pushed`, `pr_created`.

use std::path::{Path, PathBuf};

use serde_json::{json, Map, Value};

/// Valid checkpoint stages, in order of progression.
pub const CHECKPOINT_STAGES: [&str; 6] = [
    "planning",
    "implementing",
    "tested",
    "committed",
    "pushed",
    "pr_created",
];

/// Recovery path per checkpoint stage.
pub const RECOVERY_PATHS: [(&str, &str); 6] = [
    ("planning", "retry_from_scratch"), // No useful work done
    ("implementing", "check_changes"),  // May have useful changes
    ("tested", "route_to_commit"),      // Tests ran, route based on result
    ("committed", "push_and_pr"),       // Just needs push and PR
    ("pushed", "create_pr"),            // Just needs PR creation
    ("pr_created", "verify_labels"),    // Just needs label check
];

pub const CHECKPOINT_FILENAME: &str = ".loom-checkpoint";

/// Optional details about the checkpoint stage.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct CheckpointDetails {
    pub files_changed: i64,
    pub test_command: String,
    pub test_result: String,
    pub test_output_summary: String,
    pub commit_sha: String,
    pub pr_number: Option<i64>,
}

impl CheckpointDetails {
    /// Only non-empty fields are serialized — matching the Python `to_dict`.
    #[must_use]
    pub fn to_value(&self) -> Value {
        let mut d = Map::new();
        if self.files_changed != 0 {
            d.insert("files_changed".into(), json!(self.files_changed));
        }
        if !self.test_command.is_empty() {
            d.insert("test_command".into(), json!(self.test_command));
        }
        if !self.test_result.is_empty() {
            d.insert("test_result".into(), json!(self.test_result));
        }
        if !self.test_output_summary.is_empty() {
            d.insert("test_output_summary".into(), json!(self.test_output_summary));
        }
        if !self.commit_sha.is_empty() {
            d.insert("commit_sha".into(), json!(self.commit_sha));
        }
        if let Some(pr) = self.pr_number {
            d.insert("pr_number".into(), json!(pr));
        }
        Value::Object(d)
    }

    #[must_use]
    pub fn from_value(data: &Value) -> Self {
        let s = |k: &str| {
            data.get(k)
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string()
        };
        Self {
            files_changed: data
                .get("files_changed")
                .and_then(Value::as_i64)
                .unwrap_or(0),
            test_command: s("test_command"),
            test_result: s("test_result"),
            test_output_summary: s("test_output_summary"),
            commit_sha: s("commit_sha"),
            pr_number: data.get("pr_number").and_then(Value::as_i64),
        }
    }
}

/// A builder checkpoint representing progress at a specific stage.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Checkpoint {
    pub stage: String,
    pub timestamp: String,
    pub issue: i64,
    pub details: CheckpointDetails,
}

impl Checkpoint {
    #[must_use]
    pub fn to_value(&self) -> Value {
        let mut d = Map::new();
        d.insert("stage".into(), json!(self.stage));
        d.insert("timestamp".into(), json!(self.timestamp));
        if self.issue != 0 {
            d.insert("issue".into(), json!(self.issue));
        }
        let details = self.details.to_value();
        if !details.as_object().is_none_or(serde_json::Map::is_empty) {
            d.insert("details".into(), details);
        }
        Value::Object(d)
    }

    #[must_use]
    pub fn from_value(data: &Value) -> Self {
        Self {
            stage: data
                .get("stage")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            timestamp: data
                .get("timestamp")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            issue: data.get("issue").and_then(Value::as_i64).unwrap_or(0),
            details: CheckpointDetails::from_value(data.get("details").unwrap_or(&Value::Null)),
        }
    }

    /// The recommended recovery path for this checkpoint's stage.
    #[must_use]
    pub fn recovery_path(&self) -> &'static str {
        RECOVERY_PATHS
            .iter()
            .find(|(stage, _)| *stage == self.stage)
            .map_or("retry_from_scratch", |(_, path)| *path)
    }

    /// 0-based index of this stage in the progression, or `-1` if unknown.
    #[must_use]
    pub fn stage_index(&self) -> i32 {
        CHECKPOINT_STAGES
            .iter()
            .position(|s| *s == self.stage)
            .map_or(-1, |i| i32::try_from(i).unwrap_or(-1))
    }

    /// Whether this checkpoint is strictly after `other_stage`.
    #[must_use]
    pub fn is_after(&self, other_stage: &str) -> bool {
        CHECKPOINT_STAGES
            .iter()
            .position(|s| *s == other_stage)
            .is_some_and(|other| self.stage_index() > i32::try_from(other).unwrap_or(i32::MAX))
    }
}

#[must_use]
pub fn checkpoint_path(worktree_path: &Path) -> PathBuf {
    worktree_path.join(CHECKPOINT_FILENAME)
}

/// Read a checkpoint from a worktree. `None` when absent or invalid (a
/// checkpoint must at minimum carry a non-empty `stage`).
#[must_use]
pub fn read_checkpoint(worktree_path: &Path) -> Option<Checkpoint> {
    let path = checkpoint_path(worktree_path);
    if !path.is_file() {
        return None;
    }
    let data = super::read_json_file(&path)?;
    if !data.is_object() {
        return None;
    }
    let stage = data
        .get("stage")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if stage.is_empty() {
        return None;
    }
    Some(Checkpoint::from_value(&data))
}

/// Write a checkpoint to a worktree. Returns `false` (after a diagnostic,
/// unless `quiet`) for an invalid stage, a missing worktree, or an I/O error.
pub fn write_checkpoint(
    worktree_path: &Path,
    stage: &str,
    issue: i64,
    details: CheckpointDetails,
    quiet: bool,
) -> bool {
    if !CHECKPOINT_STAGES.contains(&stage) {
        if !quiet {
            super::log_error(&format!("Invalid checkpoint stage '{stage}'"));
            super::log_error(&format!("Valid stages: {}", CHECKPOINT_STAGES.join(", ")));
        }
        return false;
    }
    if !worktree_path.is_dir() {
        if !quiet {
            super::log_error(&format!(
                "Worktree directory does not exist: {}",
                worktree_path.display()
            ));
        }
        return false;
    }

    let checkpoint = Checkpoint {
        stage: stage.to_string(),
        timestamp: chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string(),
        issue,
        details,
    };

    match super::write_json_file(&checkpoint_path(worktree_path), &checkpoint.to_value()) {
        Ok(()) => {
            if !quiet {
                super::log_success(&format!("Checkpoint saved: stage={stage}"));
            }
            true
        }
        Err(e) => {
            if !quiet {
                super::log_error(&format!("Failed to write checkpoint: {e}"));
            }
            false
        }
    }
}

/// Remove the checkpoint file. `true` when it was removed or never existed.
pub fn clear_checkpoint(worktree_path: &Path, quiet: bool) -> bool {
    let path = checkpoint_path(worktree_path);
    if !path.is_file() {
        return true;
    }
    match std::fs::remove_file(&path) {
        Ok(()) => {
            if !quiet {
                super::log_info("Checkpoint cleared");
            }
            true
        }
        Err(e) => {
            if !quiet {
                super::log_error(&format!("Failed to clear checkpoint: {e}"));
            }
            false
        }
    }
}

/// Recovery recommendation derived from a checkpoint (or the absence of one).
#[must_use]
pub fn recovery_recommendation(checkpoint: Option<&Checkpoint>) -> Value {
    let Some(cp) = checkpoint else {
        return json!({
            "recovery_path": "retry_from_scratch",
            "skip_stages": Vec::<String>::new(),
            "details": "No checkpoint found",
        });
    };

    let stage_index = cp.stage_index();
    let skip_stages: Vec<&str> = if stage_index >= 0 {
        CHECKPOINT_STAGES[..(stage_index as usize + 1)].to_vec()
    } else {
        Vec::new()
    };

    let mut parts = vec![format!("Checkpoint at stage '{}'", cp.stage)];
    if !cp.details.test_result.is_empty() {
        parts.push(format!("test_result={}", cp.details.test_result));
    }
    if cp.details.files_changed != 0 {
        parts.push(format!("files_changed={}", cp.details.files_changed));
    }
    if let Some(pr) = cp.details.pr_number.filter(|p| *p != 0) {
        parts.push(format!("pr_number={pr}"));
    }

    json!({
        "recovery_path": cp.recovery_path(),
        "skip_stages": skip_stages,
        "details": parts.join(", "),
        "checkpoint": cp.to_value(),
    })
}

/// The `stages` subcommand payload.
#[must_use]
pub fn stages_value() -> Value {
    let mut recovery = Map::new();
    for (stage, path) in RECOVERY_PATHS {
        recovery.insert(stage.to_string(), json!(path));
    }
    json!({"stages": CHECKPOINT_STAGES, "recovery_paths": recovery})
}

/// Human-readable `stages` listing.
#[must_use]
pub fn stages_text() -> String {
    let mut lines = vec!["Valid checkpoint stages (in order of progression):".to_string()];
    for (stage, recovery) in RECOVERY_PATHS {
        lines.push(format!("  {stage:15} -> recovery: {recovery}"));
    }
    lines.join("\n")
}

/// Human-readable `read` output for an existing checkpoint.
#[must_use]
pub fn read_text(cp: &Checkpoint) -> String {
    let mut lines = vec![format!(
        "Checkpoint: stage={}, timestamp={}",
        cp.stage, cp.timestamp
    )];
    if cp.issue != 0 {
        lines.push(format!("  Issue: #{}", cp.issue));
    }
    if !cp.details.test_result.is_empty() {
        lines.push(format!("  Test result: {}", cp.details.test_result));
    }
    if cp.details.files_changed != 0 {
        lines.push(format!("  Files changed: {}", cp.details.files_changed));
    }
    if let Some(pr) = cp.details.pr_number.filter(|p| *p != 0) {
        lines.push(format!("  PR number: #{pr}"));
    }
    lines.push(format!("  Recovery path: {}", cp.recovery_path()));
    lines.join("\n")
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn write_then_read_round_trips() {
        let dir = tempdir().unwrap();
        assert!(write_checkpoint(
            dir.path(),
            "implementing",
            42,
            CheckpointDetails::default(),
            true
        ));
        let cp = read_checkpoint(dir.path()).unwrap();
        assert_eq!(cp.stage, "implementing");
        assert_eq!(cp.issue, 42);
        assert!(cp.timestamp.ends_with('Z'));
        assert_eq!(cp.recovery_path(), "check_changes");
    }

    #[test]
    fn write_rejects_invalid_stage_and_missing_worktree() {
        let dir = tempdir().unwrap();
        assert!(!write_checkpoint(dir.path(), "bogus", 1, CheckpointDetails::default(), true));
        assert!(read_checkpoint(dir.path()).is_none());
        let missing = dir.path().join("nope");
        assert!(!write_checkpoint(&missing, "planning", 1, CheckpointDetails::default(), true));
    }

    #[test]
    fn details_round_trip_through_json() {
        let dir = tempdir().unwrap();
        let details = CheckpointDetails {
            files_changed: 7,
            test_command: "cargo test".into(),
            test_result: "pass".into(),
            test_output_summary: "all green".into(),
            commit_sha: "abc123".into(),
            pr_number: Some(99),
        };
        assert!(write_checkpoint(dir.path(), "tested", 42, details.clone(), true));
        let cp = read_checkpoint(dir.path()).unwrap();
        assert_eq!(cp.details, details);
    }

    /// The Python `to_dict` omits empty fields entirely; the on-disk shape is
    /// part of the contract for anything reading `.loom-checkpoint`.
    #[test]
    fn empty_details_are_omitted_from_disk() {
        let dir = tempdir().unwrap();
        write_checkpoint(dir.path(), "planning", 0, CheckpointDetails::default(), true);
        let raw: Value =
            serde_json::from_str(&std::fs::read_to_string(checkpoint_path(dir.path())).unwrap())
                .unwrap();
        assert!(raw.get("details").is_none());
        assert!(raw.get("issue").is_none(), "issue 0 is omitted");
        assert!(raw.get("stage").is_some());
        assert!(raw.get("timestamp").is_some());
    }

    #[test]
    fn read_rejects_missing_or_stageless_checkpoints() {
        let dir = tempdir().unwrap();
        assert!(read_checkpoint(dir.path()).is_none());
        std::fs::write(checkpoint_path(dir.path()), r#"{"timestamp": "x"}"#).unwrap();
        assert!(read_checkpoint(dir.path()).is_none());
        std::fs::write(checkpoint_path(dir.path()), r#"{"stage": ""}"#).unwrap();
        assert!(read_checkpoint(dir.path()).is_none());
        std::fs::write(checkpoint_path(dir.path()), "not json").unwrap();
        assert!(read_checkpoint(dir.path()).is_none());
        std::fs::write(checkpoint_path(dir.path()), "[1,2]").unwrap();
        assert!(read_checkpoint(dir.path()).is_none());
    }

    #[test]
    fn clear_is_idempotent() {
        let dir = tempdir().unwrap();
        assert!(clear_checkpoint(dir.path(), true));
        write_checkpoint(dir.path(), "pushed", 1, CheckpointDetails::default(), true);
        assert!(clear_checkpoint(dir.path(), true));
        assert!(read_checkpoint(dir.path()).is_none());
        assert!(clear_checkpoint(dir.path(), true));
    }

    #[test]
    fn stage_progression_and_recovery_paths() {
        for (stage, expected) in RECOVERY_PATHS {
            let cp = Checkpoint {
                stage: stage.to_string(),
                timestamp: String::new(),
                issue: 0,
                details: CheckpointDetails::default(),
            };
            assert_eq!(cp.recovery_path(), expected);
        }
        let committed = Checkpoint {
            stage: "committed".into(),
            timestamp: String::new(),
            issue: 0,
            details: CheckpointDetails::default(),
        };
        assert!(committed.is_after("tested"));
        assert!(!committed.is_after("pushed"));
        assert!(!committed.is_after("nonexistent-stage"));
        assert_eq!(committed.stage_index(), 3);

        let unknown = Checkpoint {
            stage: "mystery".into(),
            timestamp: String::new(),
            issue: 0,
            details: CheckpointDetails::default(),
        };
        assert_eq!(unknown.stage_index(), -1);
        assert_eq!(unknown.recovery_path(), "retry_from_scratch");
    }

    #[test]
    fn recommendation_without_a_checkpoint() {
        let rec = recovery_recommendation(None);
        assert_eq!(rec["recovery_path"], json!("retry_from_scratch"));
        assert_eq!(rec["details"], json!("No checkpoint found"));
        assert_eq!(rec["skip_stages"], json!([]));
    }

    #[test]
    fn recommendation_lists_skippable_stages() {
        let cp = Checkpoint {
            stage: "committed".into(),
            timestamp: "2026-01-01T00:00:00Z".into(),
            issue: 42,
            details: CheckpointDetails {
                files_changed: 3,
                test_result: "pass".into(),
                pr_number: Some(7),
                ..CheckpointDetails::default()
            },
        };
        let rec = recovery_recommendation(Some(&cp));
        assert_eq!(rec["recovery_path"], json!("push_and_pr"));
        assert_eq!(rec["skip_stages"], json!(["planning", "implementing", "tested", "committed"]));
        let details = rec["details"].as_str().unwrap();
        assert!(details.contains("Checkpoint at stage 'committed'"));
        assert!(details.contains("test_result=pass"));
        assert!(details.contains("files_changed=3"));
        assert!(details.contains("pr_number=7"));
    }

    #[test]
    fn stages_payloads() {
        let v = stages_value();
        assert_eq!(v["stages"], json!(CHECKPOINT_STAGES));
        assert_eq!(v["recovery_paths"]["pr_created"], json!("verify_labels"));
        assert!(stages_text().contains("pr_created      -> recovery: verify_labels"));
    }

    #[test]
    fn read_text_renders_optional_fields() {
        let cp = Checkpoint {
            stage: "tested".into(),
            timestamp: "2026-01-01T00:00:00Z".into(),
            issue: 42,
            details: CheckpointDetails {
                files_changed: 2,
                test_result: "fail".into(),
                pr_number: Some(5),
                ..CheckpointDetails::default()
            },
        };
        let text = read_text(&cp);
        assert!(text.contains("Checkpoint: stage=tested, timestamp=2026-01-01T00:00:00Z"));
        assert!(text.contains("  Issue: #42"));
        assert!(text.contains("  Test result: fail"));
        assert!(text.contains("  Files changed: 2"));
        assert!(text.contains("  PR number: #5"));
        assert!(text.contains("  Recovery path: route_to_commit"));
    }
}
