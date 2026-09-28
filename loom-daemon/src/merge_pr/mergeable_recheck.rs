//! The stale-cached-mergeable recheck decision (#6104, slice of #8191).
//!
//! # What this decides
//!
//! GitHub's REST `.mergeable` field is computed asynchronously and invalidated
//! on every push to the base branch. On a repo with continuous automated
//! merges it can read a stale `false` shortly after a base-branch push even
//! though the branch would merge cleanly against current main — observed live
//! on PR #5995, where `merge-pr.sh` refused a merge asserting a conflict that
//! `git merge-tree` proved did not exist against the very base the script had
//! just fetched.
//!
//! `merge-pr.sh`'s `_recheck_mergeable_before_refusal` answers that: once
//! `.mergeable` has read `false`, it re-reads (uncached, post-backoff) up to
//! `retries` times, and if the answer never resolves to `true`, corroborates
//! with a local `git merge-tree` against the freshly fetched base. The I/O —
//! the sleeps, the uncached PR reads, the `git fetch` / `git merge-tree` —
//! stays in the shell (the retained suite drives exactly that loop through
//! its own stubs); THIS module owns the terminal classification and the
//! operator-facing reason strings, which is where the three-way contract
//! lives:
//!
//! | action | meaning |
//! |---|---|
//! | `merge` | proceed — the recheck resolved to `mergeable=true`, or local `git merge-tree` independently confirms clean |
//! | `refuse-conflict` | refuse — local git independently confirms a real conflict |
//! | `refuse-stale` | refuse — the forge's cached state never resolved and local corroboration was unavailable; NOT a confirmed conflict, just unresolved |
//!
//! The `refuse-stale` / `refuse-conflict` split is the whole point of #6104's
//! acceptance criteria: an operator reading the refusal must be able to tell
//! "this branch genuinely conflicts" from "the forge's cache is stale and
//! could not be corroborated" — the remedies differ (rebase vs re-run / raise
//! the backoff).
//!
//! # Byte parity is load-bearing
//!
//! The caller branches on the `<action>:` prefix and the merge-admission
//! telemetry (#6978) derives `retries_used` from a `recheck #N` match inside
//! the reason text. The strings this module emits are therefore frozen
//! exactly as the shell wrote them, and the differential test
//! (`tests/merge_pr_mergeable_recheck_differential.rs`) holds the port to
//! byte-for-byte agreement with the frozen pre-port function.
//!
//! # Why the decision and not the loop moved
//!
//! Every input to [`decide`] is an observation the shell already made by the
//! time it needs the answer (which attempt resolved, whether the refs were
//! named, whether the fetch succeeded, what `merge-tree` said). Moving the
//! classification without moving the I/O keeps the retained suite's stubs —
//! its canned `.mergeable` sequences, its real git fixtures — driving the
//! real code path unchanged, which is the strongest evidence this port has:
//! the suite's cases (a)–(f) now exercise the shell loop AND the Rust
//! decision together, end to end.

/// How the local `git merge-tree` corroboration came out. `NotRun` is the
/// state before corroboration (resolved early, refs unavailable, or fetch
/// failed) — [`decide`] never reads it in those cases.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TreeOutcome {
    NotRun,
    Clean,
    Conflict,
}

/// Everything the shell observed by the time it asks for the decision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Evidence {
    /// The 1-based attempt at which the post-backoff uncached recheck
    /// resolved to `mergeable=true`, if it ever did.
    pub resolved_attempt: Option<u32>,
    /// The configured recheck budget (echoed into every unresolved reason).
    pub retries: u32,
    /// Whether base and head refs were both available for corroboration.
    pub refs_available: bool,
    /// Whether `git fetch origin <base> <head>` succeeded.
    pub fetch_ok: bool,
    /// The `git merge-tree` corroboration result.
    pub tree: TreeOutcome,
    /// The base ref name, interpolated into the corroborating reasons.
    pub base_ref: String,
    /// The head ref name, interpolated into the fetch-failure reason.
    pub head_ref: String,
}

/// The three-way action prefix. Order matters: it is the caller's `case`
/// order, and `refuse-stale` is the fallback arm.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Merge,
    RefuseStale,
    RefuseConflict,
}

impl Action {
    pub fn as_str(self) -> &'static str {
        match self {
            Action::Merge => "merge",
            Action::RefuseStale => "refuse-stale",
            Action::RefuseConflict => "refuse-conflict",
        }
    }
}

