//! The pure half of `forge wait-checks` (#10330): fold one poll's check-runs
//! and combined-status payloads into a single rollup, classify it with the
//! existing merge-path classifiers, and decide what the poll means.
//!
//! Nothing here performs a forge read. The two classifiers are reused rather
//! than re-derived, so the agent-facing wait and `merge-pr.sh --auto`'s settle
//! wait cannot disagree about what "failing" or "pending" means:
//!
//! - [`crate::merge_pr::check_runs_rollup::classify`] — failing / pending /
//!   `total_count`, refusing any payload outside the contract (an unreadable
//!   payload never looks settled);
//! - [`crate::merge_pr::checks_failure::classify`] — the `--required-only`
//!   decision (a failing REQUIRED check refuses; an informational failure
//!   with nothing required pending is the end of the wait).
//!
//! # Verdict parity with `gh pr checks`
//!
//! `gh pr checks` buckets each row as pass / fail / pending / skipping /
//! cancel. The rollup classifier treats `failure`, `timed_out`, `cancelled`
//! and `action_required` as failing and any non-`completed` status as pending.
//! One addition, in the fail-closed direction: a `completed` run whose
//! conclusion is none of `success` / `neutral` / `skipped` (e.g.
//! `startup_failure`, `stale`) is also failing here, so `GREEN` only ever
//! means terminal success — the classifier alone would read such a run as
//! "neither failing nor pending", i.e. green.

use std::collections::BTreeSet;

use serde_json::{json, Value};

use crate::merge_pr::check_runs_rollup;
use crate::merge_pr::checks_failure::{self, Verdict as RequiredVerdict};

/// Conclusions that count as success for a `completed` check-run (gh's
/// `pass` and `skipping` buckets).
const SUCCESS_CONCLUSIONS: [&str; 3] = ["success", "neutral", "skipped"];

/// One failing check, as printed on stderr after a `RED` sentinel:
/// `<name>\t<html_url>\t<run_id>` so `gh run view <run_id> --log-failed`
/// works directly. `run_id` is `-` for a non-Actions check or a legacy status.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Failed {
    pub name: String,
    pub url: String,
    pub run_id: String,
}

/// One poll's classified rollup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rollup {
    /// Rows the forge reported (check-runs plus legacy statuses).
    pub total: u64,
    /// Failing check names, sorted and de-duplicated.
    pub failing: Vec<String>,
    /// Still-running check names, sorted and de-duplicated.
    pub pending: Vec<String>,
    /// Every check name seen, for the required-context presence test.
    pub seen: BTreeSet<String>,
    /// Detail rows for [`Rollup::failing`], for the stderr listing.
    pub failed_detail: Vec<Failed>,
}

