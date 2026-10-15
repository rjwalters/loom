//! The required-context conclusion gate for the SYNCHRONOUS merge path
//! (#10688): refuse a merge whose head has a required status-check context
//! that concluded red, and hold one whose required context is missing or
//! still pending.
//!
//! # The gap
//!
//! The overlap classification in the parent module only runs inside
//! `--auto`'s settle-wait. A plain `merge-pr.sh N` reached the merge API call
//! without ever looking at the required contexts' conclusions, so the forge's
//! branch protection was the only guard — and a token that can bypass
//! rulesets (an admin) walks straight through it. The CI-run conclusion gate
//! (`ci_result`, #10444) covers a red check produced by the `CI` workflow;
//! this covers every OTHER required context: another workflow, a third-party
//! app's check run, or a legacy commit status (2am#3112's `GitHub Bypass
//! Check` was exactly that kind).
//!
//! # Contract (`loom-daemon merge-pr required-checks`)
//!
//! | outcome | stdout | exit |
//! |---|---|---|
//! | every required context green (or none required) | [`CLEAN`] | 0 |
//! | the only red contexts were named by `--allow-red-required` | [`BYPASSED`] + names (audit comment posted) | 0 |
//! | a required context concluded red | the refusal naming it | 1 |
//! | a required context is missing or pending (nothing unallowed red) | [`HOLD`] + reason | 3 |
//! | the forge could not be read | [`UNREADABLE`] + reason | 2 |
//!
//! Red outranks waiting: a context that is already red can never turn green on
//! this head, so the refusal is reported even while other contexts are still
//! running. `--allow-red-required <ctx>` lifts the refusal for exactly the
//! named contexts and nothing else — it never turns a missing or pending
//! context into a pass, and naming one context never admits a different red
//! one.
//!
//! # Per-context state
//!
//! A context's evidence is every check run AND every commit status bearing its
//! name (a required context may be either). Check runs of one name: any run
//! not yet `completed` makes the context pending (a re-run in flight supersedes
//! the run it replaces); otherwise the latest run by `started_at` decides, with
//! `success`/`neutral`/`skipped` green — the conclusions GitHub's branch
//! protection accepts — and every other conclusion red (an unrecognised one is
//! not evidence of success). Statuses: `success` green, `pending` pending,
//! anything else red. Across sources the worst state wins.

use std::collections::BTreeSet;

use serde_json::Value;

use crate::merge_pr::stale_checks::CheckRun;

/// Stdout when every required context is green.
pub const CLEAN: &str = "LOOM-REQUIRED-CHECKS-CLEAN";
/// Stdout prefix when red contexts were all overridden (exit 0).
pub const BYPASSED: &str = "LOOM-REQUIRED-CHECKS-BYPASSED";
/// Stdout prefix when a required context is missing or pending (exit 3).
pub const HOLD: &str = "LOOM-REQUIRED-CHECKS-HOLD";
/// Stdout prefix when the forge could not be read (exit 2).
pub const UNREADABLE: &str = "LOOM-REQUIRED-CHECKS-UNREADABLE";

/// Check-run conclusions branch protection accepts for a required check.
const GREEN_CONCLUSIONS: &[&str] = &["success", "neutral", "skipped"];

/// One required context's state on the head being merged.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum State {
    Green,
    /// Not reported at all on this head.
    Missing,
    /// Reported but not concluded; carries the status (`queued`, `pending`, …).
    Pending(String),
    /// Concluded anything but green; carries the conclusion.
    Red(String),
}

/// A commit status from the combined-status endpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Status {
    pub context: String,
    pub state: String,
}

/// `(context, detail)` pairs, sorted by context.
pub type Named = Vec<(String, String)>;

/// The gate's verdict.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    Clean,
    /// Red contexts existed but every one was named by `--allow-red-required`.
    Bypassed(Named),
    /// Unallowed red contexts (the refusal), plus any that were overridden.
    Refuse {
        red: Named,
        bypassed: Named,
    },
    /// Missing/pending contexts, with nothing unallowed red.
    Hold {
        waiting: Named,
        bypassed: Named,
    },
}

