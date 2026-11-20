use std::path::Path;
use std::time::Duration;

use serde::Deserialize;
use serde_json::Value;

use crate::forge_call_stats::{ops, ForgeOp};
use crate::gh_invocation::{AccessIntent, GhInvocation, GhTarget, Operation};

use super::tree::{is_sha, Tree, QUEUE_PROOF};
use super::{field, sha};

type Receive<'a> = dyn Fn(ForgeOp, &str) -> Option<Value> + 'a;

pub(super) struct Forge<'a> {
    pub repo: &'a str,
    pub get: Box<Receive<'a>>,
}

impl<'a> Forge<'a> {
    pub(super) fn github(root: &'a Path, repo: &'a str) -> Self {
        Self {
            repo,
            get: Box::new(move |operation, path| {
                let result = GhInvocation::new(
                    Operation::new("ci_docs_only.read"),
                    AccessIntent::Read,
                    GhTarget::None,
                    Duration::from_secs(10),
                )
                .forge_op(operation)
                .current_dir(root)
                .identity_scope(Some("github.com"), Some(repo))
                .reader_only()
                .args(["api", "--hostname", "github.com", "--method", "GET", path])
                .run();
                serde_json::from_slice(&result.ok_output()?.stdout).ok()
            }),
        }
    }

    fn read(&self, operation: ForgeOp, route: &str) -> Option<Value> {
        (self.get)(operation, &format!("repos/{}/{route}", self.repo))
    }

    pub(super) fn green_push(&self, workflow: u64, exact: Option<&str>) -> Option<String> {
        let head_query = exact.map_or_else(String::new, |sha| format!("&head_sha={sha}"));
        let runs = self.read(ops::CI_WORKFLOW_RUNS_FOR_SHA, &format!(
            "actions/workflows/{workflow}/runs?event=push&branch=main&status=success&per_page=1{head_query}"
        ))?;
        let [run] = runs.get("workflow_runs")?.as_array()?.as_slice() else {
            return None;
        };
        let head = sha(run, "/head_sha")?;
        (field(run, "/event") == Some("push")
            && field(run, "/head_branch") == Some("main")
            && field(run, "/status") == Some("completed")
            && field(run, "/conclusion") == Some("success")
            && run.get("workflow_id")?.as_u64()? == workflow
            && field(run, "/repository/full_name") == Some(self.repo)
            && field(run, "/head_repository/full_name") == Some(self.repo)
            && exact.is_none_or(|base| base == head))
        .then(|| head.to_string())
    }

    pub(super) fn queue_basis(
        &self,
        tree: &Tree<'_>,
        first: &str,
        workflow: u64,
    ) -> Option<String> {
        let branch = self.read(ops::GIT_READ_OBJECTS, "git/ref/heads/main")?;
        let protected = sha(&branch, "/object/sha")?;
        if field(&branch, "/ref") != Some("refs/heads/main")
            || field(&branch, "/object/type") != Some("commit")
            || !tree.first_parent_contains(protected, first)
        {
            return None;
        }

        // Read the checked-in attestation at the forge-confirmed protected tip,
        // never in the merge group. GitHub hides bypass_actors from the read-only
        // job, so the current ruleset revision must match the reviewed receipt.
        let proof: QueueProof = serde_json::from_slice(&tree.blob(protected, QUEUE_PROOF)?).ok()?;
        if proof.repository != self.repo
            || proof.workflow_id != workflow
            || !is_sha(&proof.green_seed)
            || !tree.ancestor(&proof.green_seed, protected)
        {
            return None;
        }

        let rules = self.read(ops::PROTECTION_READ_EFFECTIVE_RULES, "rules/branches/main")?;
        let detail = self.read(
            ops::PROTECTION_READ_EFFECTIVE_RULES,
            &format!("rulesets/{}", proof.ruleset_id),
        )?;
        if !proof.matches_policy(rules.as_array()?, &detail) {
            return None;
        }
        if first != protected {
            let refs = self.read(
                ops::GIT_READ_OBJECTS,
                "git/matching-refs/heads/gh-readonly-queue/main/pr-",
            )?;
            let predecessor = refs.as_array()?.iter().any(|entry| {
                field(entry, "/ref").is_some_and(is_queue_ref)
                    && field(entry, "/object/type") == Some("commit")
                    && sha(entry, "/object/sha") == Some(first)
            });
            if !predecessor {
                return None;
            }
        }
        Some(protected.to_string())
    }
}

