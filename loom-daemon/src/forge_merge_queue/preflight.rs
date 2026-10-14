//! Merge-queue capability preflight (#10255).
//!
//! Answers "could this repository's branch accept a merge-queue entry at all?"
//! with **distinct** failure kinds, because each needs a different human:
//!
//! | Kind | Meaning | Who fixes it |
//! |---|---|---|
//! | `UNSUPPORTED_FORGE` | Gitea (no merge queue) | nobody — use `direct` |
//! | `UNSUPPORTED_REPOSITORY` | archived, or user-owned | repo owner |
//! | `MISSING_QUEUE_RULE` | no active `merge_queue` rule on the branch | repo admin |
//! | `MISSING_REQUIRED_CHECKS` | queue rule present, no required status checks | repo admin |
//! | `CONFIG_INACCESSIBLE` | the settings/rules could not be read (403/404/no auth/timeout) | credential owner |
//! | `RATE_LIMITED` | the API budget is spent — no verdict at all | wait |
//!
//! **No kind falls back to a direct merge.** The preflight is read-only and
//! only ever reports.
//!
//! Rules come from the same effective-rules endpoint `forge merge-config`
//! reads (`GET repos/<nwo>/rules/branches/<branch>`, see
//! [`crate::forge_merge_config`]) and are decoded into its
//! [`BranchRule`] type, so both checks see the same rule set.
//!
//! Why "user-owned" is `UNSUPPORTED_REPOSITORY`: GitHub documents merge queues
//! for organization-owned repositories only (verified 2026-10-04 on #9978).
//! This matches `loom-daemon merge-group-ci eligibility`'s
//! `OWNER_NOT_ORGANIZATION` (#10257), so the two checks cannot disagree. That
//! command is the broader *pilot* gate: it adds push permission and the
//! workflow `merge_group` audit. This preflight answers only "can the forge
//! accept a queue entry here?", and it keeps rate limits and unreadable
//! settings apart, which a pilot gate does not need to do.

use std::fmt;
use std::time::Duration;

use serde::Deserialize;

use super::github::{classify_failure, safe_detail, FailureClass};
use crate::cmd_out::{decode_json, Query};
use crate::forge_merge_config::BranchRule;
use crate::gh_invocation::{AccessIntent, GhInvocation, GhTarget, Operation};

const PROBE_TIMEOUT: Duration = Duration::from_secs(20);

/// Why the queue cannot be used here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CapabilityError {
    UnsupportedForge { forge: String },
    UnsupportedRepository { reason: String },
    MissingQueueRule { branch: String },
    MissingRequiredChecks { branch: String },
    ConfigInaccessible { what: String, detail: String },
    RateLimited { detail: String },
}

impl CapabilityError {
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            CapabilityError::UnsupportedForge { .. } => "UNSUPPORTED_FORGE",
            CapabilityError::UnsupportedRepository { .. } => "UNSUPPORTED_REPOSITORY",
            CapabilityError::MissingQueueRule { .. } => "MISSING_QUEUE_RULE",
            CapabilityError::MissingRequiredChecks { .. } => "MISSING_REQUIRED_CHECKS",
            CapabilityError::ConfigInaccessible { .. } => "CONFIG_INACCESSIBLE",
            CapabilityError::RateLimited { .. } => "RATE_LIMITED",
        }
    }
}

impl fmt::Display for CapabilityError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let never = "Loom never edits rulesets; nothing falls back to a direct merge.";
        match self {
            CapabilityError::UnsupportedForge { forge } => write!(
                f,
                "{forge} has no merge queue. Keep champion.mergeMode=direct."
            ),
            CapabilityError::UnsupportedRepository { reason } => write!(
                f,
                "this repository cannot use a merge queue: {reason}. Keep champion.mergeMode=direct. {never}"
            ),
            CapabilityError::MissingQueueRule { branch } => write!(
                f,
                "no active ruleset on '{branch}' carries a merge_queue rule. A repository admin must \
                 add one before champion.mergeMode=queue can work. {never}"
            ),
            CapabilityError::MissingRequiredChecks { branch } => write!(
                f,
                "'{branch}' has a merge queue but no required status checks, so the queue would \
                 merge untested groups. A repository admin must require the merge-group checks. {never}"
            ),
            CapabilityError::ConfigInaccessible { what, detail } => write!(
                f,
                "could not read {what} ({detail}); the credential needs read access to the \
                 repository and its rules. This is not a capability verdict."
            ),
            CapabilityError::RateLimited { detail } => write!(
                f,
                "the forge API is rate-limited ({detail}); retry after the reset. This is not a \
                 capability verdict."
            ),
        }
    }
}

impl std::error::Error for CapabilityError {}

