//! `loom-daemon forge merge-config` (#9287) — advisory, read-only check that
//! `merge-pr.sh` can actually merge on a repository's default branch.
//!
//! `merge-pr.sh`'s ability to merge depends on forge-side configuration that
//! lives outside the repo tree: the repository's `allow_*` merge flags AND every
//! active branch ruleset's `pull_request.allowed_merge_methods` /
//! `required_linear_history`. On 2026-09-27/28 this repo's own `main` ruleset
//! allowed only `squash` while the repository allowed only merge commits — the
//! intersection was empty, every `merge-pr.sh` call returned `405 Merge commits
//! are not allowed`, and nothing in install or resync looked (#9287).
//!
//! This subcommand computes
//!
//! ```text
//! effective = {methods the repo allows}
//!             ∩ {allowed_merge_methods of every ACTIVE ruleset on the branch}
//! ```
//!
//! and reports three findings:
//!
//! 1. `EMPTY_EFFECTIVE_SET` — no permitted method exists.
//! 2. `LINEAR_HISTORY_REJECTS_MERGE` — `required_linear_history` is active and
//!    `merge` is the only member of `effective`.
//! 3. `METHOD_NOT_PERMITTED` — the method `merge-pr.sh` will use (its own
//!    auto-detect, [`resolve_merge_method`], or `--method`) is not usable:
//!    excluded from `effective`, or a merge commit under linear history.
//!
//! `required_linear_history` alongside a squash/rebase method `merge-pr.sh`
//! will actually use is correct and deliberate, and is NOT a finding. A branch
//! with no active ruleset produces no output at all.
//!
//! Rules are read from `GET repos/<nwo>/rules/branches/<branch>`, GitHub's
//! *effective rules* endpoint: it returns every ACTIVE rule that applies to the
//! branch — repository- and organization-level, with `include`/`exclude`
//! ref-name conditions already resolved, `evaluate`/`disabled` rulesets already
//! dropped — each tagged with its `ruleset_id`, so several rulesets intersect
//! (AND) instead of the first one winning. That is strictly more faithful than
//! re-deriving targeting from `repos/<nwo>/rulesets` conditions by hand (the
//! enumeration `setup-branch-protection.sh` uses for overlap detection, which
//! only recognizes three include spellings and ignores `exclude`), and it needs
//! read access rather than admin.
//!
//! Contract: **always exits 0** and **never writes** anything to the forge — a
//! ruleset is a deliberate protection artifact and its remedy needs a named
//! human. A probe that cannot be answered (403, no auth, timeout, settings not
//! visible to this credential) prints "could not determine", never a finding.
//! GitHub only; Gitea has no rulesets API and is skipped.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use anyhow::Result;
use serde::Deserialize;

use crate::cmd_out::{decode_json, Query};
use crate::forge_cmd::{detect_forge, gh_bin, repo_nwo, ForgeType};
use crate::forge_merge_method::{resolve_merge_method, RepoMergeFlags};
use crate::gh_invocation::{AccessIntent, GhInvocation, GhTarget, Operation};

/// Per-probe deadline. Shorter than `FORGE_CMD_TIMEOUT`: this runs inline in
/// an install and on every resync, and an unanswered probe is only ever
/// "could not determine".
const PROBE_TIMEOUT: Duration = Duration::from_secs(20);

/// Prefix on every line this subcommand prints, so a caller that indents the
/// output (resync, install) still shows where it came from.
const PREFIX: &str = "merge-config:";

type MethodSet = BTreeSet<&'static str>;

/// The slice of `GET repos/<nwo>` this check reads. The `allow_*` flags are
/// `Option` on purpose: a credential that cannot see them must yield "could
/// not determine", not an all-false set that would read as case 1.
#[derive(Debug, Default, Deserialize)]
struct RepoSettings {
    allow_merge_commit: Option<bool>,
    allow_squash_merge: Option<bool>,
    allow_rebase_merge: Option<bool>,
    #[serde(default)]
    default_branch: Option<String>,
    #[serde(default)]
    permissions: Option<Permissions>,
}

#[derive(Debug, Default, Deserialize)]
struct Permissions {
    #[serde(default)]
    admin: bool,
}