/// The decision: one `<action>:<reason>` line, byte-identical to the
/// pre-port shell function's echoes.
pub fn decide(e: &Evidence) -> String {
    if let Some(attempt) = e.resolved_attempt {
        return format!(
            "{action}:cached mergeable=false was stale; recheck #{attempt} \
             (post-backoff, uncached) now reports mergeable=true",
            action = Action::Merge.as_str()
        );
    }
    if !e.refs_available {
        return format!(
            "{action}:forge reports mergeable=false after {retries} recheck(s); \
             base/head ref unavailable for local corroboration",
            action = Action::RefuseStale.as_str(),
            retries = e.retries
        );
    }
    if !e.fetch_ok {
        return format!(
            "{action}:forge reports mergeable=false after {retries} recheck(s); \
             could not fetch origin/{base} and origin/{head} for local corroboration",
            action = Action::RefuseStale.as_str(),
            retries = e.retries,
            base = e.base_ref,
            head = e.head_ref
        );
    }
    match e.tree {
        TreeOutcome::Clean => format!(
            "{action}:forge reports mergeable=false after {retries} recheck(s), \
             but local 'git merge-tree' against origin/{base} is clean — proceeding \
             (stale/false-negative cached state)",
            action = Action::Merge.as_str(),
            retries = e.retries,
            base = e.base_ref
        ),
        TreeOutcome::Conflict | TreeOutcome::NotRun => format!(
            "{action}:forge reports mergeable=false after {retries} recheck(s), \
             confirmed by local 'git merge-tree' against origin/{base} — this branch \
             genuinely conflicts",
            action = Action::RefuseConflict.as_str(),
            retries = e.retries,
            base = e.base_ref
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn evidence() -> Evidence {
        Evidence {
            resolved_attempt: None,
            retries: 3,
            refs_available: true,
            fetch_ok: true,
            tree: TreeOutcome::NotRun,
            base_ref: "main".to_string(),
            head_ref: "feature/x".to_string(),
        }
    }

    #[test]
    fn resolved_early_names_the_attempt() {
        let mut e = evidence();
        e.resolved_attempt = Some(2);
        assert_eq!(
            decide(&e),
            "merge:cached mergeable=false was stale; recheck #2 (post-backoff, uncached) now reports mergeable=true"
        );
    }

    #[test]
    fn the_telemetry_recheck_n_pattern_survives_byte_for_byte() {
        // #6978 derives retries_used from `recheck #N` inside the reason.
        let mut e = evidence();
        e.resolved_attempt = Some(1);
        let reason = decide(&e);
        assert!(reason.contains("recheck #1"));
        // And every unresolved reason embeds the configured count instead.
        let unresolved = decide(&evidence());
        assert!(!unresolved.contains("recheck #"));
        assert!(unresolved.contains("after 3 recheck(s)"));
    }

    #[test]
    fn missing_refs_refuse_stale_not_conflict() {
        let mut e = evidence();
        e.refs_available = false;
        assert_eq!(
            decide(&e),
            "refuse-stale:forge reports mergeable=false after 3 recheck(s); base/head ref unavailable for local corroboration"
        );
    }

    #[test]
    fn fetch_failure_names_both_refs() {
        let mut e = evidence();
        e.fetch_ok = false;
        e.base_ref = "main".into();
        e.head_ref = "feature/clean".into();
        assert_eq!(
            decide(&e),
            "refuse-stale:forge reports mergeable=false after 3 recheck(s); could not fetch origin/main and origin/feature/clean for local corroboration"
        );
    }

    #[test]
    fn clean_tree_proceeds_with_the_stale_false_negative_reason() {
        let mut e = evidence();
        e.tree = TreeOutcome::Clean;
        assert_eq!(
            decide(&e),
            "merge:forge reports mergeable=false after 3 recheck(s), but local 'git merge-tree' against origin/main is clean — proceeding (stale/false-negative cached state)"
        );
    }

    #[test]
    fn conflicting_tree_refuses_as_genuine() {
        let mut e = evidence();
        e.tree = TreeOutcome::Conflict;
        assert_eq!(
            decide(&e),
            "refuse-conflict:forge reports mergeable=false after 3 recheck(s), confirmed by local 'git merge-tree' against origin/main — this branch genuinely conflicts"
        );
    }

    #[test]
    fn precedence_resolved_beats_everything() {
        let mut e = evidence();
        e.resolved_attempt = Some(3);
        e.refs_available = false;
        e.fetch_ok = false;
        e.tree = TreeOutcome::Conflict;
        assert!(decide(&e).starts_with("merge:"));
    }

    #[test]
    fn precedence_refs_before_fetch_before_tree() {
        let mut e = evidence();
        e.refs_available = false;
        e.fetch_ok = false;
        e.tree = TreeOutcome::Conflict;
        assert!(decide(&e).starts_with("refuse-stale:"));
        assert!(decide(&e).contains("ref unavailable"));

        e.refs_available = true;
        assert!(decide(&e).starts_with("refuse-stale:"));
        assert!(decide(&e).contains("could not fetch"));

        e.fetch_ok = true;
        assert!(decide(&e).starts_with("refuse-conflict:"));
    }
}