/// Merge a complete check-runs payload (`{total_count, check_runs}`, every
/// page already folded in) and a combined-status payload into one rollup.
///
/// # Errors
///
/// A human-readable reason when either payload is outside the contract — the
/// caller turns it into `LOOM-CHECKS-ERROR`, never a settled verdict.
pub fn fold(check_runs: &Value, status: &Value) -> Result<Rollup, String> {
    let mut rows: Vec<Value> = match check_runs.get("check_runs") {
        Some(Value::Array(a)) => a.clone(),
        _ => return Err("unreadable: `check_runs` is missing or not an array".to_string()),
    };
    let runs_total = check_runs
        .get("total_count")
        .and_then(Value::as_u64)
        .ok_or("unreadable: check-runs `total_count` is missing or not an integer")?;
    let statuses: Vec<Value> = match status.get("statuses") {
        Some(Value::Array(a)) => a.clone(),
        None | Some(Value::Null) => Vec::new(),
        Some(_) => return Err("unreadable: `statuses` is not an array".to_string()),
    };
    // A legacy status becomes a synthetic check-run row, so ONE classifier
    // answers for both sources (gh pr checks shows both side by side).
    for s in &statuses {
        let state = s.get("state").and_then(Value::as_str).unwrap_or("");
        let (st, conclusion) = match state {
            "success" => ("completed", json!("success")),
            "failure" | "error" => ("completed", json!("failure")),
            _ => ("in_progress", Value::Null),
        };
        rows.push(json!({
            "name": s.get("context").cloned().unwrap_or(Value::Null),
            "status": st,
            "conclusion": conclusion,
            "details_url": s.get("target_url").cloned().unwrap_or(Value::Null),
        }));
    }
    let total = runs_total + statuses.len() as u64;
    let doc = json!({ "total_count": total, "check_runs": rows });
    let rollup =
        check_runs_rollup::classify(&doc.to_string()).map_err(|r| format!("unreadable: {r}"))?;

    let mut failing: BTreeSet<String> = lines(&rollup.failing);
    // Fail-closed extension (module docs): an unknown terminal conclusion.
    for row in &rows {
        let done = row.get("status").and_then(Value::as_str) == Some("completed");
        let conclusion = row.get("conclusion").and_then(Value::as_str);
        if done && !conclusion.is_some_and(|c| SUCCESS_CONCLUSIONS.contains(&c)) {
            failing.insert(name_of(row));
        }
    }
    let seen: BTreeSet<String> = rows.iter().map(name_of).collect();
    let failed_detail = failing
        .iter()
        .map(|name| detail(name, &rows))
        .collect::<Vec<_>>();
    Ok(Rollup {
        total,
        failing: failing.into_iter().collect(),
        pending: lines(&rollup.pending).into_iter().collect(),
        seen,
        failed_detail,
    })
}

/// What a non-empty rollup means for the wait.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// Terminal failure: these checks failed.
    Red(Vec<String>),
    /// Keep waiting on these (still running, or required but not registered).
    Pending(Vec<String>),
    /// Every relevant check is terminal-success.
    Green,
}

/// The pending name reported while the base branch's required-context set
/// is unknown (its lookup failed): the wait cannot know what it still lacks.
pub const REQUIRED_UNKNOWN: &str = "(required-contexts-unknown)";

/// Decide a non-empty rollup.
///
/// `required` is the base branch's required-context set: `Some` once looked
/// up, `None` while the lookup has not succeeded. Default mode settles on
/// every OBSERVED check and additionally waits for a required context that
/// has not registered yet (the #6169 class: a fast app check finishing
/// before Actions registers). `--required-only` settles on the required set
/// alone, via [`checks_failure::classify`].
///
/// Two fail-closed rules (#10351 review) keep `Green` from ever meaning "an
/// incomplete set was vacuously satisfied":
///
/// - **Unknown set** (`None`): never `Green` — a required context may not
///   have registered yet. Default mode still reports an observed failure as
///   `Red` (that needs no required set); everything else waits on
///   [`REQUIRED_UNKNOWN`].
/// - **Empty set under `--required-only`**: falls back to the default-mode
///   decision over every observed check. (`gh pr checks --required` errors
///   with "no required checks reported" here; it never passes.)
#[must_use]
pub fn decide(r: &Rollup, required: Option<&[String]>, required_only: bool) -> Decision {
    let Some(req) = required else {
        if !required_only && !r.failing.is_empty() {
            return Decision::Red(r.failing.clone());
        }
        let mut waiting = if required_only {
            Vec::new()
        } else {
            r.pending.clone()
        };
        waiting.push(REQUIRED_UNKNOWN.to_string());
        return Decision::Pending(waiting);
    };
    let missing: Vec<String> = req
        .iter()
        .filter(|c| !r.seen.contains(*c))
        .cloned()
        .collect();
    if required_only && !req.is_empty() {
        let mut waiting: Vec<String> = r
            .pending
            .iter()
            .filter(|p| req.contains(p))
            .cloned()
            .collect();
        waiting.extend(missing);
        return match checks_failure::classify(&r.failing, req, !waiting.is_empty()) {
            RequiredVerdict::RequiredFailed(names) => Decision::Red(names),
            RequiredVerdict::StillPending => Decision::Pending(waiting),
            RequiredVerdict::InformationalOnly => Decision::Green,
        };
    }
    if !r.failing.is_empty() {
        return Decision::Red(r.failing.clone());
    }
    if !r.pending.is_empty() {
        return Decision::Pending(r.pending.clone());
    }
    if !missing.is_empty() {
        return Decision::Pending(missing);
    }
    Decision::Green
}

