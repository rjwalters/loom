//! The post-merge partial-increment label reset decision (#3667, with the
//! #4569 premature-auto-close revert), a slice of the merge-pr port #8191.
//!
//! # What it decides
//!
//! When a PR merges with a NON-closing `Part of #N` / `Contributes to #N`
//! reference, issue N stays open but keeps the `loom:building` claim the
//! slice's Builder put on it. `merge-pr.sh`'s `_reset_one_partial_issue`
//! returns such an issue to the ready queue (`loom:building` -> `loom:issue`)
//! — and, when GitHub auto-closed it anyway because a stray closing keyword
//! elsewhere in the PR created a real closing link, reopens it first (#4569).
//!
//! Given a fresh read of the issue (the `gh api repos/<nwo>/issues/<n>` body
//! the shell still fetches) and the two facts the pre-merge
//! `_check_partial_increment_close_conflict` guard recorded about it, [`plan`]
//! returns the ordered [`Step`]s the retired function performed: which log
//! line, whether to reopen, whether to swap. The mutations themselves — the
//! reopen, the label swap, the two audit comments — stay in the shell, behind
//! the #4856 rate-limit-safe `forge_gh_*_rl_safe` wrappers, so the retained
//! suite's `gh` stub still records the exact calls it always did.
//!
//! # Fidelity to `jq`, deliberately
//!
//! The retired function read the issue body through three separate `jq -r`
//! filters — `has("pull_request")`, `.state // ""`, `.labels[]?.name` — and
//! each has semantics a "parse it as an issue" reading would get wrong on the
//! inputs a failed `gh api` actually produces (an error body, `{}`, or an
//! error body followed by the `|| echo '{}'` fallback — two JSON documents).
//! [`IssueView::from_json`] models each filter separately, per document, with
//! jq 1.7's error behaviour: a runtime error ends that filter's output for
//! THAT document only and jq moves on to the next, while a parse error ends
//! the stream. `tests/merge_pr_partial_reset_differential.rs` holds the model
//! against a frozen copy of the retired function on a shared corpus.
//!
//! The one place the port can render differently is a non-string JSON value
//! interpolated into a log line (a numeric `state`, say): jq preserves a
//! number's literal spelling and `serde_json` re-serialises it. That changes
//! what an operator reads for an input GitHub never sends, never a decision —
//! no non-string can equal `"open"` or `"loom:building"` either way.

use serde_json::Value;

/// The issue facts the retired function's three `jq` filters extracted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IssueView {
    /// `has("pull_request")` printed exactly `true` — the GitHub issues
    /// endpoint also serves PRs, and a PR must never be mutated here.
    pub is_pr: bool,
    /// `.state // ""`, as the shell's `$(...)` captured it (trailing newlines
    /// stripped). Compared against `open` verbatim, and shown in log lines.
    pub state: String,
    /// Some line of `.labels[]?.name` is exactly `loom:building` (the shell's
    /// `grep -qx`).
    pub building: bool,
}

/// `jq -r`'s rendering of one output value.
fn raw(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => serde_json::to_string_pretty(other).unwrap_or_default(),
    }
}

/// `$(...)`'s view of a filter's output lines: joined, trailing newlines gone.
fn captured(lines: &[String]) -> String {
    let mut s = lines.join("\n");
    while s.ends_with('\n') {
        s.pop();
    }
    s
}

/// `has("pull_request")` on one document: `null` answers false, a non-object
/// is a runtime error (no output), an object answers key presence — even for
/// `"pull_request": null`, which `has` still counts.
fn has_pull_request(doc: &Value) -> Option<String> {
    match doc {
        Value::Object(m) => Some(m.contains_key("pull_request").to_string()),
        Value::Null => Some("false".to_string()),
        _ => None,
    }
}

/// `.state // ""` on one document. `//` replaces a `null`/`false` result with
/// `""`; on a non-object, non-null document `.state` is an error and jq 1.7
/// prints nothing for that document at all.
fn state_of(doc: &Value) -> Option<String> {
    match doc {
        Value::Object(m) => Some(match m.get("state") {
            None | Some(Value::Null | Value::Bool(false)) => String::new(),
            Some(v) => raw(v),
        }),
        Value::Null => Some(String::new()),
        _ => None,
    }
}

/// `.labels[]?.name` on one document. The `?` suppresses a failure to iterate
/// (no `labels`, a non-iterable, a non-object document), but NOT the `.name`
/// applied to each element: the first element that is neither an object nor
/// `null` ends this document's output, keeping whatever preceded it. An object
/// `labels` iterates its values in document order (`preserve_order`).
fn label_names(doc: &Value) -> Vec<String> {
    let elems: Vec<&Value> = match doc {
        Value::Object(m) => match m.get("labels") {
            Some(Value::Array(a)) => a.iter().collect(),
            Some(Value::Object(o)) => o.values().collect(),
            _ => Vec::new(),
        },
        _ => Vec::new(),
    };
    let mut out = Vec::new();
    for e in elems {
        match e {
            Value::Object(m) => out.push(m.get("name").map_or_else(|| "null".to_string(), raw)),
            Value::Null => out.push("null".to_string()),
            _ => break,
        }
    }
    out
}

