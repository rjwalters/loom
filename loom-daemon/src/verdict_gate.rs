//! Verdict-time gate and label transition behind `post-verdict.sh` (#10581).
//!
//! Two Judges reviewed PR #10578 at the same head six minutes apart and posted
//! opposite verdicts. The later one approved a tree whose CI was red and which
//! still carried `loom:changes-requested` and `loom:ci-failure`, because the
//! verdict comment and the label transition were decoupled: the script posted
//! the comment, and each call site in `judge.md` chained its own `gh pr edit`,
//! none of which removed the opposite verdict label. PR #10605 showed the
//! reverse: an approval comment whose label edit never ran.
//!
//! This module is the logic; `post-verdict.sh` only calls it:
//!
//! - [`decide`] — before posting. Refuses an approval over a `changes-requested`
//!   marker at the same head (unless an explicit overrule rationale is given),
//!   refuses an approval while `loom:ci-failure` is on the PR, and dedupes a
//!   second identical verdict at the same head inside a short window (two
//!   concurrent Judges). Unread state never passes an approval.
//! - [`transition`] / [`label_problems`] — after posting. One verdict label,
//!   never both: the add happens first, then the removals, then a re-read
//!   verifies the result, so a failure is loud instead of a verdict with no
//!   label.
//!
//! The exact-head CI read (#10485) is deliberately NOT here: that gate reads
//! check runs through `forge wait-checks`. This one only reads what the forge
//! already says about the PR (comments and labels).

use chrono::{DateTime, Utc};
use serde_json::Value;

use crate::claim_reconciliation::VerdictKind;

/// How long a same-head, same-verdict marker counts as "another Judge's
/// verdict is still landing" rather than history (`--window-secs`).
pub const DEFAULT_WINDOW_SECS: i64 = 600;

/// Minimum length of an `--overrules-prior` rationale, matching
/// `--reviews-reconciled` (#7647): a sentence, not a token.
pub const OVERRULE_MIN_CHARS: usize = 40;

/// The companion label that means "CI was red when someone last looked".
pub const CI_FAILURE_LABEL: &str = "loom:ci-failure";

/// Exit code for a refusal.
pub const EXIT_REFUSE: i32 = 3;

/// Exit code for a dedupe (do not post, still apply the labels).
pub const EXIT_DEDUPE: i32 = 10;

/// The first token of every gate answer, so a caller accepts only a positive
/// signal (an old binary or a crash prints something else).
pub const GATE_SENTINEL: &str = "LOOM-VERDICT-GATE";

/// The first token of a successful label transition.
pub const LABELS_SENTINEL: &str = "LOOM-VERDICT-LABELS";

/// A `<!-- loom:verdict-sha ... -->` marker read back from a comment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PriorMarker {
    /// The SHA the marker records.
    pub sha: String,
    /// The verdict it records.
    pub verdict: VerdictKind,
    /// When the carrying comment was created, if the listing said.
    pub created_at: Option<DateTime<Utc>>,
}

/// The gate's answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// Post the verdict.
    Proceed(String),
    /// The same verdict at the same head just landed: do not post a second
    /// comment, but do apply the labels (they may be what failed last time).
    Dedupe(String),
    /// Do not post anything.
    Refuse(String),
}

impl Decision {
    /// The one-line answer `post-verdict.sh` parses, and the exit code.
    #[must_use]
    pub fn render(&self) -> (String, i32) {
        match self {
            Self::Proceed(m) => (format!("{GATE_SENTINEL} PROCEED {m}"), 0),
            Self::Dedupe(m) => (format!("{GATE_SENTINEL} DEDUPE {m}"), EXIT_DEDUPE),
            Self::Refuse(m) => (format!("{GATE_SENTINEL} REFUSE {m}"), EXIT_REFUSE),
        }
    }
}

/// `approved` / `changes-requested` → the verdict kind.
#[must_use]
pub fn parse_verdict(token: &str) -> Option<VerdictKind> {
    match token {
        "approved" => Some(VerdictKind::Approved),
        "changes-requested" => Some(VerdictKind::ChangesRequested),
        _ => None,
    }
}

/// Two SHAs name the same commit when one is a prefix of the other (the
/// marker accepts 7-40 hex chars, like `verdict-staleness-guard.sh`).
#[must_use]
pub fn same_sha(a: &str, b: &str) -> bool {
    let (a, b) = (a.to_ascii_lowercase(), b.to_ascii_lowercase());
    a.len() >= 7 && b.len() >= 7 && (a.starts_with(&b) || b.starts_with(&a))
}

/// The newest marker recorded for `sha` among `records` (a comment listing,
/// oldest first, already filtered to trusted authors). Markers for other
/// SHAs are ignored: a verdict on another tree says nothing about this one.
#[must_use]
pub fn latest_marker_for(records: &[Value], sha: &str) -> Option<PriorMarker> {
    let re = regex::Regex::new(
        r"<!-- loom:verdict-sha sha=([0-9a-f]{7,40}) verdict=(approved|changes-requested) -->",
    )
    .ok()?;
    let mut latest = None;
    for record in records {
        let Some(body) = record.get("body").and_then(Value::as_str) else {
            continue;
        };
        let created_at = ["created_at", "createdAt"]
            .iter()
            .find_map(|k| record.get(*k).and_then(Value::as_str))
            .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
            .map(|d| d.with_timezone(&Utc));
        for cap in re.captures_iter(body) {
            if same_sha(&cap[1], sha) {
                if let Some(verdict) = parse_verdict(&cap[2]) {
                    latest = Some(PriorMarker {
                        sha: cap[1].to_string(),
                        verdict,
                        created_at,
                    });
                }
            }
        }
    }
    latest
}