/// One entry of `GET repos/<nwo>/rules/branches/<branch>`.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct BranchRule {
    #[serde(rename = "type")]
    pub(crate) kind: String,
    #[serde(default)]
    pub(crate) ruleset_id: Option<u64>,
    #[serde(default)]
    pub(crate) parameters: Option<serde_json::Value>,
}

/// One entry of `GET repos/<nwo>/rulesets` — used only to put names on ids.
#[derive(Debug, Deserialize)]
struct RulesetSummary {
    id: u64,
    #[serde(default)]
    name: String,
}

/// Normalize a forge method name onto the static vocabulary.
fn method_name(raw: &str) -> Option<&'static str> {
    match raw {
        "merge" => Some("merge"),
        "squash" => Some("squash"),
        "rebase" => Some("rebase"),
        _ => None,
    }
}

fn repo_allowed(flags: RepoMergeFlags) -> MethodSet {
    let mut set = MethodSet::new();
    if flags.allow_merge_commit {
        set.insert("merge");
    }
    if flags.allow_squash_merge {
        set.insert("squash");
    }
    if flags.allow_rebase_merge {
        set.insert("rebase");
    }
    set
}

fn fmt_set(set: &MethodSet) -> String {
    if set.is_empty() {
        "none".to_string()
    } else {
        set.iter().copied().collect::<Vec<_>>().join(", ")
    }
}

/// What the rules on one branch say about merging, folded per ruleset.
#[derive(Debug, Default)]
pub(crate) struct RuleConstraints {
    /// ruleset id -> the methods its `pull_request` rule allows. A ruleset
    /// whose `pull_request` rule names no `allowed_merge_methods` does not
    /// constrain the method, so it is absent here.
    pub(crate) allowed_by_ruleset: BTreeMap<u64, MethodSet>,
    /// ids of rulesets carrying `required_linear_history`.
    pub(crate) linear_rulesets: BTreeSet<u64>,
}

pub(crate) fn fold_rules(rules: &[BranchRule]) -> RuleConstraints {
    let mut out = RuleConstraints::default();
    for rule in rules {
        let id = rule.ruleset_id.unwrap_or(0);
        match rule.kind.as_str() {
            "required_linear_history" => {
                out.linear_rulesets.insert(id);
            }
            "pull_request" => {
                let Some(methods) = rule
                    .parameters
                    .as_ref()
                    .and_then(|p| p.get("allowed_merge_methods"))
                    .and_then(serde_json::Value::as_array)
                else {
                    continue;
                };
                let set: MethodSet = methods
                    .iter()
                    .filter_map(serde_json::Value::as_str)
                    .filter_map(method_name)
                    .collect();
                // Two pull_request rules in ONE ruleset still AND together.
                out.allowed_by_ruleset
                    .entry(id)
                    .and_modify(|cur| *cur = cur.intersection(&set).copied().collect())
                    .or_insert(set);
            }
            _ => {}
        }
    }
    out
}

/// The outcome of [`evaluate`]: the effective set and zero or more findings.
#[derive(Debug)]
pub(crate) struct Evaluation {
    pub(crate) effective: MethodSet,
    pub(crate) findings: Vec<String>,
}