/// The terminal answer of one `wait-checks` run — exactly one sentinel line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Green { sha: String },
    NoChecks { sha: String },
    Red { sha: String, failed: Vec<Failed> },
    Timeout { sha: String, pending: Vec<String> },
    Error(String),
    HeadMoved { old: String, new: String },
}

impl Outcome {
    /// The single stdout line callers branch on.
    #[must_use]
    pub fn sentinel(&self) -> String {
        let line = match self {
            Self::Green { sha } => format!("LOOM-CHECKS-GREEN {sha}"),
            Self::NoChecks { sha } => format!("LOOM-CHECKS-NONE {sha}"),
            Self::Red { sha, failed } => {
                let names: Vec<&str> = failed.iter().map(|f| f.name.as_str()).collect();
                format!("LOOM-CHECKS-RED {sha} {}", names.join(","))
            }
            Self::Timeout { sha, pending } => {
                let names = if pending.is_empty() {
                    "-".to_string()
                } else {
                    pending.join(",")
                };
                format!("LOOM-CHECKS-TIMEOUT {sha} {names}")
            }
            Self::Error(why) => format!("LOOM-CHECKS-ERROR {why}"),
            Self::HeadMoved { old, new } => format!("LOOM-CHECKS-HEAD-MOVED {old} {new}"),
        };
        one_line(&line)
    }

    /// Exit code — 0/1/2/3 match `check-ci-status.sh`. Callers MUST branch on
    /// the sentinel, not this: clap's usage error is also 2, and an older
    /// binary has no `wait-checks` at all.
    #[must_use]
    pub fn exit_code(&self) -> i32 {
        match self {
            Self::Green { .. } | Self::NoChecks { .. } => 0,
            Self::Red { .. } => 1,
            Self::Timeout { .. } => 2,
            Self::Error(_) => 3,
            Self::HeadMoved { .. } => 4,
        }
    }

    /// Lines for stderr, printed AFTER the sentinel.
    #[must_use]
    pub fn detail(&self) -> Vec<String> {
        match self {
            Self::Red { failed, .. } => failed
                .iter()
                .map(|f| one_line(&format!("{}\t{}\t{}", f.name, f.url, f.run_id)))
                .collect(),
            _ => Vec::new(),
        }
    }
}

fn lines(rendered: &str) -> BTreeSet<String> {
    rendered
        .lines()
        .filter(|l| !l.is_empty())
        .map(String::from)
        .collect()
}

fn name_of(row: &Value) -> String {
    row.get("name")
        .and_then(Value::as_str)
        .unwrap_or("null")
        .to_string()
}

fn detail(name: &str, rows: &[Value]) -> Failed {
    let row = rows.iter().find(|r| name_of(r) == name);
    let field = |k: &str| row.and_then(|r| r.get(k)).and_then(Value::as_str);
    let details = field("details_url").unwrap_or("");
    Failed {
        name: name.to_string(),
        url: field("html_url")
            .or(Some(details).filter(|d| !d.is_empty()))
            .unwrap_or("-")
            .to_string(),
        run_id: actions_run_id(details).unwrap_or_else(|| "-".to_string()),
    }
}

/// The Actions run id in a `details_url` like
/// `https://github.com/o/r/actions/runs/123/job/456`.
#[must_use]
pub fn actions_run_id(details_url: &str) -> Option<String> {
    let rest = details_url.split("/actions/runs/").nth(1)?;
    let id: String = rest.chars().take_while(char::is_ascii_digit).collect();
    (!id.is_empty()).then_some(id)
}

