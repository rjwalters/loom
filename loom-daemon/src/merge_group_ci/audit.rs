//! The merge-group coverage audit over parsed workflows.
//!
//! For every relied-on suite (see [`relied_on_reason`]) the audit evaluates
//! each job and step under the `pull_request`, `push` and `merge_group` event
//! contexts and reports, per suite, why the combined merge-group tree would
//! NOT be validated by it. The rule throughout is `ci-principles.md` rule 6:
//! a suite that is skipped, or that the audit cannot prove runs, is reported
//! **uncovered**, never green.
//!
//! That rule is applied at **job** granularity. Steps are judged by parity
//! instead: a step is a finding only when it is *less* likely to run on
//! `merge_group` than on `pull_request` (`mg < pr`). A step whose `if:` is
//! undecidable on both events (e.g. it reads a prior step's outputs) is
//! not reported, because the merge-group run then validates exactly what the
//! PR run did.

use std::collections::BTreeMap;

use serde::Serialize;

use super::context::{Event, EventContext, JobScope, RunState, WorkflowScope, MG_HEAD_SHA, RUN_ID};
use super::expr::{self, interpolate, interpolated_paths, Truth, Value};
use super::workflow::{Concurrency, Job, Step, Workflow};

/// Why a relied-on suite does not validate the merge-group tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Code {
    /// A workflow file the audit could not read.
    UnparseableWorkflow,
    /// A relied-on workflow has no `merge_group` trigger (or its `types:`
    /// filter excludes `checks_requested`).
    MissingMergeGroupTrigger,
    /// A job's `if:` evaluates false on `merge_group` (e.g. reads
    /// `github.event_name == 'pull_request'` / `github.event.pull_request.*`).
    PrOnlyCondition,
    /// A job is skipped on `merge_group` because a job it `needs:` is skipped
    /// and its `if:` has no status function to run anyway.
    SkippedDependency,
    /// A job or step runs on `merge_group` only if a path-filter job's output
    /// says so — the suite can be skipped on the combined tree.
    PathFilterSkip,
    /// A job's or step's condition cannot be decided statically on
    /// `merge_group`; counted as uncovered.
    UndeterminedCondition,
    /// A step of a covered job runs on `pull_request` but is skipped on
    /// `merge_group`, so that gate silently does not run on the combined tree.
    PrOnlyStep,
    /// An `actions/checkout` step checks out something other than the
    /// merge-group commit (a PR head SHA, a fixed branch, an expression).
    NonMergeGroupCheckout,
    /// `cancel-in-progress` is (or may be) true on `merge_group`: a distinct
    /// commit's run could be cancelled.
    CancellingConcurrency,
    /// The concurrency group on `merge_group` is not unique to the merge-group
    /// commit, so a pending run can be superseded by another group's (or a
    /// PR's) run and the required check never reports.
    SharedConcurrencyGroup,
    /// A required status-check context matches no job in any workflow.
    MissingRequiredSuite,
    /// A required status-check context's job is not covered on `merge_group`.
    RequiredSuiteUncovered,
}

