//! Main-push cancellation lint (#10670): no cancel path reachable from a
//! `push` (or `merge_group`) run may cancel a run that has already started.
//!
//! The policy is `ci-principles.md` rule 2: on the default branch every commit
//! is distinct work, so a started run is never cancelled. The one permitted
//! bound is GitHub's native concurrency group with `cancel-in-progress: false`
//! — at most one run in progress plus the newest pending, a newer push
//! superseding only a run that has not started. In operator terms: keep the
//! oldest (running) and the newest, drop only the queued ones in between.
//!
//! That native bound is the *only* cancellation mechanism allowed to reach a
//! main run (rule 4, one mechanism per behaviour). This lint fails closed on
//! the three ways a workflow could break it, each evaluated with the same
//! `push` / `merge_group` event contexts the merge-group audit uses:
//!
//! 1. A workflow- or job-level `concurrency:` whose `cancel-in-progress` is
//!    (or cannot be proven not to be) true on that event.
//! 2. A concurrency group the event *shares* with a `pull_request` run whose
//!    own `cancel-in-progress` may be true: GitHub applies the setting of the
//!    *arriving* run, so a PR run landing in main's group would cancel main's
//!    started run even though main's own setting is false.
//! 3. A step that cancels runs (`gh run cancel`, the REST `.../cancel` /
//!    `force-cancel` endpoints, or a `uses:` cancel action) that is reachable
//!    on that event — i.e. neither its job nor its own `if:` can be proven
//!    false there. This is the #7779 shape: a "cancel older runs for this
//!    ref" step whose ref fell back to `main` on a push and cancelled every
//!    in-progress main run.
//!
//! What it deliberately does not police: PR runs. Superseding a PR run is
//! correct (rule 2), so `cancel-in-progress` on `pull_request` and the PR-only
//! cancel steps are left alone.

use super::audit::{eval_condition, job_states, needs_of, triggered};
use super::context::{Event, EventContext, JobScope, RunState, WorkflowScope};
use super::expr::{self, interpolate, Truth, Value};
use super::workflow::{Concurrency, Step, Workflow};

/// Events whose started runs must never be cancelled.
pub const PROTECTED_EVENTS: [Event; 2] = [Event::Push, Event::MergeGroup];

/// One way a protected run could be cancelled after it started.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct CancelFinding {
    pub workflow: String,
    pub job: Option<String>,
    pub step: Option<String>,
    pub line: usize,
    pub event: Event,
    pub detail: String,
}

impl std::fmt::Display for CancelFinding {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{} [{}]", self.workflow, self.line, self.event.name())?;
        if let Some(j) = &self.job {
            write!(f, " job `{j}`")?;
        }
        if let Some(s) = &self.step {
            write!(f, " step `{s}`")?;
        }
        write!(f, ": {}", self.detail)
    }
}

/// Whether a step's body or action cancels workflow runs. Shell comment
/// lines (`# ...`) are skipped for every pattern, so a step that only
/// *mentions* cancelling (`# never gh run cancel here`) is not a cancel step.
#[must_use]
pub fn is_cancel_step(step: &Step) -> bool {
    let run_cancels = step.run.as_deref().is_some_and(|r| {
        r.lines().any(|l| {
            let l = l.trim();
            !l.starts_with('#')
                && (l.contains("run cancel")
                    || l.contains("force-cancel")
                    || l.contains("/cancel\"")
                    || l.contains("/cancel ")
                    || l.ends_with("/cancel"))
        })
    });
    let uses_cancels = step
        .uses
        .as_deref()
        .is_some_and(|u| u.to_ascii_lowercase().contains("cancel"));
    run_cancels || uses_cancels
}

/// Evaluate a `cancel-in-progress` value: YAML boolean literals are booleans
/// (not the truthy string "false"); anything else is an expression.
fn cancel_value(c: &Concurrency, scope: &dyn expr::Context) -> Value {
    c.cancel_in_progress
        .as_deref()
        .map_or(Value::Bool(false), |s| match s.trim() {
            "true" | "True" | "TRUE" => Value::Bool(true),
            "false" | "False" | "FALSE" => Value::Bool(false),
            _ => interpolate(s, scope),
        })
}