/// The consuming repository records this from a privileged, attended read at
/// adoption time. The CI job itself uses ordinary read-only GitHub access.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct QueueProof {
    repository: String,
    workflow_id: u64,
    ruleset_id: u64,
    ruleset_updated_at: String,
    no_bypass: bool,
    green_seed: String,
    required_checks: Vec<Check>,
}

#[derive(Deserialize, Eq, PartialEq, Ord, PartialOrd)]
#[serde(deny_unknown_fields)]
struct Check {
    context: String,
    integration_id: Option<u64>,
}

impl QueueProof {
    fn matches_policy(&self, rules: &[Value], detail: &Value) -> bool {
        let same_revision = field(detail, "/source") == Some(&self.repository)
            && field(detail, "/source_type") == Some("Repository")
            && field(detail, "/target") == Some("branch")
            && field(detail, "/enforcement") == Some("active")
            && detail.get("id").and_then(Value::as_u64) == Some(self.ruleset_id)
            && field(detail, "/updated_at") == Some(&self.ruleset_updated_at)
            && chrono::DateTime::parse_from_rfc3339(&self.ruleset_updated_at).is_ok();
        let no_bypass = self.no_bypass
            && detail
                .get("bypass_actors")
                .is_none_or(|actors| actors.as_array().is_some_and(Vec::is_empty));
        if !same_revision || !no_bypass {
            return false;
        }
        let Some(queue) = self.effective_rule(rules, "merge_queue") else {
            return false;
        };
        let Some(checks) = self.effective_rule(rules, "required_status_checks") else {
            return false;
        };
        if field(queue, "/parameters/grouping_strategy") != Some("ALLGREEN")
            || field(queue, "/parameters/merge_method") != Some("MERGE")
            || checks
                .pointer("/parameters/do_not_enforce_on_create")
                .and_then(Value::as_bool)
                != Some(false)
        {
            return false;
        }
        let Some(raw_checks) = checks.pointer("/parameters/required_status_checks") else {
            return false;
        };
        let Ok(mut effective) = serde_json::from_value::<Vec<Check>>(raw_checks.clone()) else {
            return false;
        };
        effective.sort_unstable();
        let mut expected = self.required_checks.iter().collect::<Vec<_>>();
        expected.sort_unstable();
        // Require at least one first-party Actions suite in the audited set.
        // The consuming repo attests that the complete set covers its CI gate.
        effective
            .iter()
            .any(|check| check.integration_id == Some(15368))
            && effective
                .iter()
                .all(|check| !check.context.trim().is_empty())
            && effective
                .windows(2)
                .all(|pair| pair[0].context != pair[1].context)
            && effective.iter().eq(expected)
    }

    fn effective_rule<'a>(&self, rules: &'a [Value], kind: &str) -> Option<&'a Value> {
        let mut same_kind = rules
            .iter()
            .filter(|rule| field(rule, "/type") == Some(kind));
        let rule = same_kind.next()?;
        (same_kind.next().is_none()
            && rule.get("ruleset_id")?.as_u64()? == self.ruleset_id
            && field(rule, "/ruleset_source_type") == Some("Repository")
            && field(rule, "/ruleset_source") == Some(&self.repository))
        .then_some(rule)
    }
}

pub(super) fn is_queue_ref(reference: &str) -> bool {
    let Some(suffix) = reference.strip_prefix("refs/heads/gh-readonly-queue/main/pr-") else {
        return false;
    };
    suffix.split_once('-').is_some_and(|(number, sha)| {
        number.parse::<u64>().is_ok_and(|n| n > 0)
            && number.bytes().all(|c| c.is_ascii_digit())
            && is_sha(sha)
    })
}
