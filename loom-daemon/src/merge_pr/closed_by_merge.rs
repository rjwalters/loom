//! "Did THIS merge close the issue?" (#8942) — the attribution Champion
//! Step 4's negated-reference reopen (#1057) was missing.
//!
//! # What it decides
//!
//! Step 4 reopens an issue when the merged PR references it only in negated
//! form ("does not fix #N"), because the forge's closing-reference parser is
//! not negation-aware and closes it anyway. Before #8942 the reopen fired on
//! `state = CLOSED` alone, so an issue that was ALREADY closed — by an earlier
//! PR, or by a person — was reopened with a comment blaming a merge that never
//! touched it. That is reachable on GitHub, not just in theory: its
//! `closingIssuesReferences` keeps listing an issue that was closed before the
//! merge (measured on this repo, 2026-10-10: 86 of 4,060 merged-PR/close-target
//! pairs, e.g. PR #10265 merged 954 s after #10264 was closed by hand).
//!
//! [`decide`] answers from facts the caller fetched — this module makes no
//! forge call — and the caller keeps the only mutation (`gh issue reopen`).
//!
//! # The evidence, in the order it is trusted
//!
//! 1. **`closedAt` earlier than this PR's `mergedAt` is a hard no**, whatever
//!    the closer says. GitHub can append a `ClosedEvent` naming the PR to an
//!    issue that was already closed while leaving `closedAt` at the earlier
//!    close (measured: #3251 closed 548 s before PR #3277 merged, yet its
//!    latest `ClosedEvent.closer` is PR #3277). Closer identity alone would
//!    have reopened exactly the issue this gate exists to leave alone. No
//!    genuine merge-driven close in the sample had a negative lag.
//! 2. **The latest close event's closer**, which only GraphQL exposes (REST
//!    `issues/N/events` and `timeline` carry none for a merge-driven close):
//!    this PR, or a commit that is this PR's merge commit or one of its own
//!    commits (a merge-commit merge is attributed to the branch commit whose
//!    message carries the keyword — 87 closes in the sample), is a yes. A
//!    different PR is a no. An event with a `null` closer is a no as well: the
//!    forge attributed the close and named no PR or commit, i.e. somebody
//!    closed it directly, and a deliberate close is not Step 4's to undo.
//! 3. **Timestamps, only when no closer is available** (GraphQL unavailable,
//!    a Gitea-shaped REST read, a commit closer this PR's commit list does not
//!    contain — a rebase merge, or more commits than the query pages): a close
//!    at most [`TIED_WINDOW_SECS`] after the merge is a yes. 3,936 of the 3,974
//!    at-or-after-merge closes sampled landed within that window, and all but
//!    3 of those were this merge's own (the 3 were another PR merging the same
//!    second, which rule 2 catches whenever a closer is readable). One of them
//!    — #8101, 1 s after PR #8102 — has an empty timeline on GitHub itself, so
//!    this rule is not Gitea-only. Later than the window is
//!    [`Verdict::Unattributed`]: not reopened, said loudly.
//!
//! # Fail direction
//!
//! The reopen is a visible write on somebody else's issue, and skipping it is
//! always recoverable (the issue stays closed exactly as the forge left it;
//! an operator reopens by hand). So every input this module cannot fully read
//! is [`Verdict::Unanswered`], never a yes — and the CLI gives it an exit code
//! distinct from both answers so the caller cannot mistake "no facts" for "no".

use chrono::{DateTime, FixedOffset};
use serde_json::Value;

/// How long after the merge a close with NO readable closer is still tied to
/// it. Merge-driven closes trail `mergedAt` by 0-2 s (99.6% within 4 s on the
/// 2026-10-10 sample); the manual closes that followed a merge in that sample
/// started at 31 s.
pub const TIED_WINDOW_SECS: i64 = 5;