fn from_check_runs(runs: &[&CheckRun]) -> Option<State> {
    if runs.is_empty() {
        return None;
    }
    if let Some(r) = runs.iter().find(|r| r.status != "completed") {
        return Some(State::Pending(r.status.clone()));
    }
    let latest = runs.iter().max_by_key(|r| r.started_at)?;
    let c = latest.conclusion.as_deref().unwrap_or("");
    Some(if GREEN_CONCLUSIONS.contains(&c) {
        State::Green
    } else {
        State::Red(if c.is_empty() {
            "no conclusion".into()
        } else {
            c.into()
        })
    })
}

fn from_status(s: &Status) -> State {
    match s.state.as_str() {
        "success" => State::Green,
        "pending" => State::Pending("pending".into()),
        other => State::Red(other.into()),
    }
}

/// The state of `context` on the head: the worst of its check runs and
/// statuses, or [`State::Missing`] when neither reports it.
#[must_use]
pub fn context_state(context: &str, runs: &[CheckRun], statuses: &[Status]) -> State {
    let named: Vec<&CheckRun> = runs.iter().filter(|r| r.name == context).collect();
    from_check_runs(&named)
        .into_iter()
        .chain(
            statuses
                .iter()
                .filter(|s| s.context == context)
                .map(from_status),
        )
        .max()
        .unwrap_or(State::Missing)
}

/// Classify every required context. `allow` names the contexts whose red
/// conclusion the operator overrides (`--allow-red-required`).
#[must_use]
pub fn assess(
    required: &[String],
    runs: &[CheckRun],
    statuses: &[Status],
    allow: &[String],
) -> Verdict {
    let allow: BTreeSet<&str> = allow.iter().map(String::as_str).collect();
    let (mut red, mut bypassed, mut waiting) = (Named::new(), Named::new(), Named::new());
    let required: BTreeSet<&str> = required.iter().map(String::as_str).collect();
    for ctx in required {
        match context_state(ctx, runs, statuses) {
            State::Green => {}
            State::Red(c) if allow.contains(ctx) => bypassed.push((ctx.into(), c)),
            State::Red(c) => red.push((ctx.into(), c)),
            State::Pending(s) => waiting.push((ctx.into(), s)),
            State::Missing => waiting.push((ctx.into(), "not reported".into())),
        }
    }
    if !red.is_empty() {
        Verdict::Refuse { red, bypassed }
    } else if !waiting.is_empty() {
        Verdict::Hold { waiting, bypassed }
    } else if !bypassed.is_empty() {
        Verdict::Bypassed(bypassed)
    } else {
        Verdict::Clean
    }
}