/// The pure decision. `method` is the method `merge-pr.sh` will send;
/// `names` labels ruleset ids in messages (best-effort, may be empty).
pub(crate) fn evaluate(
    branch: &str,
    flags: RepoMergeFlags,
    constraints: &RuleConstraints,
    method: &str,
    names: &BTreeMap<u64, String>,
) -> Evaluation {
    let label = |id: &u64| match names.get(id) {
        Some(n) if !n.is_empty() => format!("ruleset {id} ('{n}')"),
        _ => format!("ruleset {id}"),
    };
    let repo = repo_allowed(flags);
    let mut effective = repo.clone();
    for set in constraints.allowed_by_ruleset.values() {
        effective = effective.intersection(set).copied().collect();
    }
    let rulesets_desc = constraints
        .allowed_by_ruleset
        .iter()
        .map(|(id, set)| format!("{} allows {}", label(id), fmt_set(set)))
        .collect::<Vec<_>>()
        .join("; ");
    let linear_desc = constraints
        .linear_rulesets
        .iter()
        .map(label)
        .collect::<Vec<_>>()
        .join(", ");
    let linear = !constraints.linear_rulesets.is_empty();
    let never =
        "Loom never edits rulesets or repository settings; a repo admin must change one of them.";

    let mut findings = Vec::new();
    if effective.is_empty() {
        findings.push(format!(
            "[EMPTY_EFFECTIVE_SET] no merge method is permitted on '{branch}': the repository allows {}; {}. \
             Every merge-pr.sh merge will fail (HTTP 405) for any identity that cannot bypass the ruleset. \
             Fix: make the ruleset's allowed merge methods overlap the repository's merge settings. {never}",
            fmt_set(&repo),
            if rulesets_desc.is_empty() { "no ruleset narrows it".to_string() } else { rulesets_desc.clone() },
        ));
    } else if linear && effective.len() == 1 && effective.contains("merge") {
        findings.push(format!(
            "[LINEAR_HISTORY_REJECTS_MERGE] the only permitted merge method on '{branch}' is 'merge', but {linear_desc} \
             requires linear history, which rejects every merge commit. Every merge-pr.sh merge will fail. \
             Fix: drop required_linear_history, or permit squash/rebase in both the ruleset and the repository settings. {never}"
        ));
    } else {
        // A method the branch will actually take: in `effective`, and not a
        // merge commit under linear history. Preferred in merge-pr.sh's own
        // auto-detect order (merge > rebase > squash, #9105).
        let takes = |m: &str| effective.contains(m) && !(linear && m == "merge");
        let alt = ["merge", "rebase", "squash"]
            .into_iter()
            .find(|m| takes(m))
            .unwrap_or("squash");
        if !effective.contains(method) {
            findings.push(format!(
                "[METHOD_NOT_PERMITTED] merge-pr.sh will merge '{branch}' with '{method}', but the effective merge methods are {} \
                 (repository allows {}; {}). Its merges will fail. Fix: align the repository's merge settings with the ruleset, \
                 or pass `merge-pr.sh --merge-method {alt}`. {never}",
                fmt_set(&effective),
                fmt_set(&repo),
                if rulesets_desc.is_empty() { "no ruleset narrows it".to_string() } else { rulesets_desc.clone() },
            ));
        } else if !takes(method) {
            findings.push(format!(
                "[METHOD_NOT_PERMITTED] merge-pr.sh will merge '{branch}' with 'merge', but {linear_desc} requires linear history, \
                 which rejects merge commits. Its merges will fail. Fix: pass `merge-pr.sh --merge-method {alt}`, or disable \
                 allow_merge_commit so its auto-detect picks '{alt}'. {never}"
            ));
        }
    }
    Evaluation {
        effective,
        findings,
    }
}

/// First line of a `gh` error, for a one-line "could not determine".
fn first_line(text: &str) -> String {
    text.lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("no error text")
        .to_string()
}

/// One counted (#10089) `gh api <path>` read, run to a classified outcome.
fn gh_api(gh: &str, path: &str) -> crate::cmd_out::CmdOutcome {
    GhInvocation::new(
        Operation::new("merge_config.api"),
        AccessIntent::Read,
        GhTarget::None,
        PROBE_TIMEOUT,
    )
    // Merge settings may be hidden from a reader App (W4-C writer_only).
    .writer_identity()
    .program(gh)
    .args(["api", path])
    .run()
}

fn query_reason<T>(q: Query<T>) -> Result<T, String> {
    match q {
        Query::Populated(v) => Ok(v),
        Query::Empty => Err("empty response".to_string()),
        Query::Malformed { error, .. } => Err(format!("unparseable response: {error}")),
        Query::Failed { stderr, .. } => Err(first_line(&stderr)),
        Query::Unavailable(u) => Err(u.to_string()),
    }
}