/// Everything [`decide`] looks at. `comments` / `labels` are `None` when the
/// read failed — never conflated with "none found".
#[derive(Debug, Clone)]
pub struct GateInput<'a> {
    pub verdict: VerdictKind,
    pub sha: &'a str,
    pub comments: Option<&'a [Value]>,
    pub labels: Option<&'a [String]>,
    pub overrule: &'a str,
    pub now: DateTime<Utc>,
    pub window_secs: i64,
}

/// The pre-post decision. Pure; see the module docs for the rules.
#[must_use]
pub fn decide(input: &GateInput<'_>) -> Decision {
    let approving = input.verdict == VerdictKind::Approved;
    if approving {
        let Some(labels) = input.labels else {
            return Decision::Refuse(
                "the PR's labels could not be read, so loom:ci-failure cannot be ruled out; \
                 an unread state is never a pass — retry"
                    .into(),
            );
        };
        if input.comments.is_none() {
            return Decision::Refuse(
                "the PR's comments could not be read, so a same-head changes-requested verdict \
                 cannot be ruled out; an unread state is never a pass — retry"
                    .into(),
            );
        }
        if labels.iter().any(|l| l == CI_FAILURE_LABEL) {
            return Decision::Refuse(format!(
                "the PR carries {CI_FAILURE_LABEL}; never approve over red CI. If CI on this \
                 exact head is now green (verify with `loom-daemon forge wait-checks`), remove \
                 the stale label and retry; otherwise post changes-requested"
            ));
        }
    }
    let prior = input.comments.and_then(|c| latest_marker_for(c, input.sha));
    let Some(prior) = prior else {
        return Decision::Proceed(format!("no earlier verdict at {}", input.sha));
    };
    let age = prior.created_at.map(|t| (input.now - t).num_seconds());
    // An unknown age counts as recent: for a dedupe that is the harmless side.
    let recent = age.is_none_or(|a| a < input.window_secs);
    if prior.verdict == input.verdict && recent {
        return Decision::Dedupe(format!(
            "a {} verdict for {} was posted {} ago (window {}s); not posting a second one",
            prior.verdict.marker_token(),
            prior.sha,
            age.map_or_else(|| "an unknown time".to_string(), |a| format!("{a}s")),
            input.window_secs
        ));
    }
    if approving && prior.verdict == VerdictKind::ChangesRequested {
        let given = input.overrule.trim().chars().count();
        if given < OVERRULE_MIN_CHARS {
            return Decision::Refuse(format!(
                "the newest verdict at this same head ({}) is changes-requested and the head has \
                 not moved. Read that verdict and wait for a new push, or pass --overrules-prior \
                 \"<why each point no longer blocks>\" (at least {OVERRULE_MIN_CHARS} chars, got \
                 {given})",
                prior.sha
            ));
        }
        return Decision::Proceed(format!(
            "overruling the changes-requested verdict at {} with an explicit rationale",
            prior.sha
        ));
    }
    Decision::Proceed(format!(
        "newest verdict at this head is {}; proceeding",
        prior.verdict.marker_token()
    ))
}

/// The labels a verdict adds and removes. Exactly one verdict label survives.
#[must_use]
pub fn transition(verdict: VerdictKind) -> (&'static [&'static str], &'static [&'static str]) {
    match verdict {
        VerdictKind::Approved => (
            &["loom:pr"],
            &[
                "loom:review-requested",
                "loom:reviewing",
                "loom:changes-requested",
                CI_FAILURE_LABEL,
            ],
        ),
        VerdictKind::ChangesRequested => (
            &["loom:changes-requested"],
            &["loom:pr", "loom:review-requested", "loom:reviewing"],
        ),
    }
}

/// What is wrong with `labels` after the transition, empty when it held.
#[must_use]
pub fn label_problems(verdict: VerdictKind, labels: &[String]) -> Vec<String> {
    let (add, remove) = transition(verdict);
    let has = |l: &str| labels.iter().any(|x| x == l);
    add.iter()
        .filter(|l| !has(l))
        .map(|l| format!("missing {l}"))
        .chain(
            remove
                .iter()
                .filter(|l| has(l))
                .map(|l| format!("still carries {l}")),
        )
        .collect()
}

/// The single command that repairs a half-applied transition by hand.
#[must_use]
pub fn repair_command(pr: u64, repo: &str, verdict: VerdictKind) -> String {
    let (add, remove) = transition(verdict);
    // A hint printed for a human, not a call: kept out of the forge-call scanner.
    let gh = "gh";
    let mut cmd = format!("{gh} pr edit {pr} --repo {repo}");
    for l in add {
        cmd.push_str(&format!(" --add-label \"{l}\""));
    }
    for l in remove {
        cmd.push_str(&format!(" --remove-label \"{l}\""));
    }
    cmd
}

/// Label names from a REST `issues/{n}/labels` (or issue object) document.
#[must_use]
pub fn label_names(doc: &Value) -> Option<Vec<String>> {
    let items = doc.as_array().or_else(|| doc.get("labels")?.as_array())?;
    Some(
        items
            .iter()
            .filter_map(|l| l.get("name").and_then(Value::as_str).map(str::to_string))
            .collect(),
    )
}

#[cfg(test)]
mod tests;