/// The GraphQL document whose response [`Facts::from_json`] reads. Built here,
/// beside its parser, so the two cannot drift; the role prompt asks the verb
/// for it (`--print-query`) rather than carrying a copy. The issue and PR
/// numbers are inlined — they are integers, so nothing forge-controlled reaches
/// the document — which leaves the caller two variables to supply: `$o` owner
/// and `$r` repo (`gh api graphql -F o='{owner}' -F r='{repo}'`).
#[must_use]
pub fn query(issue: u64, pr: u64) -> String {
    format!(
        "query($o:String!,$r:String!){{repository(owner:$o,name:$r){{\
         pullRequest(number:{pr}){{number mergedAt mergeCommit{{oid}} commits(last:100){{nodes{{commit{{oid}}}}}}}} \
         issue(number:{issue}){{number state closedAt timelineItems(last:1,itemTypes:CLOSED_EVENT){{nodes{{\
         ... on ClosedEvent{{closer{{__typename ... on PullRequest{{number}} ... on Commit{{oid}}}}}}}}}}}}}}}}"
    )
}

/// Who the forge says performed the issue's latest close.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Closer {
    /// No close event was readable at all — REST, Gitea, a failed GraphQL
    /// read, an empty timeline. Only timestamps can answer.
    #[default]
    Unavailable,
    /// A close event was read and its closer is `null`: closed directly by a
    /// user or an API client, not through a PR or a commit.
    Direct,
    /// Closed through pull request `number`.
    PullRequest(u64),
    /// Closed through the commit `oid` (its message carried the keyword).
    Commit(String),
    /// A closer of a type this module does not model (e.g. `ProjectV2`).
    Other(String),
}

/// The facts [`decide`] needs, as far as the input supplied them.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Facts {
    /// The issue's state, lower-cased (`open` / `closed`).
    pub state: Option<String>,
    /// The issue's `closedAt` / `closed_at`, verbatim.
    pub closed_at: Option<String>,
    /// The PR's `mergedAt` / `merged_at`, verbatim.
    pub merged_at: Option<String>,
    /// The latest close event's closer.
    pub closer: Closer,
    /// The PR's merge commit plus (GraphQL only) its own commits.
    pub pr_commits: Vec<String>,
    /// The issue number the input itself declared, when it declared one.
    pub issue_number: Option<u64>,
    /// The PR number the input itself declared, when it declared one.
    pub pr_number: Option<u64>,
}