/// `` `ctx` (detail), … ``
#[must_use]
pub fn list(named: &Named) -> String {
    named
        .iter()
        .map(|(c, d)| format!("`{c}` ({d})"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// The refusal printed on exit 1.
#[must_use]
pub fn refusal(pr: &str, sha: &str, red: &Named) -> String {
    format!(
        "Merge blocked: PR #{pr}'s head {sha} has required status-check context(s) that concluded red: {} (#10688). \
A red required check cannot turn green on this head, and a token able to bypass the branch rules would otherwise merge straight over it. \
Fix or re-run the check (never cancel a run), or — operator asserting responsibility — re-run with `--allow-red-required <context>` once per context.",
        list(red)
    )
}

/// The hold reason printed (after [`HOLD`]) on exit 3.
#[must_use]
pub fn hold_reason(sha: &str, waiting: &Named) -> String {
    format!(
        "required status-check context(s) not concluded green on head {sha}: {} (#10688). \
Unknown is not success: nothing was merged; re-attempt once they report.",
        list(waiting)
    )
}

/// The audit comment a real merge posts for an `--allow-red-required` override.
#[must_use]
pub fn bypass_comment(pr: &str, sha: &str, bypassed: &Named) -> String {
    format!(
        "## Merge Proceeded Over Red Required Check(s)\n\n\
`merge-pr.sh` merged PR #{pr} (head `{sha}`) with `--allow-red-required`, overriding the required-context conclusion gate (#10688) for: {}.\n\n\
The operator asserted responsibility for these contexts only; any other red required context still refuses the merge.",
        list(bypassed)
    )
}

/// The `statuses` of a `GET commits/{sha}/status` payload. `Err` when the
/// payload is unreadable or truncated: a partial list cannot prove a required
/// status is absent or green.
pub fn parse_statuses(combined: &Value) -> Result<Vec<Status>, String> {
    let arr = combined
        .get("statuses")
        .and_then(Value::as_array)
        .ok_or("combined status has no `statuses` array")?;
    if let Some(n) = combined.get("total_count").and_then(Value::as_u64) {
        if n as usize > arr.len() {
            return Err(format!("combined status truncated ({} of {n} read)", arr.len()));
        }
    }
    arr.iter()
        .map(|s| {
            let f = |k: &str| s.get(k).and_then(Value::as_str).map(String::from);
            Ok(Status {
                context: f("context").ok_or("a status has no string `context`")?,
                state: f("state").ok_or("a status has no string `state`")?,
            })
        })
        .collect()
}

/// `[{name, status, conclusion, started_at}]` rows (the `--from-stdin` shape)
/// as [`CheckRun`]s.
pub fn parse_check_runs(rows: &Value) -> Result<Vec<CheckRun>, String> {
    rows.as_array()
        .ok_or("`check_runs` is not an array")?
        .iter()
        .map(|v| {
            let s = |k: &str| v.get(k).and_then(Value::as_str);
            let started_at = match s("started_at") {
                Some(t) if !t.is_empty() => Some(
                    chrono::DateTime::parse_from_rfc3339(t)
                        .map_err(|e| format!("bad started_at {t:?}: {e}"))?
                        .with_timezone(&chrono::Utc),
                ),
                _ => None,
            };
            Ok(CheckRun {
                name: s("name").ok_or("a check run has no string `name`")?.into(),
                status: s("status").unwrap_or_default().into(),
                conclusion: s("conclusion").map(String::from),
                started_at,
                actions_run_id: None,
                actions_job_id: None,
            })
        })
        .collect()
}

/// `(required contexts, check runs, commit statuses)`.
pub type Inputs = (Vec<String>, Vec<CheckRun>, Vec<Status>);

/// The live forge reads: required contexts for `base_ref` (rulesets + classic
/// protection, the same lookup the freshness guard uses), the head's check
/// runs (paginated, short reads refused) and its combined commit status.
pub fn live_inputs(nwo: &str, base_ref: &str, sha: &str) -> Result<Inputs, String> {
    let (required, notices) =
        crate::merge_pr::stale_checks::fetch::required_contexts(nwo, base_ref)?;
    for n in notices {
        eprintln!("{n}");
    }
    if required.is_empty() {
        return Ok((required, Vec::new(), Vec::new()));
    }
    let gh = crate::gh_invocation::gh_bin();
    let runs = crate::merge_pr::stale_checks::fetch::fetch_check_runs(&gh, nwo, sha)?;
    let raw = gh_get(&format!("repos/{nwo}/commits/{sha}/status?per_page=100"))?;
    let combined: Value =
        serde_json::from_str(&raw).map_err(|e| format!("unparseable combined status: {e}"))?;
    Ok((required, runs, parse_statuses(&combined)?))
}

/// `gh api <path>` (REST, read-only) through the counted `gh` facade. It
/// lived in the deleted `ci_result` gate (#10444, #12063 slice 1) and keeps its
/// ledger caller name so the operation inventory does not move.
fn gh_get(path: &str) -> Result<String, String> {
    let out = crate::gh_invocation::GhInvocation::new(
        crate::gh_invocation::Operation::new("merge_guard.ci_result"),
        crate::gh_invocation::AccessIntent::Read,
        crate::gh_invocation::GhTarget::None,
        std::time::Duration::from_secs(60),
    )
    .program(crate::gh_invocation::gh_bin())
    .arg("api")
    .arg(path)
    .run();
    match out {
        crate::cmd_out::CmdOutcome::Ran(o) if o.status.success() => {
            Ok(String::from_utf8_lossy(&o.stdout).into_owned())
        }
        crate::cmd_out::CmdOutcome::Ran(o) => {
            Err(format!("gh api {path} failed: {}", String::from_utf8_lossy(&o.stderr).trim()))
        }
        crate::cmd_out::CmdOutcome::Unavailable(u) => Err(format!("could not exec gh api: {u}")),
    }
}

#[cfg(test)]
mod tests;
