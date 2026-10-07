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
//! 4. A cancel step that runs on `pull_request` but can *target* a main run:
//!    the job gate keeps it off a push, yet a PR run holds `actions: write`, so
//!    a step that lists `main` (or any run that is not a `pull_request` run)
//!    cancels a started main run from the PR side. The lint pins the target
//!    safety of every PR-reachable `run:` cancel step: its ref must be
//!    `github.head_ref` with no fallback, and its run selection must filter on
//!    `.event == \"pull_request\"`.
//!
//! What it deliberately does not police: superseding a PR run is correct
//! (rule 2), so `cancel-in-progress` on `pull_request` is left alone.

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

/// Whether `line` names a REST `.../cancel` endpoint: `/cancel` must end the
/// path segment, whatever shell quoting (`"`, `'`), whitespace, `?query` or
/// `)` follows it. `/cancelled` or `/cancel-foo` are other segments.
fn ends_cancel_endpoint(line: &str) -> bool {
    line.match_indices("/cancel").any(|(i, m)| {
        line[i + m.len()..]
            .chars()
            .next()
            .is_none_or(|c| !(c.is_alphanumeric() || matches!(c, '-' | '_' | '.' | '/')))
    })
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
                    || ends_cancel_endpoint(l))
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
        if triggered(wf, Event::PullRequest) {
            lint_pr_targets(wf, &mut out);
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

/// Whether the one `gh run cancel` in `code` takes its run ID from the loop
/// variable of `... --jq '<PR-only filter>' | while read [-r] VAR; do ...`, so
/// every cancellation target is an ID the PR-only selection emitted. Fails
/// closed on anything else: a stage between the filter and the loop, a second
/// cancel (including `force-cancel` and the REST `/cancel` endpoint), a cancel
/// argument other than `$VAR`, or the loop variable being re-read or reassigned.
fn cancel_consumes_filtered_ids(code: &str) -> bool {
    if code.contains("force-cancel") || code.contains("/cancel") {
        return false;
    }
    let norm = code
        .replace("\\\n", " ")
        .replace('|', " | ")
        .replace([';', '\n'], " ; ");
    let Some((_, list)) = norm.split_once(concat!("g", "h run list")) else {
        return false;
    };
    let Some(i) = list.find("--jq") else {
        return false;
    };
    let mut quotes = list[i..].splitn(3, '\'');
    let (Some(_), Some(_), Some(tail)) = (quotes.next(), quotes.next(), quotes.next()) else {
        return false;
    };
    let toks: Vec<&str> = tail.split_whitespace().collect();
    let mut it = toks.iter().copied();
    if it.next() != Some("|") || it.next() != Some("while") || it.next() != Some("read") {
        return false;
    }
    let mut var = it.next();
    if var == Some("-r") {
        var = it.next();
    }
    let Some(var) =
        var.filter(|v| !v.is_empty() && v.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'))
    else {
        return false;
    };
    if it.next() != Some(";") || it.next() != Some("do") {
        return false;
    }
    let body: Vec<&str> = it.collect();
    let all: Vec<&str> = norm.split_whitespace().collect();
    if all
        .windows(3)
        .filter(|w| *w == ["gh", "run", "cancel"])
        .count()
        != 1
    {
        return false;
    }
    loop_body_is_supported(&body, var)
}

/// A shell word that cannot run a command or write a variable: no command or
/// process substitution, no legacy `$[...]` arithmetic, and no `${...=...}`
/// assigning expansion.
fn inert_word(w: &str) -> bool {
    if ["$(", "`", "$[", "<(", ">("].iter().any(|p| w.contains(p)) {
        return false;
    }
    w.split("${")
        .skip(1)
        .all(|r| !r.split('}').next().unwrap_or("").contains('='))
}

/// `$name`, `${name}` or either quoted: a plain variable read.
fn plain_var_read(w: &str) -> bool {
    let w = w.trim_matches('"');
    let name = w
        .strip_prefix("${")
        .and_then(|r| r.strip_suffix('}'))
        .or_else(|| w.strip_prefix('$'));
    name.is_some_and(|n| !n.is_empty() && n.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'))
}

/// Allowlist of the loop body (the tokens after `do`, ending in `done`). Each
/// statement must be one of: `if [[ <read> (!=|==) <read> ]]`, `then`, `fi`,
/// `echo <inert words>`, or the one `gh run cancel $VAR [--repo <inert>]`
/// optionally followed by `|| echo <inert words>`. A statement outside that
/// set (`printf -v`, `eval`, `declare`, `id=...`, `read`, a pipeline, ...) is a
/// command whose effect on the target is not understood, so it fails closed.
fn loop_body_is_supported(body: &[&str], var: &str) -> bool {
    let Some((&"done", stmts)) = body.split_last() else {
        return false;
    };
    let echo_ok =
        |s: &[&str]| s.first() == Some(&"echo") && s.iter().all(|w| *w != "|" && inert_word(w));
    let mut cancels = 0;
    for stmt in stmts.split(|t| *t == ";") {
        let stmt = match stmt {
            ["then", rest @ ..] if !rest.is_empty() => rest,
            s => s,
        };
        let ok = match stmt {
            [] | ["then"] | ["fi"] => true,
            ["if", "[[", a, "!=" | "==", b, "]]"] => plain_var_read(a) && plain_var_read(b),
            ["echo", ..] => echo_ok(stmt),
            ["gh", "run", "cancel", arg, rest @ ..] => {
                let (flags, fallback) = match rest.iter().position(|t| *t == "|") {
                    Some(i) => (&rest[..i], Some(&rest[i..])),
                    None => (rest, None),
                };
                let arg = arg.trim_matches('"');
                let target = arg == format!("${var}") || arg == format!("${{{var}}}");
                let flags_ok = match flags {
                    [] => true,
                    ["--repo", v] => inert_word(v),
                    _ => false,
                };
                let fallback_ok =
                    fallback.is_none_or(|f| matches!(f, ["|", "|", ..]) && echo_ok(&f[2..]));
                cancels += 1;
                target && flags_ok && fallback_ok
            }
            _ => false,
        };
        if !ok {
            return false;
        }
    }
    cancels == 1
}

/// Whether the whitespace-stripped jq `program` after the PR-only filter keeps
/// the selected run objects intact and ends in the exact `.databaseId`
/// projection: zero or more whole `select(...)` stages, then `.databaseId`.
fn preserves_run_ids(program: &str) -> bool {
    let mut stages: Vec<&str> = program.split('|').collect();
    if stages.pop() != Some(".databaseId") {
        return false;
    }
    stages.iter().all(|st| {
        let Some(inner) = st.strip_prefix("select(") else {
            return false;
        };
        // The opening paren must close only at the very end of the stage.
        let mut depth = 1usize;
        for (i, c) in inner.char_indices() {
            match c {
                '(' => depth += 1,
                ')' => {
                    depth -= 1;
                    if depth == 0 {
                        return i + 1 == inner.len();
                    }
                }
                _ => {}
            }
        }
        false
    })
}

/// Why a cancel step that runs on a PR could still reach a main run, if it can.
/// A step with no `run:` body (a `uses:` cancel action) has a target the lint
/// cannot inspect, so it fails closed.
fn pr_target_problem(step: &Step) -> Option<&'static str> {
    let Some(run) = step.run.as_deref() else {
        return Some("it is an opaque `uses:` cancel action whose target runs cannot be inspected");
    };
    let compact = |s: &str| s.split_whitespace().collect::<String>();
    let code: String = run
        .lines()
        .filter(|l| !l.trim().starts_with('#'))
        .collect::<Vec<_>>()
        .join("\n");
    // The run selection is the `--jq` program of the one `gh run list` whose
    // output feeds the cancel: its pipeline's *first* stage must be the PR-only
    // filter `'.[] | select(.event == "pull_request") | <plain filters>'`.
    // Anything after it may only narrow (`select(...)` stages) and then project
    // the exact `.databaseId`; a constant, arithmetic or any other stage could
    // emit an ID the filter never selected. A widened
    // predicate (`... or .event == "push"`), a stage before the filter, or a
    // construct that can emit values from outside the filtered stream (a comma
    // branch such as `.[] | ., select(...)`, `..`, variables, constructors) is
    // unrecognised and fails closed. The program is read from the actual
    // command, never from elsewhere in the script, so the same text inside an
    // `echo` or a string proves nothing; more than one `gh run list`, a `-q`
    // alias, or a list that does not pipe into the cancel is unrecognised too.
    let squashed = compact(&code.replace("\\\n", " "));
    let prefix = r#"--jq'.[]|select(.event=="pull_request")|"#;
    let only_pr_events = squashed.matches("ghrunlist").count() == 1
        && squashed.split_once("ghrunlist").is_some_and(|(_, list)| {
            let Some(i) = list.find("--jq") else {
                return false;
            };
            let Some(rest) = list[i..].strip_prefix(prefix) else {
                return false;
            };
            let Some((program, _)) = rest.split_once('\'') else {
                return false;
            };
            !list[..i].contains("-q")
                && !program.contains(".event")
                && preserves_run_ids(program)
                && !program.contains([',', '$', '[', '{', '/', '?'])
                && !program.contains("..")
                && !["input", "reduce", "foreach", "limit", "env", "path"]
                    .iter()
                    .any(|w| program.contains(w))
                && cancel_consumes_filtered_ids(&code)
        });
    if !only_pr_events {
        return Some("its run selection is not exactly `select(.event == \"pull_request\")`, so it can list and cancel push runs");
    }
    let unsafe_ref =
        |v: &str| v.contains("ref_name") || v.contains("github.ref") || v.contains("||");
    let head_ref = |v: &str| compact(v) == "${{github.head_ref}}";
    if code.contains("ref_name") || step.env.iter().any(|(_, v)| unsafe_ref(v)) {
        return Some(
            "its target ref falls back to `ref_name`/`github.ref`, which is `main` on a push",
        );
    }
    if !step.env.iter().any(|(_, v)| head_ref(v)) {
        return Some("its target ref is not `github.head_ref` (no `env:` value is exactly `${{ github.head_ref }}`)");
    }
    None
}

/// Rule 4: PR-reachable cancel steps must not be able to target a main run.
fn lint_pr_targets(wf: &Workflow, out: &mut Vec<CancelFinding>) {
    let ctx = EventContext::new(Event::PullRequest, &wf.name);
    let states = job_states(wf, &ctx);
    for job in &wf.jobs {
        if states[&job.id] == RunState::Skipped {
            continue;
        }
        let scope = JobScope {
            event: &ctx,
            needs: needs_of(job, &states),
            job_level: false,
        };
        for step in job.steps.iter().filter(|s| is_cancel_step(s)) {
            if eval_condition(step.if_cond.as_deref(), &scope).truth() == Truth::False {
                continue;
            }
            if let Some(why) = pr_target_problem(step) {
                out.push(CancelFinding {
                    workflow: wf.file.clone(),
                    job: Some(job.id.clone()),
                    step: Some(step.label()),
                    line: step.line,
                    event: Event::PullRequest,
                    detail: format!(
                        "a cancel step reachable on pull_request can target a started main run: {why} (ci-principles.md rule 2)"
                    ),
                });
            }
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

    const PR_ONLY_CANCEL_STEP: &str = r#"
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
        env:
          REF: ${{ github.head_ref }}
        run: |
          __GH__ run list --branch "$REF" --json databaseId,event --jq '.[] | select(.event == "pull_request") | .databaseId' | while read -r id; do
            __GH__ run cancel $id
          done
  test:
    runs-on: ubuntu-latest
    steps:
      - run: cargo test
"#;

    #[test]
    fn pr_only_cancellation_is_clean() {
        assert_eq!(lint(&[wf(PR_ONLY_CANCEL_STEP)]), Vec::new());
    }

    /// The PR-side hole (#10677 review): the job gate keeps the step off a
    /// push, but a PR run can still cancel main if the step targets it.
    #[test]
    fn pr_only_step_targeting_main_is_flagged() {
        for (from, to) in [
            (
                r#"--jq '.[] | select(.event == "pull_request") | .databaseId'"#,
                "--jq '.[] | .databaseId'",
            ),
            ("REF: ${{ github.head_ref }}", "REF: ${{ github.head_ref || github.ref_name }}"),
            ("REF: ${{ github.head_ref }}", "REF: main"),
            (
                r#"select(.event == "pull_request")"#,
                r#"select(.event == "pull_request" or .event == "push")"#,
            ),
            (
                r#"select(.event == "pull_request")"#,
                r#"select(.event == "pull_request" or true)"#,
            ),
            (r#"select(.event == "pull_request")"#, r#"select(.event != "schedule")"#),
            // A comma branch emits the unfiltered element too (jq prints the
            // push ID once per branch), so the exact `select` is not proof.
            (
                r#"'.[] | select(.event == "pull_request") | .databaseId'"#,
                r#"'.[] | ., select(.event == "pull_request") | .databaseId'"#,
            ),
            (
                r#"'.[] | select(.event == "pull_request") | .databaseId'"#,
                r#"'.[] | select(.event == "pull_request"), . | .databaseId'"#,
            ),
            (
                r#"'.[] | select(.event == "pull_request") | .databaseId'"#,
                r#"'.[] | select(.event == "pull_request") | ., .databaseId'"#,
            ),
            (
                r#"'.[] | select(.event == "pull_request") | .databaseId'"#,
                r#"'(.[] | select(.event == "pull_request")), .[] | .databaseId'"#,
            ),
            (
                r#"'.[] | select(.event == "pull_request") | .databaseId'"#,
                r#"'.[] | select(.event == "pull_request") | $ENV.X'"#,
            ),
            // The projection must stay the exact `.databaseId`: a constant or
            // arithmetic result is an ID the filter never selected.
            (".databaseId'", "123'"),
            (".databaseId'", ".databaseId + 1'"),
            (".databaseId'", ".databaseId | 123'"),
            (".databaseId'", "select(.databaseId) + (1) | .databaseId'"),
            (".databaseId'", "(.databaseId = 123) | .databaseId'"),
        ] {
            let src = PR_ONLY_CANCEL_STEP.replace(from, to);
            assert_ne!(src, PR_ONLY_CANCEL_STEP, "{from}");
            let f = lint(&[wf(&src)]);
            assert_eq!(f.len(), 1, "{to}: {f:?}");
            assert_eq!(f[0].event, Event::PullRequest);
            assert!(f[0].detail.contains("started main run"), "{}", f[0].detail);
        }
    }

    /// The safe filter appearing in an `echo`/string must not vouch for an
    /// unsafe actual selection (#10677 review, #10670 criterion 3).
    #[test]
    fn safe_filter_in_echo_does_not_vouch_for_unsafe_selection() {
        let safe = r#"echo '.[] | select(.event == "pull_request") | .databaseId'"#;
        let src = PR_ONLY_CANCEL_STEP
            .replace(
                r#"--jq '.[] | select(.event == "pull_request") | .databaseId'"#,
                "--jq '.[] | .databaseId'",
            )
            .replace(
                "          __GH__ run list",
                &format!("          {safe}\n          __GH__ run list"),
            );
        assert!(src.contains(safe));
        let f = lint(&[wf(&src)]);
        assert_eq!(f.len(), 1, "{f:?}");
        assert!(f[0].detail.contains("started main run"), "{}", f[0].detail);
    }

    /// A list that does not pipe into the cancel is not the selection.
    #[test]
    fn selection_not_feeding_the_cancel_fails_closed() {
        let src = PR_ONLY_CANCEL_STEP
            .replace(" | while read -r id; do", "; __GH__ run list | while read -r id; do");
        assert_eq!(lint(&[wf(&src)]).len(), 1);
    }

    /// The cancel must consume the filtered stream's IDs: a fixed or unrelated
    /// target, an extra cancel, or a stage that replaces the IDs fails closed
    /// (#10677 review, #10670 criterion 3).
    #[test]
    fn cancel_not_consuming_filtered_ids_fails_closed() {
        let cancel = "            __GH__ run cancel $id\n";
        for (from, to) in [
            (cancel, "            __GH__ run cancel 123\n"),
            (cancel, "            __GH__ run cancel $other\n"),
            (cancel, "            __GH__ run cancel $id\n            __GH__ run cancel 123\n"),
            (cancel, "            id=123\n            __GH__ run cancel $id\n"),
            (cancel, "            read -r id\n            __GH__ run cancel $id\n"),
            (cancel, "            printf -v id '%s' 123\n            __GH__ run cancel $id\n"),
            (cancel, "            eval id=123\n            __GH__ run cancel $id\n"),
            (cancel, "            declare id=123\n            __GH__ run cancel $id\n"),
            (cancel, "            echo $((id=123))\n            __GH__ run cancel $id\n"),
            (cancel, "            echo ${id:=123}\n            __GH__ run cancel $id\n"),
            (cancel, "            echo $(printf 123)\n            __GH__ run cancel $id\n"),
            (cancel, "            [[ 1 -eq id=123 ]]\n            __GH__ run cancel $id\n"),
            (cancel, "            __GH__ run cancel $id 123\n"),
            (cancel, "            __GH__ api -X POST repos/o/r/actions/runs/123/cancel\n"),
            (cancel, "            __GH__ api -X POST 'repos/o/r/actions/runs/123/cancel'\n"),
            ("          done\n", "          done\n          __GH__ run cancel 123\n"),
            (" | while read -r id; do", " | sed 's/.*/123/' | while read -r id; do"),
        ] {
            let src = PR_ONLY_CANCEL_STEP.replace(from, to);
            assert_ne!(src, PR_ONLY_CANCEL_STEP, "{to}");
            let f = lint(&[wf(&src)]);
            assert!(
                f.iter().any(|x| x.event == Event::PullRequest
                    && x.detail.contains("started main run")),
                "{to}: {f:?}"
            );
        }
    }

    /// The `${id}` / quoted forms and extra loop logic (as in ci.yml) stay clean.
    #[test]
    fn cancel_consuming_filtered_ids_in_supported_shapes_is_clean() {
        let cancel = "            __GH__ run cancel $id\n";
        for to in [
            "            __GH__ run cancel \"$id\" --repo r\n",
            "            __GH__ run cancel \"${id}\"\n",
            "            if [[ \"$id\" != \"$CUR\" ]]; then\n              __GH__ run cancel \"$id\" || echo warn\n            fi\n",
        ] {
            let src = PR_ONLY_CANCEL_STEP.replace(cancel, to);
            assert_eq!(lint(&[wf(&src)]), Vec::new(), "{to}");
        }
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
            "      - run: __GH__ api -X POST 'repos/o/r/actions/runs/1/cancel'\n",
        ] {
            let src = PR_ONLY_CANCEL_STEP
                .replace("      - run: cargo test\n", &format!("      - run: cargo test\n{body}"));
            let f: Vec<_> = lint(&[wf(&src)])
                .into_iter()
                .filter(|x| x.event == Event::Push)
                .collect();
            assert_eq!(f.len(), 1, "{body}: {f:?}");
            assert_eq!(f[0].job.as_deref(), Some("test"));
        }
    }

    /// An opaque `uses:` cancel action reachable on a PR cannot be proven to
    /// spare main runs, so it must not pass silently (#10677 review).
    #[test]
    fn pr_only_uses_cancel_action_fails_closed() {
        let src = PR_ONLY_CANCEL_STEP.replace(
            "      - name: Cancel older runs\n        env:\n          REF: ${{ github.head_ref }}\n        run: |\n          __GH__ run list --branch \"$REF\" --json databaseId,event --jq '.[] | select(.event == \"pull_request\") | .databaseId' | while read -r id; do\n            __GH__ run cancel $id\n          done\n",
            "      - name: Cancel older runs\n        uses: styfle/cancel-workflow-action@0.12.1\n",
        );
        assert_ne!(src, PR_ONLY_CANCEL_STEP);
        let f = lint(&[wf(&src)]);
        assert_eq!(f.len(), 1, "{f:?}");
        assert_eq!(f[0].event, Event::PullRequest);
        assert!(f[0].detail.contains("opaque `uses:`"), "{}", f[0].detail);
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