fn text(v: Option<&Value>) -> Option<String> {
    v.and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// Fill `slot` only when it is still empty: the first document to state a
/// fact wins, so a trailing fallback read cannot overwrite a richer one.
fn fill<T>(slot: &mut Option<T>, v: Option<T>) {
    if slot.is_none() {
        *slot = v;
    }
}

impl Facts {
    /// Read the facts out of `input`: any number of concatenated JSON
    /// documents, each one of
    ///
    /// - the [`query`] GraphQL response (`data.repository.{issue,pullRequest}`),
    /// - a REST pull object (it has a top-level `merged_at` key), or
    /// - a REST issue object (`state` / `closed_at`) — GitHub's and Gitea's
    ///   spell these fields the same way.
    ///
    /// Documents carrying none of those (an error body ahead of a fallback
    /// read, say) are skipped. Unparseable input yields empty facts, which
    /// [`decide`] reports as [`Verdict::Unanswered`].
    #[must_use]
    pub fn from_json(input: &str) -> Self {
        let mut docs: Vec<Value> = Vec::new();
        let mut stream = serde_json::Deserializer::from_str(input).into_iter::<Value>();
        let mut clean = true;
        for doc in &mut stream {
            match doc {
                Ok(v) => docs.push(v),
                Err(_) => {
                    clean = false;
                    break;
                }
            }
        }
        if !clean {
            // A non-JSON line (an HTML error page, a stray log line) ends the
            // stream; `gh api` prints one document per line, so recover the
            // rest line by line rather than discarding a good fallback read.
            docs.extend(
                input
                    .lines()
                    .filter_map(|l| serde_json::from_str::<Value>(l).ok()),
            );
        }

        let mut facts = Self::default();
        for doc in &docs {
            if let Some(repo) = doc.pointer("/data/repository").filter(|r| r.is_object()) {
                facts.absorb_graphql(repo);
            } else if doc.get("merged_at").is_some() {
                fill(&mut facts.merged_at, text(doc.get("merged_at")));
                fill(&mut facts.pr_number, doc.get("number").and_then(Value::as_u64));
                facts.pr_commits.extend(text(doc.get("merge_commit_sha")));
            } else if doc.get("closed_at").is_some() || doc.get("state").is_some() {
                fill(&mut facts.state, text(doc.get("state")).map(|s| s.to_lowercase()));
                fill(&mut facts.closed_at, text(doc.get("closed_at")));
                fill(&mut facts.issue_number, doc.get("number").and_then(Value::as_u64));
            }
        }
        facts
    }

    fn absorb_graphql(&mut self, repo: &Value) {
        if let Some(pr) = repo.get("pullRequest").filter(|v| v.is_object()) {
            fill(&mut self.merged_at, text(pr.get("mergedAt")));
            fill(&mut self.pr_number, pr.get("number").and_then(Value::as_u64));
            self.pr_commits.extend(text(pr.pointer("/mergeCommit/oid")));
            if let Some(nodes) = pr.pointer("/commits/nodes").and_then(Value::as_array) {
                self.pr_commits
                    .extend(nodes.iter().filter_map(|n| text(n.pointer("/commit/oid"))));
            }
        }
        if let Some(issue) = repo.get("issue").filter(|v| v.is_object()) {
            fill(&mut self.state, text(issue.get("state")).map(|s| s.to_lowercase()));
            fill(&mut self.closed_at, text(issue.get("closedAt")));
            fill(&mut self.issue_number, issue.get("number").and_then(Value::as_u64));
            let latest = issue
                .pointer("/timelineItems/nodes")
                .and_then(Value::as_array)
                .and_then(|n| n.last());
            // Only a node that actually carries a `closer` key is a close
            // event that was read; `{}` (a fragment that matched nothing) and
            // an empty list stay `Unavailable`.
            if let Some(closer) = latest.and_then(|n| n.get("closer")) {
                if self.closer == Closer::Unavailable {
                    self.closer = parse_closer(closer);
                }
            }
        }
    }
}

fn parse_closer(closer: &Value) -> Closer {
    if closer.is_null() {
        return Closer::Direct;
    }
    let kind = closer
        .get("__typename")
        .and_then(Value::as_str)
        .unwrap_or("");
    match kind {
        "PullRequest" => closer
            .get("number")
            .and_then(Value::as_u64)
            .map_or_else(|| Closer::Other(kind.to_string()), Closer::PullRequest),
        "Commit" => {
            text(closer.get("oid")).map_or_else(|| Closer::Other(kind.to_string()), Closer::Commit)
        }
        _ => Closer::Other(kind.to_string()),
    }
}

/// Why a close is attributed to this merge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Evidence {
    /// The latest close event names this PR.
    CloserIsPr,
    /// The latest close event names this PR's merge commit or one of its own.
    CloserIsPrCommit,
    /// No closer was readable; the close landed within [`TIED_WINDOW_SECS`]
    /// of the merge.
    TimestampTied,
}

/// The answer to "did this merge close the issue?".
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// Yes — this merge closed it.
    ThisMerge(Evidence),
    /// The issue is not closed; there is nothing to attribute.
    NotClosed,
    /// It was closed `secs` seconds before this PR merged.
    ClosedBeforeMerge { secs: i64 },
    /// Its latest close is attributed to a different pull request.
    OtherPullRequest(u64),
    /// Its latest close was direct (a `null` closer) or by a closer type
    /// that is not this PR.
    OtherCloser(String),
    /// No closer was readable and the close landed `secs` seconds after the
    /// merge — outside the window that ties it to this merge.
    Unattributed { secs: i64 },
    /// The facts were missing, unparseable, or about a different issue/PR.
    Unanswered(String),
}

impl Verdict {
    /// The process exit code: 0 = this merge closed it, 1 = it did not,
    /// 3 = could not answer. 2 is deliberately unused — it is what `clap`
    /// exits with on a binary that predates this verb, and the caller treats
    /// every code other than 0 and 1 as "no answer".
    #[must_use]
    pub fn exit_code(&self) -> i32 {
        match self {
            Verdict::ThisMerge(_) => 0,
            Verdict::Unanswered(_) => 3,
            _ => 1,
        }
    }

