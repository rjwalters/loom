//! The pre-merge partial-increment close-conflict decision (#4569, with
//! #4595's commit-message signal and #5234's declaration quoting), a slice of
//! the merge-pr port #8191.
//!
//! # What it decides
//!
//! A PR that declares itself only a PART of an issue (`Part of #N` /
//! `Contributes to #N`) must leave that issue open. GitHub closes it anyway
//! when a closing keyword immediately followed by `#N` appears anywhere else
//! the merge honours: the PR body, one of the PR's commit messages (the squash
//! message is composed from them), or a Development-sidebar link. `merge-pr.sh`'s
//! `_check_partial_increment_close_conflict` runs before the merge and records
//! two facts per declared issue, which the post-merge
//! [`super::partial_reset`] pass consumes:
//!
//! - **open before merge** — the issue was open when the guard ran, so a close
//!   observed afterwards happened in the merge window;
//! - **conflicted** — a closing reference to it exists, so that close is
//!   attributable to THIS merge and is reverted.
//!
//! Both sets being wrong is silent: a missed `conflicted` leaves an unfinished
//! issue closed, a spurious one reopens a correctly-closed issue. The retired
//! shell reached them through a `printf | grep -E | sort -un` union of three
//! ref lists, two `jq -r` reads per issue, a `grep -qx` membership test, and a
//! three-way source attribution for the warning — all inside a loop that a
//! `|| true` caller ran with `errexit` off.
//!
//! [`plan`] makes the whole per-issue ladder one function. The forge reads —
//! the commit messages, `forge_pr_close_targets`, and each issue's fresh
//! `gh api` body — stay in the shell, which hands them over in one NUL-framed
//! [`Frame`] on stdin.
//!
//! # Fidelity
//!
//! The per-issue `jq` reads are [`IssueView::from_json`], the model
//! [`super::partial_reset`] already holds against the same endpoint's failure
//! shapes. The closing-reference union reproduces `grep -E '^[0-9]+$' | sort
//! -un`: body refs, then commit-message refs, then the sidebar lines, keeping
//! the FIRST spelling of each numeric value, and membership is an exact string
//! match (`grep -qx`) — so a zero-padded sidebar line never matches, exactly as
//! before. `tests/merge_pr_partial_conflict_differential.rs` holds the port
//! against a frozen copy of the retired function.

use std::collections::HashSet;

use super::partial_reset::IssueView;
use super::refs;

/// The terminal line of every successful plan. The shell requires it: a
/// daemon predating this verb, or one that died mid-plan, prints no such line
/// and the merge is refused rather than read as "no conflicts".
pub const DONE: &str = "LOOM-PARTIAL-CONFLICT-DONE";

/// The forge reads the shell performed, as it captured them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Frame {
    /// The PR body (`jq -r '.body // ""'`, trailing newlines stripped).
    pub body: String,
    /// `$(_pr_commit_messages)` — every commit message, concatenated.
    pub commit_messages: String,
    /// `$(forge_pr_close_targets …)` — GitHub's `closingIssuesReferences`,
    /// one per line, or empty under GraphQL exhaustion.
    pub graphql_close_refs: String,
    /// `(issue number, captured gh api body)` for each declared issue.
    pub issues: Vec<(String, String)>,
}

impl Frame {
    /// Parse `printf '%s\0' body commits graphql [n json]...`. A bash string
    /// cannot contain NUL, so the framing is lossless — and unlike argv it has
    /// no size limit for a PR body that runs to tens of kilobytes. `None` when
    /// the frame is short or its issue fields are unpaired.
    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        let mut fields: Vec<&str> = raw.split('\0').collect();
        // Every field is NUL-terminated, so the split leaves one empty tail.
        if fields.pop() != Some("") || fields.len() < 3 || (fields.len() - 3) % 2 != 0 {
            return None;
        }
        let issues = fields[3..]
            .chunks(2)
            .map(|p| (p[0].to_string(), p[1].to_string()))
            .collect();
        Some(Self {
            body: fields[0].to_string(),
            commit_messages: fields[1].to_string(),
            graphql_close_refs: fields[2].to_string(),
            issues,
        })
    }
}

/// One thing the shell does, in order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    /// Append to `$PARTIAL_OPEN_BEFORE_MERGE`.
    Open(u64),
    /// Append to `$PARTIAL_CONFLICT_ISSUES`.
    Conflict(u64),
    /// Log through the script's `warning`.
    Warning(String),
}

/// The closing references GitHub will honour on merge, as the retired
/// `printf '%s\n%s\n%s\n' … | grep -E '^[0-9]+$' | sort -un` left them.
fn close_ref_lines(body: &str, commits: &str, graphql: &str) -> Vec<String> {
    let mut seen: HashSet<String> = HashSet::new();
    let mut out = Vec::new();
    let text_refs = refs::closing_refs(body)
        .into_iter()
        .chain(refs::closing_refs(commits))
        .map(|n| n.to_string());
    for line in text_refs.chain(graphql.split('\n').map(str::to_string)) {
        if line.is_empty() || !line.bytes().all(|b| b.is_ascii_digit()) {
            continue;
        }
        let key = line.trim_start_matches('0');
        let key = if key.is_empty() { "0" } else { key };
        if seen.insert(key.to_string()) {
            out.push(line);
        }
    }
    out
}

