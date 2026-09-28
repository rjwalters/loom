//! The POST-merge stacked-child reconcile decision (#3747 stacked-PR v2 item 1,
//! #8010 item 2), a slice of the merge-pr port #8191.
//!
//! # What it decides
//!
//! When a stacked PARENT PR on a `feature/issue-<N>` branch squash-merges, every
//! still-open CHILD PR based on that branch is left pointing at a branch the
//! forge is about to delete. `merge-pr.sh`'s `_auto_reconcile_stacked_children`
//! walks those children and, per child, takes one of two routes:
//!
//! * **reconcile** — hand the child to `reconcile-stack.sh`, which rebases it
//!   onto the default branch and force-with-leases the result.
//! * **defer** — do *not* rebase; post a comment on the child PR saying why and
//!   how to finish by hand.
//!
//! The route turns on one fact: whether the child's own issue is still
//! `loom:building`. A live `loom:building` claim means a Builder probably has
//! that branch checked out in a worktree, and an out-of-band
//! `git rebase --onto` + `push --force-with-lease` against a branch somebody is
//! committing to is how you lose their work.
//!
//! This module owns the two *decisions* and the deferral comment's text:
//!
//! * [`plan`] — the parent-branch gate, the children-rollup parse, and each
//!   child's issue-number derivation.
//! * [`child_route`] — safe/unsafe from the child issue's live label set, plus
//!   [`defer_comment`], the byte-frozen comment body the defer route posts.
//!
//! Everything with an effect stays in `merge-pr.sh`: the live `gh pr list`
//! discovery (never the daemon registry — #3747), the uncached `gh api` label
//! read, the `reconcile-stack.sh` invocation, and the #4856 rate-limit-safe
//! comment post.
//!
//! # Why this one
//!
//! The retired shell expressed the child-issue derivation and the parent gate as
//! the **same** `[[ =~ ^feature/issue-([0-9]+)$ ]]` written out twice, 90 lines
//! apart, reading two different variables — and the safe/unsafe gate as a
//! `grep -qx` over a `jq`-extracted label list whose failure mode is *silent
//! agreement with the unsafe answer*:
//!
//! ```text
//! issue_json="$(gh api "repos/$REPO_NWO/issues/$child_issue" 2>/dev/null || echo '{}')"
//! issue_labels="$(echo "$issue_json" | jq -r '.labels[]?.name' 2>/dev/null || true)"
//! if printf '%s\n' "$issue_labels" | grep -qx 'loom:building'; then building="true"; fi
//! ```
//!
//! Three layers each turn a failure into the empty string, and the empty string
//! is indistinguishable from "this issue carries no `loom:building`" — which is
//! the answer that authorises the force-push. The shell's own comment calls that
//! out and accepts it ("a read failure is treated as 'not building' (safe) since
//! the reconcile itself is best-effort and force-with-lease still protects the
//! branch"). The port keeps that disposition — see [`Route::Reconcile`] — but
//! makes it a *decided* one: [`child_route`] distinguishes "no labels were
//! supplied at all" from "labels were supplied and `loom:building` is not among
//! them", so the transcript says which happened.
//!
//! # Fidelity
//!
//! Per `defaults/docs/verification-recipes.md` §6:
//!
//! * The branch predicate is a hand parse (`strip_prefix` + an all-ASCII-digit
//!   run), not a regex. Bash's `=~` is POSIX ERE with no `REG_NEWLINE`, so `$`
//!   anchors at end-of-*string* — a trailing newline does **not** match. A Rust
//!   `^…$` would be the same today but the hand parse cannot drift.
//! * The issue number stays a **`String`**, never a `u64`. Bash captured
//!   `[0-9]+` as text and interpolated it straight into a URL, so a 40-digit
//!   run round-tripped intact; parsing to `u64` would introduce an overflow
//!   divergence the shell never had.
//! * `grep -qx 'loom:building'` is a **whole-line literal** match, so the port
//!   compares whole lines for byte equality — not `contains`, which would also
//!   match a hypothetical `loom:building-paused`. There is no CR stripping and
//!   no trimming of any kind: `merge-pr.sh` is a script, so its `grep` is the
//!   PATH one (GNU on the fleet, BSD on macOS), and both treat `\r` as data —
//!   `loom:building\r` is not the claim. Trimming it would look like tidying
//!   and would flip that input from `reconcile` to `defer`. (`ugrep`, which
//!   *does* strip CR as line-ending handling, is the reason this is spelled
//!   out rather than left to "obviously".)
//! * The label list is read as **bytes-to-lines**: `grep` is byte-oriented and
//!   does not require valid UTF-8, so a label list that is not valid UTF-8 must
//!   not abort the read the way `read_to_string` would.
//!
//! # Fail direction: OPEN
//!
//! This whole pass runs **after** the merge has already happened, returns 0
//! unconditionally, and its own shell already has a skip path for a missing
//! `reconcile-stack.sh`. So a daemon that cannot answer must degrade to "no
//! auto-reconciliation this pass" with a warning, exactly as that skip does —
//! never to a guess. The cost is one manual `reconcile-stack.sh` invocation,
//! which every message on both routes already prints; the cost of failing
//! closed would be refusing merges on a host whose daemon lags a release, for a
//! cleanup step.
//!
//! That is the same fail direction [`super::stacked_children`] (this guard's
//! PRE-merge sibling) argues for, and the reason neither raises the
//! `requires-daemon: merge-pr` floor in `merge-pr.sh`.
//!
//! The one thing the seam must not do is read silence as a route: both verbs
//! answer with a positive sentinel line, so "no output" is unambiguously "the
//! verb did not run".