/// Lint every workflow. An empty result means no started `push` /
/// `merge_group` run can be cancelled by anything these files declare.
#[must_use]
pub fn lint(workflows: &[Workflow]) -> Vec<CancelFinding> {
    let mut out = Vec::new();
    for wf in workflows {
        for event in PROTECTED_EVENTS {
            if triggered(wf, event) {
                lint_event(wf, event, &mut out);
            }
        }
    }
    out
}

fn lint_event(wf: &Workflow, event: Event, out: &mut Vec<CancelFinding>) {
    let ctx = EventContext::new(event, &wf.name);
    let pr =
        triggered(wf, Event::PullRequest).then(|| EventContext::new(Event::PullRequest, &wf.name));
    let finding =
        |job: Option<&str>, step: Option<&Step>, line: usize, detail: String| CancelFinding {
            workflow: wf.file.clone(),
            job: job.map(str::to_string),
            step: step.map(Step::label),
            line,
            event,
            detail,
        };

    if let Some(c) = &wf.concurrency {
        let pr_scope = pr.as_ref().map(WorkflowScope);
        check_concurrency(
            c,
            &WorkflowScope(&ctx),
            pr_scope.as_ref().map(|s| s as &dyn expr::Context),
            event,
            &mut |line, d| out.push(finding(None, None, line, d)),
        );
    }

    let states = job_states(wf, &ctx);
    let pr_states = pr.as_ref().map(|p| job_states(wf, p));
    for job in &wf.jobs {
        if states[&job.id] == RunState::Skipped {
            continue;
        }
        let job_scope = JobScope {
            event: &ctx,
            needs: needs_of(job, &states),
            job_level: true,
        };
        if let Some(c) = &job.concurrency {
            // The PR side only matters when the job actually runs on a PR.
            let pr_job_scope = match (&pr, &pr_states) {
                (Some(p), Some(ps)) if ps[&job.id] != RunState::Skipped => Some(JobScope {
                    event: p,
                    needs: needs_of(job, ps),
                    job_level: true,
                }),
                _ => None,
            };
            check_concurrency(
                c,
                &job_scope,
                pr_job_scope.as_ref().map(|s| s as &dyn expr::Context),
                event,
                &mut |line, d| out.push(finding(Some(&job.id), None, line, d)),
            );
        }
        let step_scope = JobScope {
            event: &ctx,
            needs: needs_of(job, &states),
            job_level: false,
        };
        for step in job.steps.iter().filter(|s| is_cancel_step(s)) {
            let reach = eval_condition(step.if_cond.as_deref(), &step_scope).truth();
            if reach == Truth::False {
                continue;
            }
            let how = if states[&job.id] == RunState::Runs && reach == Truth::True {
                "runs"
            } else {
                "cannot be proven skipped"
            };
            out.push(finding(
                Some(&job.id),
                Some(step),
                step.line,
                format!(
                    "a run-cancelling step {how} on {}: it can cancel a started run of a distinct commit \
                     (ci-principles.md rule 2; gate the job or step on `github.event_name == 'pull_request'`)",
                    event.name()
                ),
            ));
        }
    }
}

