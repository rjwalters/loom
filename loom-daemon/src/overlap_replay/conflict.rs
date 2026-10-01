//! Counterfactual textual-conflict replay (#9785 step 3): replay the
//! intended integration order on pinned commits with
//! `git merge-tree --write-tree`, both orders, on the declared common source.
//!
//! Label provenance is explicit: everything this module produces is
//! **counterfactual** (a replay we ran). Observed production conflicts are
//! recorded by the manifest ([`super::manifest::PrAssociation::
//! observed_conflict`]) and reported alongside — the two are never merged
//! into one label. Actual overlap and merge conflict are separate labels:
//! shared files may merge cleanly, and missing conflict evidence is *not* a
//! clean outcome (it is `Unknown`).

use serde::{Deserialize, Serialize};
use std::path::Path;
use std::process::Command;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ConflictReplay {
    /// The base the replay merged onto (the declared common source when the
    /// recorded topology permits it).
    pub base: String,
    /// Which side merged first: `"a_into_b"` = `merge-tree base b a` (a's
    /// changes applied last) … recorded verbatim per run.
    pub order: String,
    /// Always `counterfactual` for replays — kept in the record so a JSON
    /// consumer cannot confuse replay labels with observed ones.
    pub provenance: String,
    pub outcome: ConflictOutcome,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum ConflictOutcome {
    /// The merge completed without textual conflicts.
    Clean,
    /// Textual conflicts on the listed paths.
    TextualConflict { conflicted_files: Vec<String> },
    /// Could not answer (git too old for `merge-tree --write-tree`, commits
    /// missing, git failure). Missing evidence is not a clean outcome.
    Unknown { reason: String },
}

/// git >= 2.38, where `git merge-tree --write-tree` exists.
fn supports_merge_tree(repo: &Path) -> bool {
    let Ok(out) = Command::new("git")
        .arg("-C")
        .arg(repo)
        .arg("--version")
        .output()
    else {
        return false;
    };
    let version = String::from_utf8_lossy(&out.stdout)
        .split_whitespace()
        .nth(2)
        .map(str::to_string);
    version.as_deref().is_some_and(merge_tree_in_version)
}

fn merge_tree_in_version(version: &str) -> bool {
    let mut parts = version.split('.');
    let Some(Ok(major)) = parts.next().map(str::parse::<u32>) else {
        return false;
    };
    let minor = parts
        .next()
        .and_then(|m| m.parse::<u32>().ok())
        .unwrap_or(0);
    major > 2 || (major == 2 && minor >= 38)
}

/// One replay: merge `first` then `second` onto `base`.
fn replay_one(repo: &Path, base: &str, first: &str, second: &str, order: &str) -> ConflictReplay {
    let provenance = "counterfactual".to_string();
    if !supports_merge_tree(repo) {
        return ConflictReplay {
            base: base.into(),
            order: order.into(),
            provenance,
            outcome: ConflictOutcome::Unknown {
                reason: "git < 2.38: merge-tree --write-tree unavailable".into(),
            },
        };
    }
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args([
            "merge-tree",
            "--write-tree",
            &format!("--merge-base={base}"),
            first,
            second,
        ])
        .output();
    let Ok(out) = out else {
        return ConflictReplay {
            base: base.into(),
            order: order.into(),
            provenance,
            outcome: ConflictOutcome::Unknown {
                reason: "failed to execute git merge-tree".into(),
            },
        };
    };
    let stdout = String::from_utf8_lossy(&out.stdout);
    match out.status.code() {
        Some(0) => ConflictReplay {
            base: base.into(),
            order: order.into(),
            provenance,
            outcome: ConflictOutcome::Clean,
        },
        Some(1) => {
            // Output: <tree-oid> then conflicted-file sections
            // "<mode> <oid> <stage>\t<path>"; extract paths after tabs.
            let mut lines = stdout.lines();
            let _tree = lines.next();
            let mut conflicted_files: Vec<String> = Vec::new();
            for l in lines {
                if let Some((_, path)) = l.split_once('\t') {
                    if !path.is_empty() {
                        conflicted_files.push(path.to_string());
                    }
                }
            }
            conflicted_files.sort();
            conflicted_files.dedup();
            ConflictReplay {
                base: base.into(),
                order: order.into(),
                provenance,
                outcome: ConflictOutcome::TextualConflict { conflicted_files },
            }
        }
        code => ConflictReplay {
            base: base.into(),
            order: order.into(),
            provenance,
            outcome: ConflictOutcome::Unknown {
                reason: format!(
                    "git merge-tree exited {code:?}: {}",
                    String::from_utf8_lossy(&out.stderr).trim()
                ),
            },
        },
    }
}

/// Replay both orders of the pair onto `base`.
pub fn replay_pair(repo: &Path, base: &str, a: &str, b: &str) -> Vec<ConflictReplay> {
    vec![
        replay_one(repo, base, a, b, "b_then_a"),
        replay_one(repo, base, b, a, "a_then_b"),
    ]
}

/// True when any replay in `replays` produced a textual conflict.
pub fn any_textual_conflict(replays: &[ConflictReplay]) -> Option<bool> {
    let mut saw_unknown = false;
    for r in replays {
        match &r.outcome {
            ConflictOutcome::TextualConflict { .. } => return Some(true),
            ConflictOutcome::Unknown { .. } => saw_unknown = true,
            ConflictOutcome::Clean => {}
        }
    }
    if saw_unknown {
        None
    } else {
        Some(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_predicate() {
        assert!(merge_tree_in_version("2.38.0"));
        assert!(merge_tree_in_version("2.45.1"));
        assert!(merge_tree_in_version("3.0.0"));
        assert!(!merge_tree_in_version("2.37.3"));
        assert!(!merge_tree_in_version("1.9"));
    }

    #[test]
    fn any_conflict_classification() {
        let clean = ConflictReplay {
            base: "x".into(),
            order: "b_then_a".into(),
            provenance: "counterfactual".into(),
            outcome: ConflictOutcome::Clean,
        };
        let conflict = ConflictReplay {
            base: "x".into(),
            order: "a_then_b".into(),
            provenance: "counterfactual".into(),
            outcome: ConflictOutcome::TextualConflict {
                conflicted_files: vec!["f.rs".into()],
            },
        };
        let unknown = ConflictReplay {
            base: "x".into(),
            order: "a_then_b".into(),
            provenance: "counterfactual".into(),
            outcome: ConflictOutcome::Unknown {
                reason: "no git".into(),
            },
        };
        assert_eq!(any_textual_conflict(std::slice::from_ref(&clean)), Some(false));
        assert_eq!(any_textual_conflict(std::slice::from_ref(&conflict)), Some(true));
        assert_eq!(any_textual_conflict(std::slice::from_ref(&unknown)), None);
        assert_eq!(any_textual_conflict(&[clean, unknown]), None);
    }
}