/// One row of the reconcile plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Row {
    /// A child PR to act on.
    Child {
        /// The child PR number, as text — see the module's fidelity note.
        pr: String,
        /// The child's head branch, verbatim from the rollup.
        branch: String,
        /// The child's issue number, derived from `branch` when it follows the
        /// `feature/issue-<N>` convention. `None` means the branch does not,
        /// which the retired shell treated as "no `loom:building` claim to
        /// race, so safe".
        issue: Option<String>,
    },
    /// A rollup element that is not a usable child row. The retired `jq`
    /// interpolated whatever was there (`null`, an object's compact JSON) into
    /// a tab-separated row and let `gh api` fail on it downstream; this names it
    /// instead, and the remaining rows are still returned.
    Malformed {
        /// Zero-based index of the offending element.
        index: usize,
        /// What was wrong with it.
        detail: String,
    },
}

/// What [`plan`] concluded about a merged parent branch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Plan {
    /// The parent branch is not `feature/issue-<N>`, so it cannot have stacked
    /// children by this convention. The retired shell's first `return 0`.
    NotStacked,
    /// The rollup is not a JSON array. The retired shell's `jq` errored here
    /// and `|| echo 0` / `|| true` turned that into "no children"; this says so
    /// out loud, which at the seam becomes a warning rather than a silent skip.
    Unreadable(String),
    /// The rollup parsed. May be empty (no open children — a legitimate no-op).
    Rows(Vec<Row>),
}

/// The parent-branch gate and the children-rollup parse.
///
/// `children_json` is the `[{number, headRefName}]` rollup — either the
/// pre-merge snapshot `STACKED_CHILDREN_JSON` (#8010 item 2, preferred: the
/// forge retargets an open child the instant `delete_branch_on_merge` removes
/// the parent branch, so a post-merge re-query can legitimately see zero rows)
/// or the live `gh pr list --base <parent> --state open` fallback.
pub fn plan(parent_branch: &str, children_json: &[u8]) -> Plan {
    if issue_from_branch(parent_branch).is_none() {
        return Plan::NotStacked;
    }
    let value: serde_json::Value = match serde_json::from_slice(children_json) {
        Ok(v) => v,
        Err(e) => return Plan::Unreadable(format!("not valid JSON: {e}")),
    };
    // Array only, deliberately. `jq`'s `.[]` also iterates an OBJECT's values,
    // but neither producer of this rollup emits one, and treating an object as
    // a child list would act on rows nobody meant to produce.
    let Some(elements) = value.as_array() else {
        return Plan::Unreadable(format!(
            "expected a JSON array of children, got {}",
            json_kind(&value)
        ));
    };
    Plan::Rows(
        elements
            .iter()
            .enumerate()
            .map(|(i, e)| row(i, e))
            .collect(),
    )
}