impl IssueView {
    /// Read the issue body the way the retired `jq` pipelines did.
    ///
    /// `body` is a stream of zero or more JSON documents; parsing stops at the
    /// first malformed one, keeping every document before it — jq's behaviour
    /// on `<error body>{}` and on a truncated read alike.
    #[must_use]
    pub fn from_json(body: &str) -> Self {
        let docs: Vec<Value> = serde_json::Deserializer::from_str(body)
            .into_iter::<Value>()
            .map_while(Result::ok)
            .collect();

        let has: Vec<String> = docs.iter().filter_map(has_pull_request).collect();
        let state: Vec<String> = docs.iter().filter_map(state_of).collect();
        let labels: Vec<String> = docs.iter().flat_map(label_names).collect();

        Self {
            is_pr: captured(&has) == "true",
            state: captured(&state),
            building: captured(&labels).split('\n').any(|l| l == "loom:building"),
        }
    }
}

/// One thing the shell does, in order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    /// Log through the script's `info`.
    Info(String),
    /// Log through the script's `warning`.
    Warning(String),
    /// Reopen the issue (#4569) and post the premature-close audit comment.
    /// If the reopen FAILS the shell warns and stops — nothing after this
    /// step runs, because a label swap on a still-closed issue returns
    /// nothing to any queue.
    Reopen,
    /// `loom:building` -> `loom:issue`, plus the partial-increment comment.
    Swap,
}

/// What the pre-merge `_check_partial_increment_close_conflict` guard
/// recorded about this issue.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PreMerge {
    /// A closing reference to the issue was found on this PR (body, commit
    /// messages, or sidebar link) — `$PARTIAL_CONFLICT_ISSUES` membership.
    pub conflicted: bool,
    /// The issue was open when the guard ran — `$PARTIAL_OPEN_BEFORE_MERGE`
    /// membership.
    pub open_before_merge: bool,
}

/// The ordered steps for one partial-increment reference.
///
/// `issue`, `pr` and `repo` appear only in log text. An empty plan (a PR
/// masquerading as an issue) is a silent skip, exactly as before.
#[must_use]
pub fn plan(issue: &str, pr: &str, repo: &str, view: &IssueView, pre: PreMerge) -> Vec<Step> {
    if view.is_pr {
        return Vec::new();
    }
    let shown = if view.state.is_empty() {
        "unknown"
    } else {
        &view.state
    };

    let mut steps = Vec::new();
    if view.state != "open" {
        if pre.conflicted {
            steps.push(Step::Warning(format!(
                "Partial-increment reset: issue #{issue} was auto-closed by PR #{pr}'s merge \
despite its non-closing `Part of`/`Contributes to` reference (a closing reference to #{issue} was \
detected pre-merge) — reopening (#4569)"
            )));
            steps.push(Step::Reopen);
        } else if pre.open_before_merge {
            steps.push(Step::Warning(format!(
                "Partial-increment reset: issue #{issue} was open before PR #{pr} merged and is \
now closed (state='{shown}'), but no closing reference to it was detected on this PR — NOT \
reopening automatically (it may be a deliberate close). If this was a premature auto-close, \
reopen it with: gh issue reopen {issue} --repo {repo}"
            )));
            return steps;
        } else {
            steps.push(Step::Info(format!(
                "Partial-increment reset: issue #{issue} is not open (state='{shown}') — skipping"
            )));
            return steps;
        }
    }

    // Read from the SAME pre-reopen body as the state, as the retired
    // function did: a reopen does not change the labels.
    if view.building {
        steps.push(Step::Info(format!(
            "Partial-increment reset: PR #{pr} merged as a partial slice of #{issue}; returning it \
to the ready queue"
        )));
        steps.push(Step::Swap);
    } else {
        steps.push(Step::Info(format!(
            "Partial-increment reset: issue #{issue} is not loom:building — skipping (idempotent)"
        )));
    }
    steps
}

/// Render steps as the shell wrapper's line protocol: `INFO<TAB>text`,
/// `WARNING<TAB>text`, or a bare `REOPEN` / `SWAP`.
///
/// A message spanning several lines (a multi-document `state`, say) is split
/// with every line carrying the message's own level, so no continuation line
/// can reach the shell's `read` without a tab and be misread as an action.
#[must_use]
pub fn render(steps: &[Step]) -> String {
    let mut out = String::new();
    for step in steps {
        match step {
            Step::Info(m) | Step::Warning(m) => {
                let level = if matches!(step, Step::Info(_)) {
                    "INFO"
                } else {
                    "WARNING"
                };
                for line in m.split('\n') {
                    out.push_str(level);
                    out.push('\t');
                    out.push_str(line);
                    out.push('\n');
                }
            }
            Step::Reopen => out.push_str("REOPEN\n"),
            Step::Swap => out.push_str("SWAP\n"),
        }
    }
    out
}

#[cfg(test)]
mod tests;
