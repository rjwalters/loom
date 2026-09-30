//! `merge-pr.sh`'s logic, ported incrementally (#8191, epic #7810).
//!
//! `merge-pr.sh` is the most defect-dense script in the portable pool — 48
//! fix-commits in six months over 1,458 code lines, against a next-worst of
//! 1.4 per 100 — and it performs 21 irreversible operations (API merges,
//! pushes, branch deletion, worktree removal) unattended, under Champion's
//! auto-merge. It is also frozen by the file-size ratchet at 1,460 lines, so
//! the fixes it keeps needing must now be net-zero or smaller. That trap is
//! what makes a port the remedy rather than another patch.
//!
//! At ~3,100 total lines it is three times the watchdog (#8086) and is being
//! taken in slices, each independently reviewable and each keeping the 22
//! retained suites (8,509 assertions' worth of lines) passing unchanged.
//!
//! Slice 1 is [`refs`]: the closing-keyword / partial-increment analysis. It
//! goes first because it is the highest fragility per line — five stacked
//! `awk`/`sed`/`grep -oiE` pipelines whose entire bug history is about what
//! the regex accidentally matched — and because it is pure, which makes a
//! byte-for-byte differential against the shell possible.
//!
//! Slice 2 is [`labels`]: the verdict-label mutual-exclusion guard (#8112) —
//! refusing to merge a PR that carries `loom:pr` alongside a contradicting
//! label.
//!
//! Slice 4 is [`head_sync`]: the self-sync head-SHA attribution guard
//! (#8164) — deciding when a 409 "Head branch was modified" was caused by
//! this run's OWN base-sync push, and may therefore be retried once against a
//! freshly-read head, versus a foreign push that must stay a hard stop.
//!
//! Slice 3 is [`stale_checks`]: the required-check freshness guard (#8248) —
//! refusing a merge whose green required-check results predate the base
//! branch's current tip, which is how a ratchet baseline tightened under an
//! in-flight PR red-lined main on 2026-09-18.
//!
//! [`redate`] is not a port — it is new functionality (#8508) closing the gap
//! #8248 left open: once that guard blocks a merge, nothing automatically
//! produces the fresh evidence it is waiting for when the merge token lacks
//! `actions:write` to re-run the stale check directly. It pushes a
//! tree-identical no-op commit instead, which re-triggers CI without
//! weakening the guard itself — bounded to one attempt per head, after which
//! the PR is escalated to a durable `loom:operator` hold rather than pushed
//! at forever.
//!
//! #8914's in-place re-run once ran before [`redate`], on the theory that
//! re-running a workflow run keeps the head SHA and with it the Judge verdict.
//! It was **removed in #8919**: GitHub replays a run against the ORIGINAL
//! `GITHUB_SHA`, which for a `pull_request` run is the test merge commit built
//! on the base already tested, so an in-place re-run re-dates the evidence
//! without re-validating anything. Only a new `pull_request` event rebuilds the
//! merge commit against the current base — which is exactly what [`redate`]'s
//! tree-identical push produces.
//!
//! [`zero_checks`] is not a port either — it is #9091's narrowing of the
//! #6169 zero-row settle guard inside `_wait_for_checks_then_sync_merge`:
//! re-polling an empty check-runs rollup until `LOOM_AUTO_MERGE_TIMEOUT`
//! elapsed was catastrophic on the case that guard meets most often (a repo
//! with no CI configured for the changed paths), so the wait is bounded when —
//! and only when — the base branch requires no status-check contexts.
//!
//! [`checks_failure`] is the OTHER classification inside the same wait loop
//! (#8191 slice): once a poll finds a FAILING check, whether it is a required
//! status-check context (refuse — that context can never turn green on this
//! SHA), informational with nothing else pending (proceed to the synchronous
//! merge, the #3486 UNSTABLE fallback), or informational with something else
//! still pending (keep waiting, unchanged). The `comm -12` overlap test
//! becomes a set intersection; both input lists (`failing`/`pending` from the
//! check-runs rollup, `required` from `forge_get_required_status_check_contexts`,
//! which — unlike [`stale_checks`]'s GitHub-only lookup — also covers Gitea)
//! stay forge reads in the shell.
//!
//! [`check_runs_streak`] is the OTHER classification the same poll loop makes
//! on a FAILED fetch attempt (#6389, an #8191 slice): whether a confirmed
//! HTTP 404 — both the attempt and its retry-once agreeing — has now repeated
//! `LOOM_CHECK_RUNS_404_STREAK` times in a row, which is treated as "this
//! repo's Checks API is unavailable for this commit" rather than a transient
//! blip, and short-circuits the wait straight to the synchronous merge. A
//! single 404 paired with a DIFFERENT failure on the retry does not confirm,
//! and any non-confirming iteration resets the streak to zero — the shell
//! keeps owning the fetch attempts themselves and the deadline/timeout
//! handling; only the streak/threshold classification moved.
//!
//! [`loom_pr_guard`] is the pre-merge `loom:pr` review-signal guard (#7419)
//! — the OTHER half of the verdict-label story [`labels`] tells: this one
//! fires on `loom:pr`'s ABSENCE ("nobody reviewed this head") rather than a
//! contradiction beside a present approval, and carries the only override
//! flag in the family (`--allow-unapproved`) because "nobody reviewed it" and
//! "a reviewer said no" are different acts.
//!
//! [`dirty_guard`] leaves the merge gates behind for the post-merge cleanup:
//! the #5031 data-loss guard that refuses `git worktree remove --force` on a
//! worktree still holding uncommitted work. It is the only member of the family
//! whose failure mode is destroying somebody's edits rather than merging the
//! wrong tree, which is why it fails CLOSED even though every other step of
//! cleanup is best-effort. The port also retires the THIRD copy of Loom's
//! runtime-marker list — #8195 slice 3 deleted `worktree.sh`'s twin after #8279,
//! and this was the one left standing.
//!
//! [`hold_state`] completes that trio: the advisory warning [`loom_pr_guard`]
//! deliberately left in the shell, fired only when `loom:pr` IS present and
//! Champion's recorded merge-risk-hold head is not the head about to merge. It
//! is the only member of the family that never refuses anything — and the port
//! fixes two ways the retired `grep | tail -1 | sed` pipeline lost the warning
//! silently, which for a check nothing else duplicates is the whole risk.
//!
//! [`stacked_children`] is the LAST pre-merge gate `merge-pr.sh` still ran
//! inline: the merge-ordering guard (#3747 item 2, reshaped by #7982). It is
//! the only member of the family that does not merely decide — it *establishes*
//! the postcondition a later step needs, pinning the parent tip to
//! `refs/loom/parent/<branch>` so `reconcile-stack.sh` can still resolve a
//! branch the merge API deletes synchronously underneath it. That is also why it
//! is the only gate here that fails OPEN: it protects a best-effort cleanup
//! step, not the question of whether this tree may merge, and the fail-closed
//! [`labels`] gate runs on the same binary later in the same script.
//!
//! [`mergeable_recheck`] is the stale-cached-mergeable recheck decision
//! (#6104): once REST `.mergeable` has read `false`, which
//! `<action>:<reason>` the backoff re-reads and the local `git merge-tree`
//! corroboration add up to — `merge`, `refuse-conflict`, or the
//! deliberately distinct `refuse-stale` ("the forge's cache is stale and
//! could not be corroborated", not "this branch genuinely conflicts"). The
//! I/O loop stays in the shell so the retained suite's stubs keep driving
//! the real code path; only the terminal classification moved.
//!
//! [`partial_reset`] is the post-merge partial-increment label reset
//! (#3667, with #4569's premature-auto-close revert): given a fresh read of a
//! `Part of #N` issue and what the pre-merge conflict guard recorded about it,
//! the ordered log / reopen / swap steps. The `jq` reads it replaces are
//! modelled filter-by-filter, and the mutations stay in the shell.
//!
//! [`partial_conflict`] is that pre-merge guard itself (#4569/#4595): from
//! the body, the commit messages, the sidebar close targets and each declared
//! issue's fresh read, which issues were open and which a closing reference
//! will close anyway — the two sets [`partial_reset`] consumes — plus the
//! source-attributed warning. It never blocks on a conflict, but a plan it
//! cannot produce refuses the merge: read as empty, it would leave a partial
//! increment closed with nothing recorded to revert it.