fn row(index: usize, element: &serde_json::Value) -> Row {
    let Some(object) = element.as_object() else {
        return Row::Malformed {
            index,
            detail: format!("expected an object, got {}", json_kind(element)),
        };
    };
    let pr = match object.get("number") {
        // A non-negative JSON integer, or a digit string — `jq`'s `"\(.number)"`
        // rendered either identically.
        Some(serde_json::Value::Number(n)) if n.as_u64().is_some() => n.to_string(),
        Some(serde_json::Value::String(s)) if is_ascii_digits(s) => s.clone(),
        other => {
            return Row::Malformed {
                index,
                detail: format!(
                    "'number' is not a PR number: {}",
                    other.map_or("absent".to_string(), json_kind)
                ),
            }
        }
    };
    let Some(branch) = object.get("headRefName").and_then(|v| v.as_str()) else {
        return Row::Malformed {
            index,
            detail: format!(
                "'headRefName' is not a string: {}",
                object
                    .get("headRefName")
                    .map_or("absent".to_string(), json_kind)
            ),
        };
    };
    Row::Child {
        pr,
        branch: branch.to_string(),
        issue: issue_from_branch(branch),
    }
}

fn json_kind(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::Null => "null",
        serde_json::Value::Bool(_) => "a boolean",
        serde_json::Value::Number(_) => "a number",
        serde_json::Value::String(_) => "a string",
        serde_json::Value::Array(_) => "an array",
        serde_json::Value::Object(_) => "an object",
    }
    .to_string()
}

/// The issue number a `feature/issue-<N>` branch names, or `None`.
///
/// This is the ONE definition of the predicate the retired shell wrote out
/// twice — once as the parent gate, once as the child derivation. See the
/// module's fidelity note for why it is a hand parse returning a `String`
/// rather than a regex returning a `u64`.
///
/// Rejects `feature/issue-` with no digits, any non-ASCII-digit character
/// anywhere after the prefix (including a trailing newline, which bash's
/// unanchored-`$`-free ERE also rejected), and a leading `+`/`-`.
pub fn issue_from_branch(branch: &str) -> Option<String> {
    let rest = branch.strip_prefix("feature/issue-")?;
    if is_ascii_digits(rest) {
        Some(rest.to_string())
    } else {
        None
    }
}

/// A non-empty run of ASCII digits and nothing else — `[0-9]+` under an anchored
/// ERE. `all`, not `any`: one digit anywhere is not what `^…([0-9]+)$` accepted,
/// and reading `feature/issue-12a` as issue `12a` would address the claim lookup
/// at an issue that does not exist, whose "no labels" answer is the route that
/// force-pushes. `!s.is_empty()` carries the `+` — `all` is vacuously true on the
/// empty slice, which is exactly the `feature/issue-` case the `+` rejects.
fn is_ascii_digits(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())
}

/// Which route one child takes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Route {
    /// No live `loom:building` claim — hand the child to `reconcile-stack.sh`.
    ///
    /// This is also the route an *unreadable* label set takes, preserving the
    /// retired shell's documented disposition: the reconcile is best-effort and
    /// `push --force-with-lease` independently refuses to overwrite a tip the
    /// pusher has not seen, so an unanswered claim lookup is not, on its own, a
    /// data-loss risk. Which of the two situations produced this route is
    /// reported separately, in [`Decision::why`].
    Reconcile,
    /// The child's issue is still `loom:building` — defer, and post
    /// [`defer_comment`].
    Defer,
}

impl Route {
    /// The wire token. `merge-pr.sh` matches on these exactly.
    pub fn token(self) -> &'static str {
        match self {
            Route::Reconcile => "reconcile",
            Route::Defer => "defer",
        }
    }
}