/// The ordered steps for one PR. Empty when the body declares no partial
/// increment (the shell returned before any of this ran).
#[must_use]
pub fn plan(frame: &Frame, pr: &str, dry_run: bool) -> Vec<Step> {
    // The shell fed every text through `printf '%s\n'`.
    let body = format!("{}\n", frame.body);
    let commits = format!("{}\n", frame.commit_messages);
    let partial = refs::partial_increment_refs(&body);
    if partial.is_empty() {
        return Vec::new();
    }
    let close = close_ref_lines(&body, &commits, &frame.graphql_close_refs);
    let dr = if dry_run { "[dry-run] " } else { "" };

    let mut steps = Vec::new();
    for n in partial {
        let key = n.to_string();
        let raw = frame
            .issues
            .iter()
            .find(|(k, _)| *k == key)
            .map_or("", |(_, v)| v.as_str());
        // `echo "$issue_json" | jq …`: the captured body plus echo's newline.
        let view = IssueView::from_json(&format!("{raw}\n"));
        // A PR served by the issues endpoint, or an issue that is not open
        // right now: nothing this merge does can be blamed on it.
        if view.is_pr || view.state != "open" {
            continue;
        }
        steps.push(Step::Open(n));
        if !close.contains(&key) {
            continue;
        }
        steps.push(Step::Conflict(n));

        let body_offending = refs::closing_ref_snippets(&body, n);
        let commit_offending = refs::closing_ref_snippets(&commits, n);
        let partial_offending = refs::partial_increment_ref_snippets(&body, n);
        // Name the source: the operator remedy differs per source.
        let (first, second) = if !body_offending.is_empty() {
            (
                format!(
                    "{dr}Partial-increment conflict (#4569): PR #{pr} declares a NON-closing \
`Part of`/`Contributes to` reference to #{n} (\"{partial_offending}\"), but its body ALSO carries \
a closing reference to #{n} (\"{body_offending}\") — GitHub honors a closing keyword ANYWHERE in \
the body, so merging this PR WILL close #{n} against the declared intent."
                ),
                format!(
                    "  {dr}merge-pr.sh would reopen #{n} immediately after the merge. To avoid the \
close/reopen flicker entirely, edit the PR body so no closing keyword is immediately followed by \
`#{n}` (e.g. write `close the issue` or `close issue #{n}` instead of `close #{n}`), then re-run \
this merge."
                ),
            )
        } else if !commit_offending.is_empty() {
            (
                format!(
                    "{dr}Partial-increment conflict (#4595): PR #{pr} declares a NON-closing \
`Part of`/`Contributes to` reference to #{n} (\"{partial_offending}\"), but a closing keyword in \
a commit message of this PR references #{n} (\"{commit_offending}\") — this merge squashes \
without overriding the commit message, so GitHub composes the squash message from these commits \
and merging WILL close #{n} against the declared intent."
                ),
                format!(
                    "  {dr}merge-pr.sh would reopen #{n} immediately after the merge. To avoid the \
close/reopen flicker entirely, reword the offending commit message (`git commit --amend` / `git \
rebase -i` + force-push) so no closing keyword is immediately followed by `#{n}`, then re-run \
this merge."
                ),
            )
        } else {
            (
                format!(
                    "{dr}Partial-increment conflict (#4569): PR #{pr} declares a NON-closing \
`Part of`/`Contributes to` reference to #{n} (\"{partial_offending}\"), but GitHub reports #{n} \
as a closing target of this PR (no closing keyword found in the body or commit messages — most \
likely a Development-sidebar link), so merging this PR WILL close #{n} against the declared \
intent."
                ),
                format!(
                    "  {dr}merge-pr.sh would reopen #{n} immediately after the merge. To avoid the \
close/reopen flicker entirely, unlink #{n} from this PR's Development sidebar, then re-run this \
merge."
                ),
            )
        };
        steps.push(Step::Warning(first));
        steps.push(Step::Warning(second));
    }
    steps
}

/// Render steps as the shell wrapper's line protocol — `OPEN<TAB>n`,
/// `CONFLICT<TAB>n`, `WARNING<TAB>text` — terminated by [`DONE`]. A
/// multi-line message is split with every line carrying `WARNING`, so no
/// continuation line can reach the shell's `read` and be misread as a record.
#[must_use]
pub fn render(steps: &[Step]) -> String {
    let mut out = String::new();
    for step in steps {
        match step {
            Step::Open(n) => out.push_str(&format!("OPEN\t{n}\n")),
            Step::Conflict(n) => out.push_str(&format!("CONFLICT\t{n}\n")),
            Step::Warning(m) => {
                for line in m.split('\n') {
                    out.push_str("WARNING\t");
                    out.push_str(line);
                    out.push('\n');
                }
            }
        }
    }
    out.push_str(DONE);
    out.push('\n');
    out
}

#[cfg(test)]
mod tests;
