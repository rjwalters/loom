//! Merge-queue pilot eligibility: may this repository run Loom's queue mode?
//!
//! Read-only by construction. [`evaluate`] is a pure function over
//! [`RepoFacts`] plus the parsed workflows; [`probe`] gathers those facts with
//! `GET` reads only (`repos/{nwo}` and `repos/{nwo}/rules/branches/{branch}`),
//! through the counted [`GhInvocation`] facade with [`AccessIntent::Read`].
//! Nothing here enqueues, edits a ruleset, or changes branch protection —
//! enabling a queue is a protected-settings change that needs a named human
//! (#10257).
//!
//! Every prerequisite fails closed: a fact that could not be read is a named
//! failure (`*_UNKNOWN`), never a pass, so an unreadable repository can never
//! come out "eligible".

use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value as Json;

use super::audit::{self, AuditReport, Code};
use super::workflow::Workflow;
use crate::cmd_out::{decode_json, Query};
use crate::gh_invocation::{AccessIntent, GhInvocation, GhTarget, Operation};

const PROBE_TIMEOUT: Duration = Duration::from_secs(20);

/// One active rule on the branch (`GET repos/{nwo}/rules/branches/{branch}`).
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct Rule {
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ruleset_id: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parameters: Option<Json>,
}

/// Everything the verdict depends on, as read from the forge. `None` always
/// means "could not be determined", never "absent".
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
pub struct RepoFacts {
    #[serde(default)]
    pub repository: Option<String>,
    /// `owner.type`: `Organization` or `User`.
    #[serde(default)]
    pub owner_type: Option<String>,
    #[serde(default)]
    pub branch: Option<String>,
    /// Whether this credential can push (needed to enqueue). `None` when the
    /// `permissions` object was not visible.
    #[serde(default)]
    pub can_push: Option<bool>,
    /// The effective active rules on `branch`. `None` when unreadable.
    #[serde(default)]
    pub rules: Option<Vec<Rule>>,
}

/// A failed prerequisite.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Prereq {
    /// GitHub documents merge queues for organization-owned repositories.
    OwnerNotOrganization,
    OwnerTypeUnknown,
    PermissionUnknown,
    InsufficientPermission,
    MergeQueueUnknown,
    NoMergeQueueRule,
    NoRequiredChecks,
    MissingRequiredSuite,
    RequiredSuiteUncovered,
    WorkflowNotQualified,
}