/// A route plus why it was taken.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Decision {
    /// The route.
    pub route: Route,
    /// Diagnostic only — never printed by `merge-pr.sh`, whose stdout the port
    /// keeps byte-identical. It exists so the verb is answerable when run by
    /// hand, and so the differential harness can assert WHICH reason each side
    /// reached rather than only that both said `reconcile`.
    pub why: &'static str,
}

/// Safe/unsafe for one child, from its issue's live label set.
///
/// `issue` is [`Row::Child::issue`]: `None` means the child branch does not
/// follow the `feature/issue-<N>` convention, so there is no claim to race and
/// the retired shell short-circuited to safe without any forge read at all.
///
/// `labels` is the raw bytes of the uncached `gh api repos/<nwo>/issues/<n>`
/// label list, one name per line, as `merge-pr.sh` still extracts it. It is
/// matched line-by-line for byte equality with `loom:building` — `grep -qx`,
/// not `grep -q`. Bytes rather than `&str` because `grep` never required valid
/// UTF-8 and a label list that is not must not abort the read.
pub fn child_route(issue: Option<&str>, labels: &[u8]) -> Decision {
    if issue.is_none() {
        return Decision {
            route: Route::Reconcile,
            why: "child branch is not feature/issue-<N>, so it carries no loom:building claim",
        };
    }
    if labels.iter().all(|b| b.is_ascii_whitespace()) {
        return Decision {
            route: Route::Reconcile,
            why: "no labels were supplied for the child issue (lookup failed, or it has none) \
                  — treated as not building, as the retired shell did; force-with-lease is the \
                  remaining protection",
        };
    }
    // No CR stripping: `grep -x` compares the whole line, and GNU grep (what
    // `merge-pr.sh`, a script, resolves) treats `\r` as an ordinary data byte.
    // A `loom:building\r` line is therefore NOT the claim. See the module's
    // fidelity note — an earlier draft of this port trimmed the CR, which read
    // as harmless tidying but silently flipped that input's route.
    let building = labels
        .split(|&b| b == b'\n')
        .any(|line| line == b"loom:building");
    if building {
        Decision {
            route: Route::Defer,
            why: "the child issue is still loom:building",
        }
    } else {
        Decision {
            route: Route::Reconcile,
            why: "the child issue's labels do not include loom:building",
        }
    }
}

/// The deferral comment body, byte-frozen from the retired shell.
///
/// Held byte-for-byte against the frozen fixture by
/// `loom-daemon/tests/merge_pr_reconcile_differential.rs`. `timestamp` is the
/// shell's `date -u +%Y-%m-%dT%H:%M:%SZ`.
///
/// Written as ONE literal with real newlines rather than backslash
/// continuations: the retired text is three very long unwrapped paragraph lines
/// (Markdown, so a hard wrap would be a rendering change, and a continuation's
/// joining space is invisible in review). The literal below is therefore wide
/// on purpose — it is the operator-visible artifact, and its shape must be
/// readable as-is.
///
/// There is deliberately **no trailing newline**: the shell's `comment="…"`
/// ended at the closing quote.
pub fn defer_comment(
    child_issue: &str,
    child_pr: &str,
    parent_branch: &str,
    timestamp: &str,
) -> String {
    format!(
        "## Stacked parent merged — reconciliation deferred

Parent branch `{parent_branch}` squash-merged, but this child's issue #{child_issue} is still `loom:building` — a Builder likely has this branch checked out. Auto-reconciliation was **skipped** to avoid racing that in-progress work with an out-of-band `git rebase --onto` + `push --force-with-lease`.

**What happens next**: once issue #{child_issue} is no longer `loom:building`, a subsequent parent-merge-triggered pass will reconcile this PR automatically. You can also reconcile it by hand now (from a clean checkout, only once the Builder has finished):

```
./.loom/scripts/reconcile-stack.sh {child_pr} {parent_branch}
```

---
*Deferred by merge-pr.sh (#3747) at {timestamp}*"
    )
}

#[cfg(test)]
mod tests;