/// GitHub backend. Returns the lines to print (possibly none); never fails.
pub(crate) fn github_merge_config(
    gh: &str,
    nwo: &str,
    branch: Option<&str>,
    method: Option<&str>,
    verbose: bool,
) -> Vec<String> {
    let undetermined = |why: &str| {
        vec![format!("{PREFIX} could not determine the merge configuration of {nwo}: {why} (not a finding; nothing was changed)")]
    };

    let settings = match query_reason(decode_json::<RepoSettings, _>(
        gh_api(gh, &format!("repos/{nwo}")),
        |_| false,
    )) {
        Ok(s) => s,
        Err(why) => return undetermined(&format!("reading repository settings failed ({why})")),
    };
    let (Some(allow_merge), Some(allow_squash), Some(allow_rebase)) = (
        settings.allow_merge_commit,
        settings.allow_squash_merge,
        settings.allow_rebase_merge,
    ) else {
        return undetermined(
            "the repository's allow_* merge settings are not visible to this credential",
        );
    };
    let flags = RepoMergeFlags {
        allow_squash_merge: allow_squash,
        allow_merge_commit: allow_merge,
        allow_rebase_merge: allow_rebase,
    };
    let admin = settings.permissions.as_ref().is_some_and(|p| p.admin);
    let Some(branch) = branch
        .map(str::to_string)
        .or(settings.default_branch.clone())
        .filter(|b| !b.is_empty())
    else {
        return undetermined("the default branch is unknown (pass --branch)");
    };

    let rules = match query_reason(decode_json::<Vec<BranchRule>, _>(
        gh_api(gh, &format!("repos/{nwo}/rules/branches/{branch}?per_page=100")),
        |_| false,
    )) {
        Ok(r) => r,
        Err(why) => {
            let hint = if admin {
                ""
            } else {
                "; this credential is not a repository admin"
            };
            return undetermined(&format!("reading the rules on '{branch}' failed ({why}{hint})"));
        }
    };
    let constraints = fold_rules(&rules);

    // Auto-detect exactly as merge-pr.sh does, unless a method was requested.
    let method = match method {
        Some(m) => m.to_string(),
        None => resolve_merge_method(None, flags).unwrap_or_else(|_| "merge".to_string()),
    };

    // Names are cosmetic: fetch them only when there is something to name.
    let mut names = BTreeMap::new();
    if !constraints.allowed_by_ruleset.is_empty() || !constraints.linear_rulesets.is_empty() {
        if let Query::Populated(list) = decode_json::<Vec<RulesetSummary>, _>(
            gh_api(gh, &format!("repos/{nwo}/rulesets")),
            |_| false,
        ) {
            names = list.into_iter().map(|r| (r.id, r.name)).collect();
        }
    }

    let eval = evaluate(&branch, flags, &constraints, &method, &names);
    let mut out: Vec<String> = eval
        .findings
        .iter()
        .map(|f| format!("{PREFIX} WARNING {f}"))
        .collect();
    if out.is_empty() && verbose {
        let scope = if constraints.allowed_by_ruleset.is_empty()
            && constraints.linear_rulesets.is_empty()
        {
            "no active ruleset constrains merging"
        } else {
            "active rulesets checked"
        };
        out.push(format!(
            "{PREFIX} OK — {nwo} '{branch}': effective merge methods {} ({scope}); merge-pr.sh will use '{method}'.",
            fmt_set(&eval.effective)
        ));
    }
    out
}

/// Handle `loom-daemon forge merge-config`. Prints its findings and ALWAYS
/// exits 0 — an advisory never blocks an install, upgrade, or resync.
pub fn handle_merge_config(
    repo: Option<&str>,
    branch: Option<&str>,
    method: Option<&str>,
    verbose: bool,
) -> Result<()> {
    let lines = match detect_forge(None) {
        ForgeType::Gitea => {
            if verbose {
                println!("{PREFIX} skipped — Gitea has no branch-rulesets API to check.");
            }
            Vec::new()
        }
        ForgeType::GitHub => {
            let gh = gh_bin();
            match repo.map(str::to_string).or_else(|| repo_nwo(&gh)) {
                Some(nwo) => github_merge_config(&gh, &nwo, branch, method, verbose),
                None => {
                    if verbose {
                        println!("{PREFIX} skipped — no forge repository could be resolved here.");
                    }
                    Vec::new()
                }
            }
        }
    };
    for line in lines {
        println!("{line}");
    }
    std::process::exit(0);
}

#[cfg(test)]
#[path = "forge_merge_config_tests.rs"]
mod tests;