impl Prereq {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Prereq::OwnerNotOrganization => "OWNER_NOT_ORGANIZATION",
            Prereq::OwnerTypeUnknown => "OWNER_TYPE_UNKNOWN",
            Prereq::PermissionUnknown => "PERMISSION_UNKNOWN",
            Prereq::InsufficientPermission => "INSUFFICIENT_PERMISSION",
            Prereq::MergeQueueUnknown => "MERGE_QUEUE_UNKNOWN",
            Prereq::NoMergeQueueRule => "NO_MERGE_QUEUE_RULE",
            Prereq::NoRequiredChecks => "NO_REQUIRED_CHECKS",
            Prereq::MissingRequiredSuite => "MISSING_REQUIRED_SUITE",
            Prereq::RequiredSuiteUncovered => "REQUIRED_SUITE_UNCOVERED",
            Prereq::WorkflowNotQualified => "WORKFLOW_NOT_QUALIFIED",
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Failure {
    pub prereq: Prereq,
    pub detail: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Eligibility {
    pub eligible: bool,
    pub facts: RepoFacts,
    /// Required status-check contexts from the branch's rules.
    pub required_checks: Vec<String>,
    /// The `merge_queue` rule's parameters, when one is active.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub merge_queue: Option<Json>,
    pub failures: Vec<Failure>,
    pub audit: AuditReport,
}

fn required_contexts(rules: &[Rule]) -> Vec<String> {
    let mut out: Vec<String> = rules
        .iter()
        .filter(|r| r.kind == "required_status_checks")
        .filter_map(|r| r.parameters.as_ref())
        .filter_map(|p| p.get("required_status_checks").and_then(Json::as_array))
        .flatten()
        .filter_map(|c| c.get("context").and_then(Json::as_str))
        .map(str::to_string)
        .collect();
    out.sort();
    out.dedup();
    out
}

/// Decide eligibility from facts alone. Pure: no I/O, no forge access.
#[must_use]
pub fn evaluate(
    facts: &RepoFacts,
    workflows: &[Workflow],
    unparseable: &[(String, String)],
) -> Eligibility {
    let mut failures = Vec::new();
    let mut fail = |prereq: Prereq, detail: String| failures.push(Failure { prereq, detail });

    match facts.owner_type.as_deref() {
        Some("Organization") => {}
        Some(other) => fail(
            Prereq::OwnerNotOrganization,
            format!("owner type is `{other}`; GitHub's merge queue is documented for organization-owned repositories, so this repository cannot pilot queue mode (transferring it needs operator authorization)"),
        ),
        None => fail(
            Prereq::OwnerTypeUnknown,
            "the repository owner's type could not be read".to_string(),
        ),
    }
    match facts.can_push {
        Some(true) => {}
        Some(false) => fail(
            Prereq::InsufficientPermission,
            "this credential cannot push to the repository, so it could not enqueue".to_string(),
        ),
        None => fail(
            Prereq::PermissionUnknown,
            "this credential's repository permissions are not visible; treated as not eligible"
                .to_string(),
        ),
    }
    let branch = facts.branch.as_deref().unwrap_or("the default branch");
    let (merge_queue, required) = match &facts.rules {
        None => {
            fail(
                Prereq::MergeQueueUnknown,
                format!("the active rules on {branch} could not be read; whether a merge queue is configured is unknown"),
            );
            (None, Vec::new())
        }
        Some(rules) => {
            let mq = rules.iter().find(|r| r.kind == "merge_queue");
            if mq.is_none() {
                fail(
                    Prereq::NoMergeQueueRule,
                    format!("no active `merge_queue` rule on {branch} (adding one is a ruleset change that needs operator authorization)"),
                );
            }
            let req = required_contexts(rules);
            if req.is_empty() {
                fail(
                    Prereq::NoRequiredChecks,
                    format!("no required status checks on {branch}; a queue would merge combined trees nothing validated"),
                );
            }
            (mq.map(|r| r.parameters.clone().unwrap_or(Json::Null)), req)
        }
    };

    let report = audit::audit(workflows, unparseable, &required);
    for f in &report.findings {
        let prereq = match f.code {
            Code::MissingRequiredSuite => Prereq::MissingRequiredSuite,
            Code::RequiredSuiteUncovered => Prereq::RequiredSuiteUncovered,
            _ => Prereq::WorkflowNotQualified,
        };
        fail(prereq, f.to_string());
    }

    Eligibility {
        eligible: failures.is_empty(),
        facts: facts.clone(),
        required_checks: required,
        merge_queue,
        failures,
        audit: report,
    }
}

#[derive(Debug, Deserialize)]
struct RepoView {
    #[serde(default)]
    owner: Option<Owner>,
    #[serde(default)]
    default_branch: Option<String>,
    #[serde(default)]
    permissions: Option<Perms>,
}

#[derive(Debug, Deserialize)]
struct Owner {
    #[serde(rename = "type", default)]
    kind: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Perms {
    #[serde(default)]
    admin: bool,
    #[serde(default)]
    maintain: bool,
    #[serde(default)]
    push: bool,
    #[serde(default)]
    pull: bool,
}

impl Perms {
    /// Whether this credential can push, when the object says anything.
    ///
    /// A GitHub App installation token (`ghs_…`) gets an all-`false`
    /// `permissions` object, `pull` included, even on a repository it just
    /// read. All-false therefore means "not reported", not "denied".
    fn can_push(&self) -> Option<bool> {
        let any = self.admin || self.maintain || self.push || self.pull;
        any.then_some(self.push || self.maintain || self.admin)
    }
}

/// One counted, read-only `GET` through the gh facade.
fn get(gh: &str, path: &str) -> crate::cmd_out::CmdOutcome {
    GhInvocation::new(
        Operation::new("merge_group_ci.read"),
        AccessIntent::Read,
        GhTarget::None,
        PROBE_TIMEOUT,
    )
    .program(gh)
    .args(["api", "--method", "GET", path])
    .run()
}

/// Gather [`RepoFacts`] with read-only API calls. Any read that fails leaves
/// its fact `None`, which [`evaluate`] reports as a named unknown.
#[must_use]
pub fn probe(gh: &str, nwo: &str, branch: Option<&str>) -> RepoFacts {
    let mut facts = RepoFacts {
        repository: Some(nwo.to_string()),
        ..RepoFacts::default()
    };
    let view = match decode_json::<RepoView, _>(get(gh, &format!("repos/{nwo}")), |_| false) {
        Query::Populated(v) => Some(v),
        _ => None,
    };
    if let Some(v) = &view {
        facts.owner_type = v.owner.as_ref().and_then(|o| o.kind.clone());
        facts.can_push = v.permissions.as_ref().and_then(Perms::can_push);
    }
    facts.branch = branch
        .map(str::to_string)
        .or_else(|| view.and_then(|v| v.default_branch))
        .filter(|b| !b.is_empty());
    if let Some(b) = &facts.branch {
        facts.rules = match decode_json::<Vec<Rule>, _>(
            get(gh, &format!("repos/{nwo}/rules/branches/{b}?per_page=100")),
            |_| false,
        ) {
            Query::Populated(r) => Some(r),
            // An empty body is not "no rules": fail closed to unknown.
            _ => None,
        };
    }
    facts
}

/// [`probe`] with the ambient `gh` binary, resolving the repository from the
/// current checkout's remote when `nwo` is not given.
///
/// # Errors
///
/// When no repository was given and none can be resolved here.
pub fn probe_ambient(nwo: Option<&str>, branch: Option<&str>) -> Result<RepoFacts, String> {
    let gh = crate::forge_cmd::gh_bin();
    let nwo = nwo
        .map(str::to_string)
        .or_else(|| crate::forge_cmd::repo_nwo(&gh))
        .ok_or_else(|| "no repository given and none resolvable here (pass --repo)".to_string())?;
    Ok(probe(&gh, &nwo, branch))
}