    /// The stable wire token, the second field of the output line.
    #[must_use]
    pub fn token(&self) -> &'static str {
        match self {
            Verdict::ThisMerge(_) => "YES",
            Verdict::Unanswered(_) => "UNANSWERED",
            Verdict::Unattributed { .. } => "UNATTRIBUTED",
            _ => "NO",
        }
    }
}

fn instant(raw: &str) -> Option<DateTime<FixedOffset>> {
    DateTime::parse_from_rfc3339(raw.trim()).ok()
}

/// Decide whether PR `pr`'s merge is what closed issue `issue`.
#[must_use]
pub fn decide(issue: u64, pr: u64, facts: &Facts) -> Verdict {
    if facts.issue_number.is_some_and(|n| n != issue) || facts.pr_number.is_some_and(|n| n != pr) {
        return Verdict::Unanswered("the facts describe a different issue or PR".into());
    }
    let Some(state) = facts.state.as_deref() else {
        return Verdict::Unanswered("no issue state in the input".into());
    };
    if state != "closed" {
        return Verdict::NotClosed;
    }
    let Some(closed) = facts.closed_at.as_deref().and_then(instant) else {
        return Verdict::Unanswered("the issue is closed but its close time is unreadable".into());
    };
    let Some(merged) = facts.merged_at.as_deref().and_then(instant) else {
        return Verdict::Unanswered("the PR's merge time is unreadable (is it merged?)".into());
    };
    let secs = (closed - merged).num_seconds();
    if secs < 0 {
        return Verdict::ClosedBeforeMerge { secs: -secs };
    }
    match &facts.closer {
        Closer::PullRequest(n) if *n == pr => Verdict::ThisMerge(Evidence::CloserIsPr),
        Closer::PullRequest(n) => Verdict::OtherPullRequest(*n),
        Closer::Commit(oid) if facts.pr_commits.iter().any(|c| c == oid) => {
            Verdict::ThisMerge(Evidence::CloserIsPrCommit)
        }
        Closer::Direct => Verdict::OtherCloser("closed directly, not through a PR".into()),
        Closer::Other(kind) => Verdict::OtherCloser(format!("closed by a {kind}")),
        // A commit this PR's list does not contain is not evidence of another
        // closer (a rebase merge rewrites every oid), so it is weighed like
        // no closer at all.
        Closer::Commit(_) | Closer::Unavailable if secs <= TIED_WINDOW_SECS => {
            Verdict::ThisMerge(Evidence::TimestampTied)
        }
        Closer::Commit(_) | Closer::Unavailable => Verdict::Unattributed { secs },
    }
}

/// The one output line: `LOOM-CLOSED-BY-MERGE <token> issue #N: <why>`. It
/// names the issue in every outcome, so a skipped reopen is never silent.
#[must_use]
pub fn render(issue: u64, pr: u64, verdict: &Verdict) -> String {
    let why = match verdict {
        Verdict::ThisMerge(Evidence::CloserIsPr) => {
            format!("its latest close is attributed to PR #{pr}")
        }
        Verdict::ThisMerge(Evidence::CloserIsPrCommit) => {
            format!("its latest close is attributed to a commit of PR #{pr}")
        }
        Verdict::ThisMerge(Evidence::TimestampTied) => format!(
            "no closer was readable, but it closed within {TIED_WINDOW_SECS}s after PR #{pr} merged"
        ),
        Verdict::NotClosed => "it is not closed".to_string(),
        Verdict::ClosedBeforeMerge { secs } => {
            format!("it was closed {secs}s BEFORE PR #{pr} merged — not by this merge")
        }
        Verdict::OtherPullRequest(n) => {
            format!("its latest close is attributed to PR #{n}, not PR #{pr}")
        }
        Verdict::OtherCloser(what) => format!("it was {what} — not by PR #{pr}'s merge"),
        Verdict::Unattributed { secs } => format!(
            "no closer was readable and it closed {secs}s after PR #{pr} merged (more than {TIED_WINDOW_SECS}s) — not attributing it to this merge"
        ),
        Verdict::Unanswered(why) => format!("could not answer — {why}"),
    };
    format!("LOOM-CLOSED-BY-MERGE {} issue #{issue}: {why}\n", verdict.token())
}

#[cfg(test)]
mod tests;