fn check_concurrency(
    c: &Concurrency,
    scope: &dyn expr::Context,
    pr_scope: Option<&dyn expr::Context>,
    event: Event,
    report: &mut dyn FnMut(usize, String),
) {
    match cancel_value(c, scope).truth() {
        Truth::False => {}
        Truth::True => report(
            c.line,
            format!(
                "cancel-in-progress is true on {}: a newer run would cancel a started one (ci-principles.md rule 2)",
                event.name()
            ),
        ),
        Truth::Unknown => report(
            c.line,
            format!(
                "cancel-in-progress `{}` cannot be proven false on {}",
                c.cancel_in_progress.as_deref().unwrap_or_default(),
                event.name()
            ),
        ),
    }
    let Some(pr_scope) = pr_scope else {
        return;
    };
    if cancel_value(c, pr_scope).truth() == Truth::False {
        return;
    }
    match (interpolate(&c.group, scope), interpolate(&c.group, pr_scope)) {
        (Value::Str(mine), Value::Str(theirs)) if mine != theirs => {}
        (Value::Str(mine), Value::Str(_)) => report(
            c.line,
            format!(
                "group `{}` resolves to `{mine}` on both {} and pull_request, and pull_request cancels in progress: a PR run would cancel the started {} run",
                c.group,
                event.name(),
                event.name()
            ),
        ),
        _ => report(
            c.line,
            format!(
                "group `{}` cannot be resolved on {} or pull_request, so a cancelling PR run sharing it cannot be ruled out",
                c.group,
                event.name()
            ),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::merge_group_ci::workflow;
    use std::path::Path;

    /// Fixtures spell the CLI as `__GH__` so the forge-inventory gate, which
    /// counts `gh <noun>` tokens in `.rs` source, does not read these YAML
    /// fixtures as direct forge call sites.
    fn wf(src: &str) -> Workflow {
        workflow::parse("t.yml", &src.replace("__GH__", "gh")).unwrap()
    }

    const PR_ONLY_CANCEL_STEP: &str = r"
name: CI
on:
  push:
    branches: [main]
  pull_request:
concurrency:
  group: ci-${{ github.event.pull_request.number || github.ref }}
  cancel-in-progress: ${{ github.event_name == 'pull_request' }}
jobs:
  changes:
    if: github.event_name == 'pull_request'
    runs-on: ubuntu-latest
    steps:
      - name: Cancel older runs
        run: |
          __GH__ run list --branch x --json databaseId | while read -r id; do
            __GH__ run cancel $id
          done
  test:
    runs-on: ubuntu-latest
    steps:
      - run: cargo test
";

    #[test]
    fn pr_only_cancellation_is_clean() {
        assert_eq!(lint(&[wf(PR_ONLY_CANCEL_STEP)]), Vec::new());
    }

    /// The #7779 shape: the cancel job runs on every event, and on a push its
    /// ref falls back to `main`.
    #[test]
    fn ungated_cancel_step_is_flagged_on_push() {
        let src = PR_ONLY_CANCEL_STEP.replace("    if: github.event_name == 'pull_request'\n", "");
        let f = lint(&[wf(&src)]);
        assert_eq!(f.len(), 1, "{f:?}");
        assert_eq!(f[0].event, Event::Push);
        assert_eq!(f[0].job.as_deref(), Some("changes"));
        assert_eq!(f[0].step.as_deref(), Some("Cancel older runs"));
        assert!(f[0].detail.contains("runs on push"), "{}", f[0].detail);
    }

    #[test]
    fn step_level_pr_gate_is_enough() {
        let src = PR_ONLY_CANCEL_STEP
            .replace("    if: github.event_name == 'pull_request'\n", "")
            .replace(
                "      - name: Cancel older runs\n",
                "      - name: Cancel older runs\n        if: github.event_name == 'pull_request'\n",
            );
        assert_eq!(lint(&[wf(&src)]), Vec::new());
    }

    #[test]
    fn unprovable_gate_fails_closed() {
        let src = PR_ONLY_CANCEL_STEP.replace(
            "if: github.event_name == 'pull_request'",
            "if: vars.CANCEL_SUPERSEDED == 'true'",
        );
        let f = lint(&[wf(&src)]);
        assert_eq!(f.len(), 1, "{f:?}");
        assert!(f[0].detail.contains("cannot be proven skipped"), "{}", f[0].detail);
    }

    #[test]
    fn cancel_actions_and_rest_endpoints_are_recognised() {
        for body in [
            "      - uses: styfle/cancel-workflow-action@0.12.1\n",
            "      - run: __GH__ api -X POST repos/o/r/actions/runs/1/cancel\n",
            "      - run: __GH__ api -X POST \"repos/o/r/actions/runs/$id/force-cancel\"\n",
        ] {
            let src = PR_ONLY_CANCEL_STEP
                .replace("      - run: cargo test\n", &format!("      - run: cargo test\n{body}"));
            let f = lint(&[wf(&src)]);
            assert_eq!(f.len(), 1, "{body}: {f:?}");
            assert_eq!(f[0].job.as_deref(), Some("test"));
        }
    }

    /// A comment that only mentions cancelling is not a cancel step, for
    /// every pattern (`run cancel`, `force-cancel`, the `/cancel` endpoint).
    #[test]
    fn comment_lines_are_not_cancel_steps() {
        let body = "      - run: |\n          # never __GH__ run cancel here, nor force-cancel, nor POST .../cancel\n          echo ok\n";
        let src = PR_ONLY_CANCEL_STEP
            .replace("      - run: cargo test\n", &format!("      - run: cargo test\n{body}"));
        assert_eq!(lint(&[wf(&src)]), Vec::new());
    }

    #[test]
    fn cancel_in_progress_true_on_push_is_flagged() {
        for value in [
            "true",
            "${{ github.event_name != 'pull_request' }}",
            "${{ vars.X }}",
        ] {
            let src = PR_ONLY_CANCEL_STEP.replace(
                "cancel-in-progress: ${{ github.event_name == 'pull_request' }}",
                &format!("cancel-in-progress: {value}"),
            );
            let f = lint(&[wf(&src)]);
            assert!(
                f.iter()
                    .any(|x| x.job.is_none() && x.detail.contains("cancel-in-progress")),
                "{value}: {f:?}"
            );
        }
    }

    #[test]
    fn literal_false_is_not_the_truthy_string() {
        let src = PR_ONLY_CANCEL_STEP.replace(
            "cancel-in-progress: ${{ github.event_name == 'pull_request' }}",
            "cancel-in-progress: false",
        );
        assert_eq!(lint(&[wf(&src)]), Vec::new());
    }

    /// A group shared by push and PR, where PR cancels in progress: the PR
    /// run's own setting would cancel main's started run.
    #[test]
    fn group_shared_with_a_cancelling_pr_run_is_flagged() {
        let src = PR_ONLY_CANCEL_STEP.replace(
            "group: ci-${{ github.event.pull_request.number || github.ref }}",
            "group: ci-${{ github.workflow }}",
        );
        let f = lint(&[wf(&src)]);
        assert_eq!(f.len(), 1, "{f:?}");
        assert!(f[0].detail.contains("a PR run would cancel"), "{}", f[0].detail);
    }

    #[test]
    fn job_level_concurrency_is_checked() {
        let src = PR_ONLY_CANCEL_STEP.replace(
            "  test:\n    runs-on: ubuntu-latest\n",
            "  test:\n    runs-on: ubuntu-latest\n    concurrency:\n      group: t-${{ github.sha }}\n      cancel-in-progress: true\n",
        );
        let f = lint(&[wf(&src)]);
        assert!(
            f.iter().any(|x| x.job.as_deref() == Some("test")
                && x.detail.contains("cancel-in-progress is true on push")),
            "{f:?}"
        );
    }

    #[test]
    fn merge_group_runs_are_protected_too() {
        let src = PR_ONLY_CANCEL_STEP
            .replace(
                "  pull_request:\n",
                "  pull_request:\n  merge_group:\n    types: [checks_requested]\n",
            )
            .replace("if: github.event_name == 'pull_request'", "if: github.event_name != 'push'");
        let f = lint(&[wf(&src)]);
        assert_eq!(f.len(), 1, "{f:?}");
        assert_eq!(f[0].event, Event::MergeGroup);
    }

    /// The lint over this repository's real workflows: no cancel path a
    /// `push` to main (or a merge group) can reach may cancel a started run.
    #[test]
    fn repo_workflows_never_cancel_a_started_main_run() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
        let loaded = workflow::load_dir(root).unwrap();
        let workflows: Vec<Workflow> = loaded.into_iter().map(|r| r.unwrap()).collect();
        assert!(
            workflows.iter().any(|w| w.file.ends_with("ci.yml")),
            "ci.yml was not loaded; the lint would pass vacuously"
        );
        let findings = lint(&workflows);
        assert!(
            findings.is_empty(),
            "a workflow can cancel a started main/merge-group run (#10670, ci-principles.md rule 2):\n{}",
            findings.iter().map(ToString::to_string).collect::<Vec<_>>().join("\n")
        );
    }

    /// The PR-only cancel steps really are seen as cancel steps, so the clean
    /// result above is not vacuous.
    #[test]
    fn repo_cancel_steps_are_detected_and_pr_only() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
        let workflows: Vec<Workflow> = workflow::load_dir(root)
            .unwrap()
            .into_iter()
            .map(|r| r.unwrap())
            .collect();
        let cancel_steps: Vec<String> = workflows
            .iter()
            .flat_map(|w| {
                w.jobs.iter().flat_map(move |j| {
                    j.steps
                        .iter()
                        .filter(|s| is_cancel_step(s))
                        .map(move |s| format!("{}:{}:{}", w.file, j.id, s.label()))
                })
            })
            .collect();
        assert!(
            cancel_steps.iter().any(|s| s.contains("ci.yml:changes:")),
            "ci.yml's PR-only cancel step is no longer recognised: {cancel_steps:?}"
        );
    }
}