/// A branch that can take queue entries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Capability {
    pub branch: String,
    /// Rulesets carrying the `merge_queue` rule.
    pub queue_rulesets: Vec<u64>,
    /// Required status-check contexts, sorted and de-duplicated.
    pub required_checks: Vec<String>,
}

/// The slice of `GET repos/<nwo>` the preflight reads.
#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
pub struct RepoFacts {
    #[serde(default)]
    pub archived: bool,
    #[serde(default)]
    pub default_branch: Option<String>,
    #[serde(default)]
    pub owner: Option<Owner>,
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
pub struct Owner {
    #[serde(default, rename = "type")]
    pub kind: Option<String>,
}

/// The pure decision over already-fetched facts.
///
/// # Errors
///
/// The first blocking [`CapabilityError`], in table order.
pub(crate) fn evaluate_capability(
    branch: &str,
    repo: &RepoFacts,
    rules: &[BranchRule],
) -> Result<Capability, CapabilityError> {
    if repo.archived {
        return Err(CapabilityError::UnsupportedRepository {
            reason: "it is archived".to_string(),
        });
    }
    if repo
        .owner
        .as_ref()
        .and_then(|o| o.kind.as_deref())
        .is_some_and(|k| k == "User")
    {
        return Err(CapabilityError::UnsupportedRepository {
            reason: "it is owned by a user account, and GitHub offers merge queues only for \
                     organization-owned repositories"
                .to_string(),
        });
    }
    let mut queue_rulesets: Vec<u64> = rules
        .iter()
        .filter(|r| r.kind == "merge_queue")
        .map(|r| r.ruleset_id.unwrap_or(0))
        .collect();
    queue_rulesets.sort_unstable();
    queue_rulesets.dedup();
    if queue_rulesets.is_empty() {
        return Err(CapabilityError::MissingQueueRule {
            branch: branch.to_string(),
        });
    }
    let mut required_checks: Vec<String> = rules
        .iter()
        .filter(|r| r.kind == "required_status_checks")
        .filter_map(|r| r.parameters.as_ref())
        .filter_map(|p| p.get("required_status_checks"))
        .filter_map(serde_json::Value::as_array)
        .flatten()
        .filter_map(|c| c.get("context").and_then(serde_json::Value::as_str))
        .map(str::to_string)
        .collect();
    required_checks.sort();
    required_checks.dedup();
    if required_checks.is_empty() {
        return Err(CapabilityError::MissingRequiredChecks {
            branch: branch.to_string(),
        });
    }
    Ok(Capability {
        branch: branch.to_string(),
        queue_rulesets,
        required_checks,
    })
}

fn read<T: serde::de::DeserializeOwned>(
    gh: &str,
    nwo: &str,
    path: &str,
    what: &str,
) -> Result<T, CapabilityError> {
    let target = GhTarget::repo(nwo).unwrap_or(GhTarget::None);
    let out = GhInvocation::new(
        Operation::new("merge_queue.preflight"),
        AccessIntent::Read,
        target,
        PROBE_TIMEOUT,
    )
    .program(gh)
    .args(["api", path])
    .run();
    let inaccessible = |detail: String| {
        if classify_failure(&detail) == FailureClass::RateLimited {
            CapabilityError::RateLimited { detail }
        } else {
            CapabilityError::ConfigInaccessible {
                what: what.to_string(),
                detail,
            }
        }
    };
    match decode_json::<T, _>(out, |_| false) {
        Query::Populated(v) => Ok(v),
        Query::Empty => Err(inaccessible("empty response".to_string())),
        Query::Malformed { error, .. } => {
            Err(inaccessible(format!("unparseable response: {error}")))
        }
        Query::Failed { stderr, .. } => Err(inaccessible(safe_detail(&stderr))),
        Query::Unavailable(u) => Err(inaccessible(u.to_string())),
    }
}

/// The GitHub preflight: read the repository, then the effective rules on
/// `branch` (default: the repository's default branch), then decide.
///
/// # Errors
///
/// [`CapabilityError`].
pub fn github_preflight(
    gh: &str,
    nwo: &str,
    branch: Option<&str>,
) -> Result<Capability, CapabilityError> {
    let repo: RepoFacts = read(gh, nwo, &format!("repos/{nwo}"), "the repository settings")?;
    let branch = branch
        .map(str::to_string)
        .or_else(|| repo.default_branch.clone())
        .filter(|b| !b.is_empty())
        .ok_or_else(|| CapabilityError::ConfigInaccessible {
            what: "the default branch".to_string(),
            detail: "not visible to this credential; pass --branch".to_string(),
        })?;
    let rules: Vec<BranchRule> = read(
        gh,
        nwo,
        &format!("repos/{nwo}/rules/branches/{branch}?per_page=100"),
        &format!("the rules on '{branch}'"),
    )?;
    evaluate_capability(&branch, &repo, &rules)
}