/// Forge-supplied text (check names, URLs) is untrusted: flatten anything
/// that could split the one-line sentinel contract.
fn one_line(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_control() && c != '\t' { ' ' } else { c })
        .collect()
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn run(name: &str, status: &str, conclusion: Option<&str>) -> Value {
        json!({"name": name, "status": status, "conclusion": conclusion,
               "html_url": format!("https://github.com/o/r/runs/{name}"),
               "details_url": "https://github.com/o/r/actions/runs/77/job/9"})
    }

    fn runs(rows: Vec<Value>) -> Value {
        json!({"total_count": rows.len(), "check_runs": rows})
    }

    fn no_status() -> Value {
        json!({"state": "pending", "statuses": [], "total_count": 0})
    }

    /// Each conclusion lands where `gh pr checks`' bucket would put it:
    /// pass/skipping → GREEN, fail/cancel → RED, pending → wait.
    #[test]
    fn conclusion_table_matches_gh_pr_checks_buckets() {
        for (status, conclusion, want) in [
            ("completed", Some("success"), "green"),
            ("completed", Some("neutral"), "green"),
            ("completed", Some("skipped"), "green"),
            ("completed", Some("failure"), "red"),
            ("completed", Some("timed_out"), "red"),
            ("completed", Some("cancelled"), "red"),
            ("completed", Some("action_required"), "red"),
            ("completed", Some("startup_failure"), "red"),
            ("completed", Some("stale"), "red"),
            ("queued", None, "pending"),
            ("in_progress", None, "pending"),
            ("waiting", None, "pending"),
        ] {
            let r = fold(&runs(vec![run("ci", status, conclusion)]), &no_status()).unwrap();
            let got = match decide(&r, Some(&[][..]), false) {
                Decision::Green => "green",
                Decision::Red(_) => "red",
                Decision::Pending(_) => "pending",
            };
            assert_eq!(got, want, "{status}/{conclusion:?}");
        }
    }

    #[test]
    fn legacy_statuses_fold_into_the_same_rollup() {
        let status = json!({"statuses": [
            {"context": "ci/legacy", "state": "error", "target_url": "https://ci.example/1"},
            {"context": "ci/slow", "state": "pending"}
        ]});
        let r = fold(&runs(vec![run("build", "completed", Some("success"))]), &status).unwrap();
        assert_eq!(r.total, 3);
        assert_eq!(r.failing, vec!["ci/legacy"]);
        assert_eq!(r.pending, vec!["ci/slow"]);
        assert_eq!(r.failed_detail[0].url, "https://ci.example/1");
        assert_eq!(r.failed_detail[0].run_id, "-");
    }

    #[test]
    fn unreadable_payloads_are_errors_never_green() {
        let bad = [
            json!({"total_count": 1}),
            json!({"total_count": "1", "check_runs": []}),
            json!({"total_count": 1, "check_runs": [7]}),
        ];
        for b in bad {
            assert!(fold(&b, &no_status()).is_err(), "{b}");
        }
        assert!(fold(&runs(vec![]), &json!({"statuses": {}})).is_err());
    }

    #[test]
    fn default_mode_waits_for_an_unregistered_required_context() {
        let r =
            fold(&runs(vec![run("labeler", "completed", Some("success"))]), &no_status()).unwrap();
        let req = vec!["Backend".to_string()];
        assert_eq!(decide(&r, Some(req.as_slice()), false), Decision::Pending(req.clone()));
        // An unknown required set (failed lookup) is never GREEN (#10351).
        assert_eq!(decide(&r, None, false), Decision::Pending(vec![REQUIRED_UNKNOWN.into()]));
        assert_eq!(decide(&r, None, true), Decision::Pending(vec![REQUIRED_UNKNOWN.into()]));
    }

    #[test]
    fn an_unknown_required_set_still_reports_observed_failures_and_pendings() {
        let rows = vec![
            run("lint", "completed", Some("failure")),
            run("docs", "in_progress", None),
        ];
        let r = fold(&runs(rows), &no_status()).unwrap();
        assert_eq!(decide(&r, None, false), Decision::Red(vec!["lint".into()]));
        let r = fold(&runs(vec![run("docs", "in_progress", None)]), &no_status()).unwrap();
        assert_eq!(
            decide(&r, None, false),
            Decision::Pending(vec!["docs".into(), REQUIRED_UNKNOWN.into()])
        );
        // --required-only cannot tell an informational failure from a
        // required one without the set: wait, never RED or GREEN.
        let r = fold(&runs(vec![run("lint", "completed", Some("failure"))]), &no_status()).unwrap();
        assert_eq!(decide(&r, None, true), Decision::Pending(vec![REQUIRED_UNKNOWN.into()]));
    }

    /// #10351 finding 1: an EMPTY required set under `--required-only` must
    /// not make every failing / pending check vacuously GREEN.
    #[test]
    fn required_only_with_no_required_contexts_falls_back_to_every_check() {
        let none: &[String] = &[];
        let red = fold(&runs(vec![run("ci", "completed", Some("failure"))]), &no_status()).unwrap();
        assert_eq!(decide(&red, Some(none), true), Decision::Red(vec!["ci".into()]));
        let wait = fold(&runs(vec![run("ci", "queued", None)]), &no_status()).unwrap();
        assert_eq!(decide(&wait, Some(none), true), Decision::Pending(vec!["ci".into()]));
        let ok = fold(&runs(vec![run("ci", "completed", Some("success"))]), &no_status()).unwrap();
        assert_eq!(decide(&ok, Some(none), true), Decision::Green);
    }

    #[test]
    fn required_only_ignores_informational_failures() {
        let rows = vec![
            run("lint", "completed", Some("failure")),
            run("Backend", "completed", Some("success")),
        ];
        let r = fold(&runs(rows), &no_status()).unwrap();
        let req = vec!["Backend".to_string()];
        assert_eq!(decide(&r, Some(req.as_slice()), true), Decision::Green);
        assert_eq!(decide(&r, Some(req.as_slice()), false), Decision::Red(vec!["lint".into()]));
        let req = vec!["lint".to_string(), "Backend".to_string()];
        assert_eq!(decide(&r, Some(req.as_slice()), true), Decision::Red(vec!["lint".into()]));
    }

    #[test]
    fn required_only_waits_on_a_pending_or_missing_required_check() {
        let rows = vec![
            run("Backend", "in_progress", None),
            run("lint", "in_progress", None),
        ];
        let r = fold(&runs(rows), &no_status()).unwrap();
        let req = vec!["Backend".to_string(), "Gate".to_string()];
        assert_eq!(
            decide(&r, Some(req.as_slice()), true),
            Decision::Pending(vec!["Backend".into(), "Gate".into()])
        );
    }

    #[test]
    fn sentinels_and_exit_codes() {
        let f = Failed {
            name: "ci\nx".into(),
            url: "u".into(),
            run_id: "1".into(),
        };
        let red = Outcome::Red {
            sha: "abc".into(),
            failed: vec![f],
        };
        assert_eq!(red.sentinel(), "LOOM-CHECKS-RED abc ci x");
        assert_eq!(red.detail(), vec!["ci x\tu\t1"]);
        assert_eq!(red.exit_code(), 1);
        let t = Outcome::Timeout {
            sha: "abc".into(),
            pending: vec![],
        };
        assert_eq!(t.sentinel(), "LOOM-CHECKS-TIMEOUT abc -");
        assert_eq!(t.exit_code(), 2);
        assert_eq!(Outcome::Error("x".into()).exit_code(), 3);
        let moved = Outcome::HeadMoved {
            old: "a".into(),
            new: "b".into(),
        };
        assert_eq!(moved.exit_code(), 4);
        assert_eq!(Outcome::NoChecks { sha: "s".into() }.sentinel(), "LOOM-CHECKS-NONE s");
    }

    #[test]
    fn run_id_comes_from_an_actions_details_url_only() {
        assert_eq!(
            actions_run_id("https://github.com/o/r/actions/runs/123/job/4").as_deref(),
            Some("123")
        );
        assert_eq!(actions_run_id("https://ci.example/build/9"), None);
    }
}