impl Code {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Code::UnparseableWorkflow => "UNPARSEABLE_WORKFLOW",
            Code::MissingMergeGroupTrigger => "MISSING_MERGE_GROUP_TRIGGER",
            Code::PrOnlyCondition => "PR_ONLY_CONDITION",
            Code::SkippedDependency => "SKIPPED_DEPENDENCY",
            Code::PathFilterSkip => "PATH_FILTER_SKIP",
            Code::UndeterminedCondition => "UNDETERMINED_CONDITION",
            Code::PrOnlyStep => "PR_ONLY_STEP",
            Code::NonMergeGroupCheckout => "NON_MERGE_GROUP_CHECKOUT",
            Code::CancellingConcurrency => "CANCELLING_CONCURRENCY",
            Code::SharedConcurrencyGroup => "SHARED_CONCURRENCY_GROUP",
            Code::MissingRequiredSuite => "MISSING_REQUIRED_SUITE",
            Code::RequiredSuiteUncovered => "REQUIRED_SUITE_UNCOVERED",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Finding {
    pub code: Code,
    pub workflow: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub job: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub step: Option<String>,
    pub line: usize,
    pub detail: String,
}

impl std::fmt::Display for Finding {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "[{}] {}:{}", self.code.as_str(), self.workflow, self.line)?;
        if let Some(j) = &self.job {
            write!(f, " job `{j}`")?;
        }
        if let Some(s) = &self.step {
            write!(f, " step `{s}`")?;
        }
        write!(f, " — {}", self.detail)
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct WorkflowSummary {
    pub file: String,
    pub name: String,
    pub relied_on: bool,
    pub reason: String,
    pub merge_group_trigger: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub permissions: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct JobReport {
    pub workflow: String,
    pub id: String,
    pub names: Vec<String>,
    /// How the job resolves under each event context.
    pub states: BTreeMap<Event, RunState>,
    pub relied_on: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pr_only_marker: Option<String>,
    /// Relied-on, runs on `merge_group`, and no finding against it.
    pub covered: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub permissions: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct RequiredCoverage {
    pub context: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub job: Option<String>,
    pub covered: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct AuditReport {
    pub workflows: Vec<WorkflowSummary>,
    pub jobs: Vec<JobReport>,
    pub required: Vec<RequiredCoverage>,
    pub findings: Vec<Finding>,
}

impl AuditReport {
    /// Every relied-on suite and required context validates the merge group.
    #[must_use]
    pub fn qualified(&self) -> bool {
        self.findings.is_empty()
    }
}

/// Whether (and why) a workflow is relied on to gate merges. A workflow is
/// relied on when it runs on every pull request (a `pull_request` /
/// `pull_request_target` trigger with no path filter) or hosts a required
/// context. A path-filtered PR workflow cannot be a required gate — a skipped
/// required check never reports — so it is advisory, not relied on.
#[must_use]
pub fn relied_on_reason(wf: &Workflow, required: &[String]) -> (bool, String) {
    let hosts_required: Vec<&String> = required
        .iter()
        .filter(|c| wf.jobs.iter().any(|j| j.names.contains(c)))
        .collect();
    if !hosts_required.is_empty() {
        return (
            true,
            format!(
                "hosts required context(s): {}",
                hosts_required
                    .iter()
                    .map(|s| s.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        );
    }
    let pr = wf
        .trigger("pull_request")
        .or_else(|| wf.trigger("pull_request_target"));
    match pr {
        Some(t) if !t.path_filtered => (true, "runs on every pull request".to_string()),
        Some(_) => (
            false,
            "advisory: path-filtered pull_request trigger (cannot be a required gate)".to_string(),
        ),
        None => (false, "not a pull-request gate".to_string()),
    }
}

/// Whether the workflow is triggered by `event` (for `merge_group`, with a
/// `types:` filter that admits `checks_requested`).
fn triggered(wf: &Workflow, event: Event) -> bool {
    match event {
        Event::PullRequest => {
            wf.trigger("pull_request").is_some() || wf.trigger("pull_request_target").is_some()
        }
        Event::Push => wf.trigger("push").is_some(),
        Event::MergeGroup => wf.trigger("merge_group").is_some_and(|t| {
            t.types
                .as_ref()
                .is_none_or(|ts| ts.iter().any(|x| x == "checks_requested"))
        }),
    }
}

fn truth_state(t: Truth) -> RunState {
    match t {
        Truth::True => RunState::Runs,
        Truth::False => RunState::Skipped,
        Truth::Unknown => RunState::Unknown,
    }
}

/// Evaluate a job- or step-level condition, with GitHub's implicit
/// `success() &&` when no status function appears.
fn eval_condition(cond: Option<&str>, scope: &JobScope<'_>) -> Value {
    let Some(src) = cond else {
        return scope_success(scope);
    };
    let Ok(e) = expr::parse_condition(src) else {
        return Value::Unknown;
    };
    let v = e.eval(scope);
    if e.has_status_function() {
        return v;
    }
    let s = scope_success(scope);
    match (s.truth(), v.truth()) {
        (Truth::False, _) | (_, Truth::False) => Value::Bool(false),
        (Truth::True, _) => v,
        _ => Value::Unknown,
    }
}

fn scope_success(scope: &JobScope<'_>) -> Value {
    use super::expr::Context;
    scope.success()
}

/// Resolve every job's run state under one event context.
fn job_states(wf: &Workflow, ctx: &EventContext) -> BTreeMap<String, RunState> {
    let mut states: BTreeMap<String, RunState> = BTreeMap::new();
    if !triggered(wf, ctx.event) {
        for j in &wf.jobs {
            states.insert(j.id.clone(), RunState::Skipped);
        }
        return states;
    }
    // Fixed point over `needs:` (jobs may be declared in any order).
    for _ in 0..=wf.jobs.len() {
        let mut progressed = false;
        for j in &wf.jobs {
            if states.contains_key(&j.id) {
                continue;
            }
            let Some(needs) = j
                .needs
                .iter()
                .map(|n| {
                    if wf.job(n).is_none() {
                        Some((n.clone(), RunState::Unknown))
                    } else {
                        states.get(n).map(|s| (n.clone(), *s))
                    }
                })
                .collect::<Option<Vec<_>>>()
            else {
                continue;
            };
            let scope = JobScope {
                event: ctx,
                needs,
                job_level: true,
            };
            let state = truth_state(eval_condition(j.if_cond.as_deref(), &scope).truth());
            states.insert(j.id.clone(), state);
            progressed = true;
        }
        if !progressed {
            break;
        }
    }
    for j in &wf.jobs {
        states.entry(j.id.clone()).or_insert(RunState::Unknown);
    }
    states
}

fn needs_of(job: &Job, states: &BTreeMap<String, RunState>) -> Vec<(String, RunState)> {
    job.needs
        .iter()
        .map(|n| (n.clone(), states.get(n).copied().unwrap_or(RunState::Unknown)))
        .collect()
}

/// The path-filter jobs whose outputs a condition reads, among jobs that may
/// run in this context.
fn path_filter_deps(cond: &str, wf: &Workflow, states: &BTreeMap<String, RunState>) -> Vec<String> {
    let mut paths = Vec::new();
    if let Ok(e) = expr::parse_condition(cond) {
        e.paths(&mut paths);
    } else {
        paths = interpolated_paths(cond);
    }
    let mut out: Vec<String> = paths
        .iter()
        .filter_map(|p| {
            let mut segs = p.split('.');
            match (segs.next(), segs.next(), segs.next()) {
                (Some("needs"), Some(job), Some("outputs")) => Some(job.to_string()),
                _ => None,
            }
        })
        .filter(|job| {
            wf.job(job).is_some_and(Job::uses_paths_filter)
                && states.get(job).copied() != Some(RunState::Skipped)
        })
        .collect();
    out.sort();
    out.dedup();
    out
}

struct Ctx3 {
    pr: EventContext,
    push: EventContext,
    mg: EventContext,
}

/// Audit a set of workflows. `required` is the list of required status-check
/// contexts (may be empty); `unparseable` carries files that failed to parse.
#[must_use]
pub fn audit(
    workflows: &[Workflow],
    unparseable: &[(String, String)],
    required: &[String],
) -> AuditReport {
    let mut findings: Vec<Finding> = unparseable
        .iter()
        .map(|(file, why)| Finding {
            code: Code::UnparseableWorkflow,
            workflow: file.clone(),
            job: None,
            step: None,
            line: 0,
            detail: format!(
                "could not read the workflow ({why}); its suites cannot be counted as covered"
            ),
        })
        .collect();
    let mut summaries = Vec::new();
    let mut jobs_out = Vec::new();

    for wf in workflows {
        let (relied, reason) = relied_on_reason(wf, required);
        let mg_trigger = triggered(wf, Event::MergeGroup);
        summaries.push(WorkflowSummary {
            file: wf.file.clone(),
            name: wf.name.clone(),
            relied_on: relied,
            reason,
            merge_group_trigger: mg_trigger,
            permissions: wf.permissions.clone(),
        });
        let ctx = Ctx3 {
            pr: EventContext::new(Event::PullRequest, &wf.name),
            push: EventContext::new(Event::Push, &wf.name),
            mg: EventContext::new(Event::MergeGroup, &wf.name),
        };
        let pr_states = job_states(wf, &ctx.pr);
        let push_states = job_states(wf, &ctx.push);
        let mg_states = job_states(wf, &ctx.mg);

        let mut wf_findings = Vec::new();
        if relied && !mg_trigger {
            let line = wf.trigger("pull_request").map_or(1, |t| t.line);
            wf_findings.push(Finding {
                code: Code::MissingMergeGroupTrigger,
                workflow: wf.file.clone(),
                job: None,
                step: None,
                line,
                detail: "relied-on workflow has no `merge_group: types: [checks_requested]` trigger, so none of its suites run on the combined tree".to_string(),
            });
        }
        if relied && mg_trigger {
            if let Some(c) = &wf.concurrency {
                check_concurrency(c, wf, None, &WorkflowScope(&ctx.mg), &mut wf_findings);
            }
        }

        for job in &wf.jobs {
            let hosts_required = job.names.iter().any(|n| required.contains(n));
            let pr_state = pr_states[&job.id];
            let job_relied = relied
                && (hosts_required || (job.pr_only.is_none() && pr_state != RunState::Skipped));
            let mut job_findings = Vec::new();
            if job_relied && mg_trigger {
                audit_job(job, wf, &ctx, &pr_states, &mg_states, &mut job_findings);
            }
            let covered = job_relied
                && mg_trigger
                && mg_states[&job.id] == RunState::Runs
                && job_findings.is_empty();
            let mut states = BTreeMap::new();
            states.insert(Event::PullRequest, pr_state);
            states.insert(Event::Push, push_states[&job.id]);
            states.insert(Event::MergeGroup, mg_states[&job.id]);
            jobs_out.push(JobReport {
                workflow: wf.file.clone(),
                id: job.id.clone(),
                names: job.names.clone(),
                states,
                relied_on: job_relied,
                pr_only_marker: job.pr_only.clone(),
                covered,
                permissions: job.permissions.clone(),
            });
            wf_findings.extend(job_findings);
        }
        findings.extend(wf_findings);
    }

    let mut required_out = Vec::new();
    for ctx_name in required {
        let hit = jobs_out
            .iter()
            .find(|j| j.names.iter().any(|n| n == ctx_name));
        match hit {
            None => findings.push(Finding {
                code: Code::MissingRequiredSuite,
                workflow: "-".to_string(),
                job: None,
                step: None,
                line: 0,
                detail: format!(
                    "required context `{ctx_name}` matches no job name in any workflow, so the merge group can never satisfy it"
                ),
            }),
            Some(j) if !j.covered => {
                let line = workflows
                    .iter()
                    .find(|w| w.file == j.workflow)
                    .and_then(|w| w.job(&j.id))
                    .map_or(0, |jb| jb.line);
                findings.push(Finding {
                    code: Code::RequiredSuiteUncovered,
                    workflow: j.workflow.clone(),
                    job: Some(j.id.clone()),
                    step: None,
                    line,
                    detail: format!(
                        "required context `{ctx_name}` does not validate the merge-group commit (see this job's other findings)"
                    ),
                });
            }
            Some(_) => {}
        }
        required_out.push(RequiredCoverage {
            context: ctx_name.clone(),
            job: hit.map(|j| format!("{}#{}", j.workflow, j.id)),
            covered: hit.is_some_and(|j| j.covered),
        });
    }

    AuditReport {
        workflows: summaries,
        jobs: jobs_out,
        required: required_out,
        findings,
    }
}

fn audit_job(
    job: &Job,
    wf: &Workflow,
    ctx: &Ctx3,
    pr_states: &BTreeMap<String, RunState>,
    mg_states: &BTreeMap<String, RunState>,
    out: &mut Vec<Finding>,
) {
    let finding = |code: Code, line: usize, step: Option<&Step>, detail: String| Finding {
        code,
        workflow: wf.file.clone(),
        job: Some(job.id.clone()),
        step: step.map(Step::label),
        line,
        detail,
    };
    let cond_text = job.if_cond.as_deref().unwrap_or("success()");
    match mg_states[&job.id] {
        RunState::Runs => {}
        RunState::Skipped => {
            let skipped_need = job
                .needs
                .iter()
                .find(|n| mg_states.get(*n) == Some(&RunState::Skipped));
            let has_status = job
                .if_cond
                .as_deref()
                .and_then(|c| expr::parse_condition(c).ok())
                .is_some_and(|e| e.has_status_function());
            if let (Some(need), false) = (skipped_need, has_status) {
                out.push(finding(
                    Code::SkippedDependency,
                    job.line,
                    None,
                    format!("skipped on merge_group because `needs: {need}` is skipped and `if: {cond_text}` has no status function (use `!cancelled() && ...`)"),
                ));
            } else {
                out.push(finding(
                    Code::PrOnlyCondition,
                    job.line,
                    None,
                    format!("`if: {cond_text}` evaluates false on merge_group, so this suite is skipped on the combined tree"),
                ));
            }
            return;
        }
        RunState::Unknown => {
            let deps = path_filter_deps(cond_text, wf, mg_states);
            if deps.is_empty() {
                out.push(finding(
                    Code::UndeterminedCondition,
                    job.line,
                    None,
                    format!("`if: {cond_text}` cannot be decided on merge_group; an unproven suite is not coverage"),
                ));
            } else {
                out.push(finding(
                    Code::PathFilterSkip,
                    job.line,
                    None,
                    format!("runs on merge_group only if path-filter job(s) {} say so (`if: {cond_text}`)", deps.join(", ")),
                ));
            }
            return;
        }
    }

    let pr_scope = JobScope {
        event: &ctx.pr,
        needs: needs_of(job, pr_states),
        job_level: false,
    };
    let mg_scope = JobScope {
        event: &ctx.mg,
        needs: needs_of(job, mg_states),
        job_level: false,
    };
    for step in &job.steps {
        let pr = eval_condition(step.if_cond.as_deref(), &pr_scope).truth();
        let mg = eval_condition(step.if_cond.as_deref(), &mg_scope).truth();
        let cond = step.if_cond.as_deref().unwrap_or("success()");
        if mg < pr {
            let deps = path_filter_deps(cond, wf, mg_states);
            let (code, detail) = match (mg, deps.is_empty()) {
                (Truth::False, _) => (
                    Code::PrOnlyStep,
                    format!("`if: {cond}` runs on pull_request but is skipped on merge_group"),
                ),
                (_, false) => (
                    Code::PathFilterSkip,
                    format!(
                        "runs on merge_group only if path-filter job(s) {} say so",
                        deps.join(", ")
                    ),
                ),
                _ => (
                    Code::UndeterminedCondition,
                    format!("`if: {cond}` cannot be decided on merge_group"),
                ),
            };
            out.push(finding(code, step.line, Some(step), detail));
        }
        if step.is_checkout() && mg != Truth::False {
            check_checkout(step, &mg_scope, |d| {
                out.push(finding(Code::NonMergeGroupCheckout, step.line, Some(step), d));
            });
        }
    }
    if let Some(c) = &job.concurrency {
        let mut tmp = Vec::new();
        check_concurrency(c, wf, Some(&job.id), &mg_scope, &mut tmp);
        out.extend(tmp);
    }
}

/// The checkout must land on the merge-group commit: no `ref:` (the default
/// is `github.sha`, the merge-group head), or a `ref:` that evaluates to it.
fn check_checkout(step: &Step, scope: &JobScope<'_>, mut report: impl FnMut(String)) {
    if let Some(repo) = step.with_value("repository") {
        let v = interpolate(repo, scope);
        if v != Value::Str("owner/repo".into()) {
            // A checkout of another repository is not the tree under test.
            return;
        }
    }
    let Some(r) = step.with_value("ref") else {
        return;
    };
    match interpolate(r, scope) {
        // Empty resolves to the default — the merge-group commit.
        Value::Null => {}
        Value::Str(s) if s.is_empty() || s.contains(MG_HEAD_SHA) => {}
        Value::Str(s) => report(format!(
            "`ref: {r}` checks out `{s}` on merge_group instead of the merge-group commit"
        )),
        _ => report(format!(
            "`ref: {r}` cannot be resolved on merge_group; the tested tree is unproven"
        )),
    }
}

fn check_concurrency(
    c: &Concurrency,
    wf: &Workflow,
    job: Option<&str>,
    scope: &dyn expr::Context,
    out: &mut Vec<Finding>,
) {
    let finding = |code: Code, detail: String| Finding {
        code,
        workflow: wf.file.clone(),
        job: job.map(str::to_string),
        step: None,
        line: c.line,
        detail,
    };
    let cancel = c
        .cancel_in_progress
        .as_deref()
        .map_or(Value::Bool(false), |s| match s.trim() {
            // A YAML boolean literal, not the (truthy) string "false".
            "true" | "True" | "TRUE" => Value::Bool(true),
            "false" | "False" | "FALSE" => Value::Bool(false),
            _ => interpolate(s, scope),
        });
    match cancel.truth() {
        Truth::False => {}
        Truth::True => out.push(finding(
            Code::CancellingConcurrency,
            "cancel-in-progress is true on merge_group: a started merge-group run could be cancelled (ci-principles.md rule 2)".to_string(),
        )),
        Truth::Unknown => out.push(finding(
            Code::CancellingConcurrency,
            format!(
                "cancel-in-progress `{}` cannot be proven false on merge_group",
                c.cancel_in_progress.as_deref().unwrap_or_default()
            ),
        )),
    }
    match interpolate(&c.group, scope) {
        Value::Str(g) if g.contains(MG_HEAD_SHA) || g.contains(RUN_ID) => {}
        Value::Str(g) => out.push(finding(
            Code::SharedConcurrencyGroup,
            format!("group `{}` resolves to `{g}` on merge_group, which is not unique to the merge-group commit", c.group),
        )),
        _ => out.push(finding(
            Code::SharedConcurrencyGroup,
            format!("group `{}` cannot be resolved on merge_group", c.group),
        )),
    }
}