//! [`version_policy`] is the pre-merge no-hand-bump guard (#7827) and its
//! oracle choice (#8284): everything `merge-pr.sh` wrapped around the
//! canonical `check-defaults-version-bump.sh` — fetch, ancestry, which ref's
//! checker to trust, and the pass / skip / block classification. The checker
//! itself stays shell, because it is CI's contract too.
//!
//! [`worktrees`] is the `git worktree list --porcelain` parsing behind the
//! post-merge cleanup: which worktree is the primary (never removable, #3710),
//! which branch a given worktree has checked out, and which worktree holds a
//! given branch. Three `awk` bodies whose entire defect history — #3671 (the
//! `exit`-triggers-`END` double-print, which handed callers a `/path\n/path`
//! that exists nowhere), #3717 (`$2` truncating a space-containing path, so the
//! primary-worktree guard compared a prefix and never fired), #4171 — is about
//! what a record-oriented parse saw, and whose every consumer is one of the
//! irreversible steps (`git worktree remove --force`, `git branch -D`). The
//! `git` invocation itself stays in the shell; only the parse moved.
//!
//! [`issue_close_gate`] is [`worktrees`]'s neighbour at the same post-merge
//! choke point, one level up: whether the issue a worktree/branch belongs to
//! is actually finished, so the async-close-race guard (#4186) does not
//! remove a partial-increment worktree the NEXT Builder increment still
//! needs. GitHub closes `Closes #N` issues asynchronously, after the merge
//! webhook fires, so a naive live re-read right here would see "open" for
//! essentially every ordinary merge; the fix is to trust this PR's OWN
//! closing-reference list first (no race there) and only fall back to a live
//! read — kept lazy, so the common case pays for no extra forge call — when
//! it is not. It fails toward [`issue_close_gate::Decision::Preserve`] on
//! anything it cannot corroborate, because the step it gates is destructive
//! and a skipped cleanup is always recoverable later while a wrongly-removed
//! worktree is not.
//!
//! [`closed_building`] is [`partial_reset`]'s twin at the same post-merge
//! choke point, and runs immediately after it: the #6199 cleanup that strips
//! `loom:building` from each issue THIS merge closed. Where `partial_reset`
//! handles the issues a `Part of #N` reference deliberately left OPEN, this
//! one handles the issues a `Closes #N` reference closed — the one label
//! #2838's "no cleanup on close" rule had to make an exception for, because
//! `loom:building` names a liveness claim and a closed issue holds none. It
//! reads its input through the same three `jq` filters, so it reuses
//! `partial_reset`'s [`partial_reset::IssueView`] rather than re-deriving a
//! second model of the same endpoint.
//!
//! [`response`] leaves the gates and the cleanup alike for the retry ladder
//! BETWEEN them: which route a failed merge's forge error TEXT sends the loop
//! down. It is the smallest slice in the family and the one with the sharpest
//! cost of being wrong, because two of its four routes are opposites over
//! near-identical English — "Base branch was modified" is retryable, "Head
//! branch was modified." is #5579's hard stop — and the precedence that kept
//! them apart was previously *emergent*, two `if` blocks five lines apart in a
//! 70-line loop, asserted only by an `awk` scan over `merge-pr.sh`'s own source
//! text for which `grep` appeared first. Porting it makes the order a single
//! ordered `match` in one place; that retirement, and its successor, are
//! recorded in `test-merge-pr-head-mismatch.sh`. All five patterns were pure
//! literals under two DIFFERENT `grep` invocations — one `-Ei`, two bare — so
//! the asymmetric case-sensitivity is upstream behaviour the port preserves
//! verbatim rather than an oversight it tidies.
//!
//! [`reconcile`] is [`stacked_children`]'s POST-merge other half (#3747 item 1):
//! once the parent has merged, which of its still-open children may be handed to
//! `reconcile-stack.sh` and which must be DEFERRED because a Builder still holds
//! a `loom:building` claim on it. It is the only member of the family whose
//! wrong answer force-pushes over somebody's uncommitted work, and the retired
//! shell reached the force-push answer through three stacked `|| true` layers
//! that each turn a failed lookup into the empty string — indistinguishable from
//! "this issue carries no claim". It also retires the second copy of the
//! `feature/issue-<N>` predicate, which the shell wrote out twice 90 lines apart
//! against two different variables, and owns the deferral comment's byte-frozen
//! text.
//!
//! [`worktree_preserve`] is the #6694/#6264 remove-vs-preserve rule
//! `merge-pr.sh`'s post-merge cleanup ran identically at THREE call sites —
//! the Loom-convention path, the porcelain discovery fallback, and the
//! co-existing Judge/Doctor review worktree — differing only in which noun and
//! path variable appeared in the messages. [`issue_close_gate`]'s verdict and
//! `branch_has_landed`'s verdict (#7812) are both already answered by the
//! caller and passed in; this module owns only the two-input decision and its
//! byte-frozen message text, consolidating three copies into one.

pub mod check_runs_streak;
pub mod checks_failure;
pub mod closed_building;
pub mod dirty_guard;
pub mod head_sync;
pub mod hold_state;
pub mod issue_close_gate;
pub mod labels;
pub mod loom_pr_guard;
pub mod mergeable_recheck;
pub mod partial_conflict;
pub mod partial_reset;
pub mod reconcile;
pub mod redate;
pub mod refs;
pub mod response;
pub mod stacked_children;
pub mod stale_checks;
pub mod version_policy;
pub mod worktree_preserve;
pub mod worktrees;
pub mod zero_checks;
