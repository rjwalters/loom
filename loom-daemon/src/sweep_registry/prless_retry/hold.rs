//! The forge-visible half of the PR-less retry hold (Issues #7972, #9239).
//!
//! [`super`] — `prless_retry` — owns the tally — what counts as a PR-less release,
//! how the streak backs off, when the threshold is reached. This module owns
//! what the fleet can actually *see*: the `loom:blocked` label that parks the
//! issue, and the comments that explain it.
//!
//! The split is the #9239 lesson in module form. That issue was filed as "the
//! hold posts a comment but does not apply `loom:blocked`", and the two halves
//! had drifted exactly that far apart: the label write was a fire-and-forget
//! `gh issue edit` whose result was discarded, while the comment that
//! *described* the park was posted unconditionally right after it. A park that
//! is announced but not applied is worse than no park at all — dispatch keeps
//! claiming the issue while every reader, human and agent, has been told it is
//! held.
//!
//! So the rule this module enforces, and the reason it is worth its own file:
//!
//! > **The `loom:blocked` write is the deliverable. The comment is commentary.**
//!
//! [`SweepRegistry::write_prless_hold_label`] checks the exit status, retries
//! the add half alone when the combined flip is rejected, reads the label back
//! before giving up, and reports [`PrlessHoldOutcome::LabelWriteFailed`] when
//! the park cannot be confirmed — in which case no "held" notice is posted and
//! the tally does not record the issue as held. This mirrors the discipline
//! `judge.md` states for verdicts: the label command is the primary
//! deliverable, and a role that only commented has not acted.

use super::*;

/// Marker prefix on every comment this module posts, so the per-attempt notes
/// and the hold notice are machine-identifiable (and greppable) the way
/// [`QUARANTINE_COMMENT_MARKER`] is for #3939.
pub const PRLESS_RETRY_COMMENT_MARKER: &str = "<!-- loom:prless-retry (#7972) -->";

/// Second-line marker naming the **kind** of comment, carried in addition to
/// [`PRLESS_RETRY_COMMENT_MARKER`] (Issue #9239).
///
/// This module posts three different things and, before #9239, all three led
/// with the same marker — so an audit that grepped for `prless-retry` counted
/// sub-threshold attempt notes as holds. That is not hypothetical: #9239's
/// headline measurement ("174 hold comments, far more than the `loom:blocked`
/// label writes") was largely that artifact. On `rjwalters/loom#8812` four of
/// the five "holds" were `Attempt 2 of 3` notes — which by design apply no
/// label — and the single real hold applied `loom:blocked` in the same second
/// it commented. A hold and a note must be distinguishable **without parsing
/// English prose**, so each body now carries its kind as a marker too.
pub const PRLESS_ATTEMPT_COMMENT_MARKER: &str = "<!-- loom:prless-retry-kind=attempt (#9239) -->";

/// Kind marker on a real hold notice — posted **only after** `loom:blocked` is
/// confirmed on the forge (Issue #9239). See [`PRLESS_ATTEMPT_COMMENT_MARKER`].
pub const PRLESS_HOLD_COMMENT_MARKER: &str = "<!-- loom:prless-retry-kind=hold (#9239) -->";

/// Kind marker on the notice posted when the hold's label write **failed**
/// (Issue #9239) — the issue is NOT parked and is still a dispatch candidate.
/// Bounded to **at most** one per streak (posted only when the count reaching
/// the hold is exactly `threshold`, i.e. the first hold attempt), so a repo
/// whose label writes are persistently rejected raises the alarm without
/// re-raising it every cycle.
///
/// "At most", not "exactly", since #9292 made that count fleet-wide: a host
/// that observes two peer releases at once can step from 1 straight past the
/// threshold and skip the equality. The failure is still logged at `error` on
/// the host that hit it every time — only the forge-side alarm is the thing
/// bounded here, and over-tight is the right direction for a comment whose
/// whole purpose is to be noticed.
pub const PRLESS_HOLD_FAILED_COMMENT_MARKER: &str =
    "<!-- loom:prless-retry-kind=hold-failed (#9239) -->";

/// Bound on how much `gh` stderr is carried into a log line or a forge notice
/// when a hold's label write fails (Issue #9239). Enough for the one-line
/// GitHub API error that actually names the cause, short of pasting a usage
/// dump into an issue.
const PRLESS_HOLD_STDERR_LIMIT: usize = 400;

/// What [`SweepRegistry::apply_prless_hold_label`] actually did (Issue #7972,
/// extended by #9239).
///
/// Only [`Self::VetoedOpenPr`] is positive evidence that the tally itself was
/// wrong; the other two vetoes leave it standing. [`Self::LabelWriteFailed`] is
/// a different kind of answer from all of them — not "the hold was
/// unnecessary" but "the hold did not happen" — and is the one the caller must
/// not round up to success.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PrlessHoldOutcome {
    /// `loom:blocked` applied (confirmed), `loom:issue` removed where the
    /// forge accepted it, hold notice posted.
    Applied,
    /// The issue is already closed — nothing to hold, tally kept.
    VetoedClosed,
    /// Re-verification found an open linked PR: this was never a PR-less loop.
    VetoedOpenPr,
    /// Label flips are disabled (hermetic fixture) — no forge state to touch.
    VetoedNoForge,
    /// The `loom:blocked` write did not land and could not be confirmed
    /// (Issue #9239). **The hold did not happen**: the issue is still a
    /// dispatch candidate, so the tally must not record it as held and no
    /// "held" notice is posted — the comment claiming a park nobody performed
    /// is exactly what made #9239's 174 hold comments look like 174 parks.
    LabelWriteFailed,
}

/// Why one `gh issue edit` attempt of the hold's label write did not succeed
/// (Issue #9239). The distinction is load-bearing: a **rejection** (the
/// command ran and exited non-zero) is cheap to re-attempt differently, while
/// a **timeout** means `gh` is wedged and further calls would only spend more
/// of the `reap_once` read path's #3973 budget on the same wedge.
#[derive(Debug, Clone, PartialEq, Eq)]
enum HoldEditFailure {
    /// The call exceeded [`reap_gh_timeout`] and was killed — do not retry.
    TimedOut(String),
    /// The call completed and exited non-zero, or could not be spawned.
    Rejected(String),
}

impl HoldEditFailure {
    /// The human-readable detail, for a log line or a forge notice.
    fn detail(&self) -> &str {
        match self {
            Self::TimedOut(d) | Self::Rejected(d) => d,
        }
    }
}

/// Collapse a failed `gh` call's stderr into one bounded line for a log
/// message or a forge notice (Issue #9239).
///
/// Blank lines are dropped and the rest joined with `; ` — GitHub's API errors
/// arrive as a short line or two, while `gh`'s usage dumps do not, so the
/// [`PRLESS_HOLD_STDERR_LIMIT`] truncation keeps the worst case out of an issue
/// comment. Empty stderr reads as `(no stderr)` rather than an empty string, so
/// a log line never trails off into nothing.
fn truncate_gh_stderr(stderr: &[u8]) -> String {
    let text = String::from_utf8_lossy(stderr);
    let joined = text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join("; ");
    if joined.is_empty() {
        return "(no stderr)".to_string();
    }
    if joined.chars().count() <= PRLESS_HOLD_STDERR_LIMIT {
        return joined;
    }
    let truncated: String = joined.chars().take(PRLESS_HOLD_STDERR_LIMIT).collect();
    format!("{truncated}…")
}

impl SweepRegistry {
    /// Best-effort comment naming the failure behind a sub-threshold PR-less
    /// release (Issue #7972 AC3) — so the next claimer, human or agent, starts
    /// from "the last attempt died like *this*" instead of from nothing.
    ///
    /// Skipped entirely when label flips are disabled (test fixtures /
    /// `skip_label_flip`). Best-effort: a `gh` failure is logged at debug and
    /// never affects the load-bearing in-memory tally.
    pub(super) fn post_prless_attempt_comment(
        &self,
        issue: u32,
        consecutive: u32,
        threshold: u32,
        reason: &str,
    ) {
        let body = format!(
            "{PRLESS_RETRY_COMMENT_MARKER}\n\
             {PRLESS_ATTEMPT_COMMENT_MARKER}\n\
             **Attempt {consecutive} of {threshold} ended without a pull request.** This issue's \
             `loom:building` claim has now been taken and released {consecutive} times in a row \
             with no PR to show for it.\n\n\
             Last failure: {reason}\n\n\
             Re-dispatch is spaced out while this streak continues; after {threshold} consecutive \
             PR-less releases the issue is held with `loom:blocked` rather than re-claimed again \
             (Issue #7972). **This note is not that hold** — the issue is still in the dispatch \
             queue and carries no `loom:blocked`; only a comment marked \
             `loom:prless-retry-kind=hold` parks it (Issue #9239). If you are the next claimer, \
             read the failure above before repeating \
             it — if the issue's scope no longer exists on `main`, say so and rescope or close it \
             rather than retrying."
        );
        self.post_prless_comment(issue, &body, "attempt note");
    }

    /// Best-effort forge mutation on a PR-less hold (Issue #7972): add
    /// `loom:blocked`, remove `loom:issue`, and post a comment naming the
    /// failure and the count — so the pause is visible to a human on the forge,
    /// not just in the daemon log. Modeled directly on
    /// [`Self::apply_quarantine_label`].
    ///
    /// `loom:blocked` (not `loom:operator`) on purpose: `loom:blocked` is the
    /// established automated-hold state that the work finder's skip-label filter
    /// and `quarantine_reconciliation` already understand, and #7972's own
    /// acceptance criterion names it first. Skipped entirely when label flips
    /// are disabled; every step is best-effort.
    pub(super) fn apply_prless_hold_label(
        &self,
        issue: u32,
        consecutive: u32,
        reason: &str,
    ) -> PrlessHoldOutcome {
        if self.config.skip_label_flip {
            return PrlessHoldOutcome::VetoedNoForge;
        }
        // A closed issue (or a PR number that slipped through) needs no hold:
        // it is already out of the candidate pool, and parking it would add a
        // `loom:blocked` label and a comment to settled work. Fail-open — an
        // unverifiable read (`None`) proceeds with the hold, since a stranded
        // re-claim loop is the failure this exists to stop.
        if self.issue_is_closed_or_pr(issue) == Some(true) {
            log::info!(
                "sweep_registry: issue #{issue} reached {consecutive} consecutive PR-less \
                 releases but is already closed — recording the tally without applying a \
                 `loom:blocked` hold (#7972)"
            );
            return PrlessHoldOutcome::VetoedClosed;
        }
        // Last-chance re-verification, and the ONLY forge probe this mechanism
        // adds anywhere. `note_prless_terminal_outcome`'s classification is
        // deliberately forge-free (see its doc comment), which leaves one
        // residual false-positive shape: an issue whose open PR was never
        // sampled by any of these sweeps and whose #6788 memo had expired. A
        // hold is the one irreversible-ish step here (a `loom:blocked` label a
        // human has to clear), so it is worth exactly one closes-graph query
        // to rule that out. `Open` means the issue is waiting on Judge, not
        // stuck in a PR-less loop; `ProbeFailed`/`NoneOpen` proceed, because a
        // forge outage must not be able to strand the loop either.
        if let OpenPrProbe::Open(pr) = self.probe_open_linked_pr(issue) {
            log::info!(
                "sweep_registry: issue #{issue} reached {consecutive} consecutive PR-less \
                 releases, but re-verification found open linked PR #{pr} — NOT holding; the \
                 issue is waiting on review, not looping (#7972)"
            );
            return PrlessHoldOutcome::VetoedOpenPr;
        }
        // THE LABEL WRITE IS THE PRIMARY DELIVERABLE (Issue #9239). Everything
        // below the next five lines is reporting; `loom:blocked` is the park.
        // Before #9239 this was a fire-and-forget `gh issue edit` whose result
        // was discarded — `Ok(Some(_)) => {}` accepts a *completed* child at
        // ANY exit status — and the hold notice was posted regardless. So a
        // rejected write produced a comment saying the issue was held, on an
        // issue that was still a live dispatch candidate: observed on
        // `2AMLogic/sky130-sar-adc#121`, seven hold notices over four days,
        // two different bot accounts, and not one `loom:blocked` label event.
        if let Err(failure) = self.write_prless_hold_label(issue) {
            log::error!(
                "sweep_registry: PR-less hold for #{issue} FAILED to apply `loom:blocked` after \
                 {consecutive} consecutive PR-less releases — the issue is NOT parked and remains \
                 a dispatch candidate (#9239). Cause: {failure}. Last failure was: {reason}"
            );
            // One alarm per streak, on the first hold attempt only: the point
            // is to make a broken label write diagnosable from the forge (the
            // daemon log lives on whichever fleet host happened to dispatch),
            // not to replace inert "held" comments with inert "not held" ones.
            if consecutive == self.prless_retry_config.threshold {
                let body = format!(
                    "{PRLESS_RETRY_COMMENT_MARKER}\n\
                     {PRLESS_HOLD_FAILED_COMMENT_MARKER}\n\
                     **Tried to hold this issue after {consecutive} consecutive claims that \
                     produced no pull request — and could not.** The `loom:blocked` label write \
                     was rejected by the forge, so this issue is **not** parked: dispatch can and \
                     will claim it again (Issue #9239).\n\n\
                     Label write failure: `{failure}`\n\n\
                     Last sweep failure: {reason}\n\n\
                     **What to do**: apply `loom:blocked` by hand to stop the re-claim loop, then \
                     fix the underlying sweep failure above. If the label write keeps being \
                     rejected, the cause is environmental rather than about this issue — check \
                     that the repo defines `loom:blocked` and that the dispatching token still \
                     carries issue-write scope."
                );
                self.post_prless_comment(issue, &body, "hold-failure notice");
            }
            return PrlessHoldOutcome::LabelWriteFailed;
        }

        let body = format!(
            "{PRLESS_RETRY_COMMENT_MARKER}\n\
             {PRLESS_HOLD_COMMENT_MARKER}\n\
             **Held after {consecutive} consecutive claims that produced no pull request.** This \
             issue's `loom:building` claim was taken and released {consecutive} times in a row \
             without a PR and without a self-reported no-op release — the dispatch loop is \
             retrying something that keeps failing the same way, so it is now held with \
             `loom:blocked` instead of being re-claimed a {next} time (Issue #7972).\n\n\
             Last failure: {reason}\n\n\
             **What to do**: read the failure above and fix its cause — the common ones are a \
             scope that no longer exists on `main` (rescope or close the issue), a build/test \
             failure the Builder cannot get past, and a missing dependency or credential. Then \
             flip `loom:blocked` back to `loom:issue` to return the issue to the queue; the tally \
             resets, so it gets a full runway of {consecutive} fresh attempts. Nothing here says \
             the issue is invalid — only that repeating the same dispatch unchanged will not \
             produce a different result.",
            next = consecutive + 1,
        );
        self.post_prless_comment(issue, &body, "hold notice");
        PrlessHoldOutcome::Applied
    }

    /// Apply the hold's `loom:blocked` label and report **honestly** whether
    /// the park actually happened (Issue #9239).
    ///
    /// Three bounded steps, in order, stopping at the first success:
    ///
    /// 1. The combined flip — `--add-label loom:blocked --remove-label
    ///    loom:issue` — which is what a healthy hold does in one call.
    /// 2. On a *rejection* (the command ran and exited non-zero), the **add
    ///    half alone**. `gh issue edit` applies its label mutations as one
    ///    unit, so a `--remove-label` the forge refuses takes the park down
    ///    with it even though the park is the half that matters. `loom:issue`
    ///    lingering next to `loom:blocked` is untidy but harmless — every
    ///    candidate filter reads `PARK_LABELS` (`SKIP_LABELS`) and skips it
    ///    (#7071, #8925). An unparked issue in a re-claim loop is not harmless.
    /// 3. A read-back: `loom:blocked` may already be present from an earlier
    ///    hold, a peer host, or a human, in which case the park is a fact
    ///    regardless of what our writes did.
    ///
    /// A **timeout** is never retried — it means `gh` is wedged, and this runs
    /// inside `reap_once` on the `ListSweeps` / `GetSweepStatus` read path
    /// whose per-call budget (#3973) is 5s. The worst case here is two fast
    /// rejections plus one probe; the wedged case still costs exactly one
    /// timeout, as before.
    ///
    /// Fails **closed**: an unconfirmable label is reported as a failed hold.
    /// The cost of a false "failed" is one more spaced-out dispatch attempt
    /// (the backoff window is armed either way); the cost of a false "applied"
    /// is #9239 itself.
    fn write_prless_hold_label(&self, issue: u32) -> Result<(), String> {
        let first = match self.run_prless_hold_edit(issue, true) {
            Ok(()) => return Ok(()),
            Err(HoldEditFailure::TimedOut(detail)) => {
                // Wedged `gh`: no retry, no probe — just report it truthfully.
                return Err(detail);
            }
            Err(failure) => failure,
        };
        log::warn!(
            "sweep_registry: PR-less hold label flip for #{issue} was rejected ({}) — retrying \
             with the `loom:blocked` half alone, which is the part that parks the issue (#9239)",
            first.detail()
        );
        let second = match self.run_prless_hold_edit(issue, false) {
            Ok(()) => {
                log::warn!(
                    "sweep_registry: PR-less hold for #{issue} applied `loom:blocked` on the \
                     add-only retry; `loom:issue` may still be present, which the work finder's \
                     park-label filter already skips (#9239)"
                );
                return Ok(());
            }
            Err(failure) => failure,
        };
        if self.issue_has_blocked_label(issue) {
            log::info!(
                "sweep_registry: PR-less hold label writes for #{issue} both failed, but the \
                 issue already carries `loom:blocked` — the park stands (#9239)"
            );
            return Ok(());
        }
        Err(format!("{}; add-only retry also failed: {}", first.detail(), second.detail()))
    }

    /// One `gh issue edit` attempt for the hold, reporting the **actual**
    /// result rather than merely that a child process finished (Issue #9239).
    ///
    /// `remove_loom_issue` selects the combined flip or the add-only fallback;
    /// see [`Self::write_prless_hold_label`] for why the fallback exists.
    fn run_prless_hold_edit(
        &self,
        issue: u32,
        remove_loom_issue: bool,
    ) -> Result<(), HoldEditFailure> {
        let issue_arg = issue.to_string();
        let mut edit = vec!["issue", "edit", &issue_arg, "--add-label", "loom:blocked"];
        if remove_loom_issue {
            edit.extend(["--remove-label", "loom:issue"]);
        }
        let repo_flag = crate::claim_reconciliation::gh_call::loom_repo_flag();
        edit.extend(repo_flag.iter().map(String::as_str));
        // Counted as `prless.hold_label` (#10089), scoped to the workspace
        // (#5401). Bounded (Issue #3973): this runs from `reap_once`, which is
        // on the `ListSweeps` / `GetSweepStatus` read path.
        let timeout = reap_gh_timeout();
        match self.gh_write("prless.hold_label", edit) {
            Ok(Some(out)) if out.status.success() => Ok(()),
            Ok(Some(out)) => Err(HoldEditFailure::Rejected(format!(
                "`gh issue edit` exited {}: {}",
                out.status
                    .code()
                    .map_or_else(|| "on a signal".to_string(), |c| c.to_string()),
                truncate_gh_stderr(&out.stderr)
            ))),
            Ok(None) => Err(HoldEditFailure::TimedOut(format!(
                "`gh issue edit` exceeded {}s and was killed (#3973)",
                timeout.as_secs()
            ))),
            Err(e) => {
                Err(HoldEditFailure::Rejected(format!("`gh issue edit` could not be run: {e}")))
            }
        }
    }

    /// Shared best-effort `gh issue comment` behind
    /// [`Self::post_prless_attempt_comment`] and
    /// [`Self::apply_prless_hold_label`] (Issue #7972) — identical transport,
    /// timeout, and credential handling; only the body and the log label differ.
    fn post_prless_comment(&self, issue: u32, body: &str, what: &str) {
        if self.config.skip_label_flip {
            return;
        }
        let issue_arg = issue.to_string();
        let mut comment = vec!["issue", "comment", &issue_arg, "--body", body];
        let repo_flag = crate::claim_reconciliation::gh_call::loom_repo_flag();
        comment.extend(repo_flag.iter().map(String::as_str));
        let timeout = reap_gh_timeout();
        match self.gh_write("prless.comment", comment) {
            Ok(Some(_)) => {}
            Ok(None) => log::debug!(
                "sweep_registry: PR-less retry {what} for #{issue} exceeded {}s, killed (#3973)",
                timeout.as_secs()
            ),
            Err(e) => {
                log::debug!("sweep_registry: PR-less retry {what} for #{issue} failed: {e}");
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::sweep_registry::test_support::{
        fake_gh_graphql_arm, fake_gh_timeline_rest_arm, state_probe_json,
    };
    use crate::sweep_registry::{SweepRegistry, SweepRegistryConfig};
    use serial_test::serial;
    use std::os::unix::fs::PermissionsExt;
    use tempfile::tempdir;

    /// A registry whose forge writes are **enabled** (`skip_label_flip =
    /// false`), driven by a fake `gh` that appends every invocation's argv to
    /// the returned log path (Issue #8728).
    ///
    /// Three answering arms. Their order in the script is **bash arm-matching
    /// precedence, not call order**: the #5911 REST timeline (`timeline_pr`,
    /// empty for "no open PR") is spliced first because its endpoint also
    /// matches the state probe's `repos/*` glob — the ordering note
    /// [`fake_gh_timeline_rest_arm`] carries — then the GraphQL closes-graph
    /// (`graphql_prs`, whitespace-separated open PR numbers), then the #4504
    /// issue-state probe (`issue_state`, `"open"` / `"closed"`). The *call*
    /// order is the reverse: the hold consults the state probe first and only
    /// then `probe_open_linked_pr`, which tries GraphQL and falls back to the
    /// REST timeline. `repo view` resolves the owner/repo the two `gh api`
    /// probes cannot infer from the working directory. Everything else — in
    /// particular the `issue edit` and `issue comment` mutations these tests
    /// exist to observe — is logged and exits 0.
    fn forge_registry(
        ws: &Path,
        issue_state: &str,
        graphql_prs: &str,
        timeline_pr: &str,
    ) -> (SweepRegistry, PathBuf) {
        let gh_log = ws.join("gh-invocations.log");
        let fake_gh = ws.join("fake-gh-prless.sh");
        let script = format!(
            "#!/usr/bin/env bash\n\
             printf '%s\\n' \"$*\" >> \"{log}\"\n\
             {timeline}\
             {gql}\
             if [[ \"$1\" == \"api\" && \"$2\" == repos/* ]]; then\n\
             printf '%s\\n' '{state}'\n\
             exit 0\n\
             fi\n\
             if [[ \"$1\" == \"repo\" && \"$2\" == \"view\" ]]; then\n\
             printf 'rjwalters/loom\\n'\n\
             exit 0\n\
             fi\n\
             exit 0\n",
            log = gh_log.display(),
            timeline = fake_gh_timeline_rest_arm(timeline_pr, 0),
            gql = fake_gh_graphql_arm(graphql_prs, 0),
            state = state_probe_json(issue_state, false),
        );
        std::fs::write(&fake_gh, &script).unwrap();
        let mut perms = std::fs::metadata(&fake_gh).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&fake_gh, perms).unwrap();
        if let Ok(f) = std::fs::File::open(&fake_gh) {
            let _ = f.sync_all();
        }

        let mut config = SweepRegistryConfig::new(ws.to_path_buf());
        config.gh_bin = Some(fake_gh);
        config.skip_label_flip = false;
        config.journal_path = Some(ws.join("test-sweeps-journal.json"));
        (SweepRegistry::new(config), gh_log)
    }

    /// A [`forge_registry`] whose `gh issue edit` arm is **scripted to fail**
    /// (Issue #9239) — the hold's label write is what is under test here, and
    /// on the forge it does sometimes fail: `2AMLogic/sky130-sar-adc#121` took
    /// seven hold notices across four days with zero `loom:blocked` events.
    ///
    /// `combined_exit` answers the `--add-label … --remove-label …` flip and
    /// `add_only_exit` the add-half fallback, so the three interesting shapes
    /// (both fail / only the combined flip fails / both succeed) are one
    /// argument pair. `blocked_probe` is what `gh issue view --json labels`
    /// answers for the read-back — `"true"` means the issue already carries
    /// `loom:blocked`.
    fn forge_registry_with_scripted_edit(
        ws: &Path,
        combined_exit: i32,
        add_only_exit: i32,
        blocked_probe: &str,
    ) -> (SweepRegistry, PathBuf) {
        let gh_log = ws.join("gh-invocations.log");
        let fake_gh = ws.join("fake-gh-prless-edit.sh");
        let script = format!(
            "#!/usr/bin/env bash\n\
             printf '%s\\n' \"$*\" >> \"{log}\"\n\
             if [[ \"$1\" == \"issue\" && \"$2\" == \"edit\" ]]; then\n\
             if [[ \"$*\" == *\"--remove-label\"* ]]; then\n\
             if [[ {combined} -ne 0 ]]; then\n\
             printf 'HTTP 422: Validation Failed (label write refused)\\n' >&2\n\
             fi\n\
             exit {combined}\n\
             fi\n\
             if [[ {add_only} -ne 0 ]]; then\n\
             printf 'HTTP 403: Resource not accessible by integration\\n' >&2\n\
             fi\n\
             exit {add_only}\n\
             fi\n\
             if [[ \"$1\" == \"issue\" && \"$2\" == \"view\" ]]; then\n\
             printf '%s\\n' '{blocked}'\n\
             exit 0\n\
             fi\n\
             {timeline}\
             {gql}\
             if [[ \"$1\" == \"api\" && \"$2\" == repos/* ]]; then\n\
             printf '%s\\n' '{state}'\n\
             exit 0\n\
             fi\n\
             if [[ \"$1\" == \"repo\" && \"$2\" == \"view\" ]]; then\n\
             printf 'rjwalters/loom\\n'\n\
             exit 0\n\
             fi\n\
             exit 0\n",
            log = gh_log.display(),
            combined = combined_exit,
            add_only = add_only_exit,
            blocked = blocked_probe,
            timeline = fake_gh_timeline_rest_arm("", 0),
            gql = fake_gh_graphql_arm("", 0),
            state = state_probe_json("open", false),
        );
        std::fs::write(&fake_gh, &script).unwrap();
        let mut perms = std::fs::metadata(&fake_gh).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&fake_gh, perms).unwrap();
        if let Ok(f) = std::fs::File::open(&fake_gh) {
            let _ = f.sync_all();
        }

        let mut config = SweepRegistryConfig::new(ws.to_path_buf());
        config.gh_bin = Some(fake_gh);
        config.skip_label_flip = false;
        config.journal_path = Some(ws.join("test-sweeps-journal.json"));
        (SweepRegistry::new(config), gh_log)
    }

    /// Set `threshold` and drive `issue` straight to its hold with that many
    /// consecutive PR-less releases (#8728).
    fn hold_at_threshold(reg: &mut SweepRegistry, issue: u32, threshold: u32, reason: &str) {
        reg.set_prless_retry_config(PrlessRetryConfig {
            threshold,
            ..PrlessRetryConfig::default()
        });
        for _ in 0..threshold {
            reg.record_prless_release(issue, reason);
        }
    }

    /// Every logged `gh` invocation whose argv begins with `prefix` — the
    /// fake `gh` logs `"$*"`, so a multi-line `--body` spans several lines and
    /// only the first one carries the subcommand (#8728).
    fn gh_calls_starting_with(gh_log: &Path, prefix: &str) -> Vec<String> {
        std::fs::read_to_string(gh_log)
            .unwrap_or_default()
            .lines()
            .filter(|l| l.starts_with(prefix))
            .map(std::string::ToString::to_string)
            .collect()
    }

    /// #8728 AC1: at the threshold the hold flips the forge labels — exactly
    /// one `gh issue edit`, naming the RIGHT issue, adding `loom:blocked` and
    /// removing `loom:issue`. The work finder's skip-label filter reads that
    /// label, not this process's memory, so the flip is the durable half of
    /// the hold.
    #[test]
    #[serial]
    fn the_hold_flips_loom_blocked_on_and_loom_issue_off_for_the_right_issue() {
        let dir = tempdir().unwrap();
        std::env::remove_var("LOOM_REPO");
        // Open issue, no open linked PR on either transport: neither veto fires.
        let (mut reg, gh_log) = forge_registry(dir.path(), "open", "", "");

        hold_at_threshold(&mut reg, 7893, 2, "builder crashed without opening a PR");

        assert!(reg.prless_retry_held(7893), "the threshold must hold the issue");
        let edits = gh_calls_starting_with(&gh_log, "issue edit ");
        assert_eq!(edits.len(), 1, "exactly one label flip at the threshold, got: {edits:?}");
        assert_eq!(
            edits[0], "issue edit 7893 --add-label loom:blocked --remove-label loom:issue",
            "the hold's argv must name the held issue and flip both labels"
        );
    }

    /// #8728 AC2: the hold posts its notice to the issue, marked with
    /// [`PRLESS_RETRY_COMMENT_MARKER`] so it is machine-identifiable, and the
    /// body names both the count and the failure — the whole point of the
    /// comment is that the next reader does not have to rediscover why.
    #[test]
    #[serial]
    fn the_hold_notice_comment_is_posted_and_carries_the_marker() {
        let dir = tempdir().unwrap();
        std::env::remove_var("LOOM_REPO");
        let (mut reg, gh_log) = forge_registry(dir.path(), "open", "", "");

        hold_at_threshold(&mut reg, 7893, 2, "scope `safehouse_chatops/` does not exist on main");

        let comments = gh_calls_starting_with(&gh_log, "issue comment ");
        assert_eq!(comments.len(), 1, "the hold posts exactly one notice, got: {comments:?}");
        assert!(
            comments[0]
                .starts_with(&format!("issue comment 7893 --body {PRLESS_RETRY_COMMENT_MARKER}")),
            "the notice must go to the held issue and lead with the marker: {}",
            comments[0]
        );
        let body = std::fs::read_to_string(&gh_log).unwrap();
        assert!(
            body.contains("**Held after 2 consecutive claims that produced no pull request.**"),
            "the notice names the count: {body}"
        );
        assert!(
            body.contains("scope `safehouse_chatops/` does not exist on main"),
            "the notice names the failure: {body}"
        );
    }

    /// #8728 AC3: the last-chance re-verification is the one forge probe this
    /// mechanism adds, and an OPEN linked PR is positive evidence the tally was
    /// wrong — the issue is waiting on Judge, not looping. No label flip, no
    /// comment, and (via `record_prless_release`'s `VetoedOpenPr` arm) the
    /// whole tally is dropped rather than left standing on a false premise.
    #[test]
    #[serial]
    fn re_verification_finding_an_open_linked_pr_vetoes_the_hold_and_clears_the_tally() {
        let dir = tempdir().unwrap();
        std::env::remove_var("LOOM_REPO");
        // The closes-graph answers with an open PR #8123.
        let (mut reg, gh_log) = forge_registry(dir.path(), "open", "8123", "");

        hold_at_threshold(&mut reg, 7893, 2, "looked PR-less from in-memory state");

        assert_eq!(
            reg.prless_release_count(7893),
            0,
            "an open linked PR is positive evidence the tally was wrong — drop it"
        );
        assert!(!reg.prless_retry_held(7893));
        assert!(
            reg.prless_retry_remaining(7893, Utc::now()).is_none(),
            "a cleared tally must not keep the issue out of dispatch"
        );
        assert!(
            gh_calls_starting_with(&gh_log, "issue edit ").is_empty(),
            "a vetoed hold must not flip any label"
        );
        assert!(
            gh_calls_starting_with(&gh_log, "issue comment ").is_empty(),
            "a vetoed hold must not post a hold notice either"
        );
    }

    /// #8728 AC4: a closed issue is already out of the candidate pool, so
    /// holding it would only add a `loom:blocked` label and a comment to
    /// settled work. The veto fires BEFORE the closes-graph re-verification —
    /// no point spending that query — and, unlike the open-PR veto, leaves the
    /// tally standing: being closed does not contradict the count.
    #[test]
    #[serial]
    fn the_closed_issue_veto_does_not_flip_labels() {
        let dir = tempdir().unwrap();
        std::env::remove_var("LOOM_REPO");
        let (mut reg, gh_log) = forge_registry(dir.path(), "closed", "", "");

        hold_at_threshold(&mut reg, 7893, 2, "crashed without opening a PR");

        assert!(
            gh_calls_starting_with(&gh_log, "issue edit ").is_empty(),
            "a closed issue must not be labeled `loom:blocked`"
        );
        assert!(
            gh_calls_starting_with(&gh_log, "issue comment ").is_empty(),
            "a closed issue must not get a hold notice"
        );
        let calls = std::fs::read_to_string(&gh_log).unwrap_or_default();
        assert!(
            calls
                .lines()
                .any(|l| l.starts_with("api repos/") && l.contains("/issues/7893 --jq")),
            "the closed-issue state probe is what vetoed the hold: {calls}"
        );
        assert!(
            !calls.contains("api graphql"),
            "the closed veto short-circuits before the closes-graph re-verification: {calls}"
        );
        assert!(
            reg.prless_retry_held(7893),
            "a closed issue does not contradict the tally — the record stands"
        );
        assert_eq!(reg.prless_release_count(7893), 2);
    }

    /// #7972 AC3 on the forge side (#8728): the second consecutive PR-less
    /// release — one short of the default threshold — posts the attempt note,
    /// marked like the hold notice, and flips nothing. Commenting from the
    /// second onward is what bounds the comment count by the threshold rather
    /// than by the length of the loop.
    #[test]
    #[serial]
    fn the_second_consecutive_release_posts_a_marked_attempt_note_without_flipping_labels() {
        let dir = tempdir().unwrap();
        std::env::remove_var("LOOM_REPO");
        let (mut reg, gh_log) = forge_registry(dir.path(), "open", "", "");
        assert_eq!(reg.prless_retry_config().threshold, DEFAULT_PRLESS_RETRY_THRESHOLD);

        reg.record_prless_release(7893, "first failure");
        assert!(
            gh_calls_starting_with(&gh_log, "issue comment ").is_empty(),
            "a single PR-less release is plausibly a one-off — no comment yet"
        );

        reg.record_prless_release(7893, "second failure: build error the Builder cannot pass");

        assert!(!reg.prless_retry_held(7893), "still one short of the threshold");
        let comments = gh_calls_starting_with(&gh_log, "issue comment ");
        assert_eq!(comments.len(), 1, "one attempt note, got: {comments:?}");
        assert!(
            comments[0]
                .starts_with(&format!("issue comment 7893 --body {PRLESS_RETRY_COMMENT_MARKER}")),
            "the attempt note must go to the right issue and carry the marker: {}",
            comments[0]
        );
        let calls = std::fs::read_to_string(&gh_log).unwrap();
        assert!(
            calls.contains(&format!(
                "**Attempt 2 of {DEFAULT_PRLESS_RETRY_THRESHOLD} ended without a pull request.**"
            )),
            "the note names where in the runway this attempt sits: {calls}"
        );
        assert!(
            gh_calls_starting_with(&gh_log, "issue edit ").is_empty(),
            "below the threshold nothing is held, so no label may be flipped"
        );
    }

    /// #9239, the headline test: when the `loom:blocked` write does not land,
    /// **the hold did not happen** and nothing may claim otherwise. No "Held
    /// after …" notice, no `held` flag — because a comment asserting a park
    /// that was never applied is precisely what let `sky130-sar-adc#121` be
    /// re-claimed 383 times while seven notices said it was held.
    #[test]
    #[serial]
    fn a_rejected_label_write_is_not_recorded_or_announced_as_a_hold() {
        let dir = tempdir().unwrap();
        std::env::remove_var("LOOM_REPO");
        // Both write shapes rejected, and the read-back confirms the label is
        // absent: the park demonstrably did not happen.
        let (mut reg, gh_log) = forge_registry_with_scripted_edit(dir.path(), 1, 1, "false");

        hold_at_threshold(&mut reg, 7893, 2, "sweep exited 1 after 86s without opening a PR");

        assert!(
            !reg.prless_retry_held(7893),
            "an unconfirmed `loom:blocked` must never be recorded as a hold (#9239)"
        );
        let log = std::fs::read_to_string(&gh_log).unwrap();
        assert!(
            !log.contains("**Held after"),
            "no notice may claim a park that did not happen: {log}"
        );
        assert!(
            log.contains(PRLESS_HOLD_FAILED_COMMENT_MARKER),
            "the failure must be raised on the issue, marked as a failed hold: {log}"
        );
        assert!(
            log.contains("HTTP 422") || log.contains("HTTP 403"),
            "the notice must carry the forge's own error, which is the only \
             diagnosable thing about it: {log}"
        );
    }

    /// #9239: the tally keeps re-attempting the hold rather than settling into
    /// a false "held" state — and the alarm comment is raised **once** per
    /// streak, not on every subsequent release. A repo whose label writes are
    /// persistently rejected must be loud, not spammy.
    #[test]
    #[serial]
    fn a_failed_hold_retries_the_label_write_and_warns_only_once_per_streak() {
        let dir = tempdir().unwrap();
        std::env::remove_var("LOOM_REPO");
        let (mut reg, gh_log) = forge_registry_with_scripted_edit(dir.path(), 1, 1, "false");

        hold_at_threshold(&mut reg, 7893, 2, "sweep exited 1 without opening a PR");
        let after_first = gh_calls_starting_with(&gh_log, "issue edit ").len();
        assert_eq!(
            after_first, 2,
            "a rejected combined flip must be retried with the add half alone, got: {after_first}"
        );

        // Two further PR-less releases past the threshold.
        reg.record_prless_release(7893, "sweep exited 1 without opening a PR");
        reg.record_prless_release(7893, "sweep exited 1 without opening a PR");

        assert!(
            gh_calls_starting_with(&gh_log, "issue edit ").len() > after_first,
            "each further release must re-attempt the label write, not assume the park (#9239)"
        );
        let alarms = std::fs::read_to_string(&gh_log)
            .unwrap()
            .matches(PRLESS_HOLD_FAILED_COMMENT_MARKER)
            .count();
        assert_eq!(alarms, 1, "exactly one alarm per streak, got {alarms}");
    }

    /// #9239: `gh issue edit` applies its label mutations as one unit, so a
    /// `--remove-label` the forge refuses takes the park down with it. The
    /// add half alone is the fallback — `loom:issue` lingering beside
    /// `loom:blocked` is untidy, and every candidate filter skips it anyway
    /// (`PARK_LABELS`, #7071/#8925); an unparked issue in a re-claim loop is
    /// the failure that actually costs something.
    #[test]
    #[serial]
    fn a_rejected_combined_flip_still_parks_the_issue_via_the_add_half() {
        let dir = tempdir().unwrap();
        std::env::remove_var("LOOM_REPO");
        let (mut reg, gh_log) = forge_registry_with_scripted_edit(dir.path(), 1, 0, "false");

        hold_at_threshold(&mut reg, 7893, 2, "builder crashed without opening a PR");

        assert!(
            reg.prless_retry_held(7893),
            "the add-only fallback applied `loom:blocked`, so the issue IS held"
        );
        let edits = gh_calls_starting_with(&gh_log, "issue edit ");
        assert_eq!(edits.len(), 2, "combined flip then add-only retry, got: {edits:?}");
        assert_eq!(
            edits[1], "issue edit 7893 --add-label loom:blocked",
            "the fallback drops only the `--remove-label` half"
        );
        assert!(
            std::fs::read_to_string(&gh_log)
                .unwrap()
                .contains("**Held after"),
            "a confirmed park does get its notice"
        );
    }

    /// #9239: both writes failing is not proof the issue is unparked — an
    /// earlier hold, a peer host, or a human may already have applied
    /// `loom:blocked`. The read-back is what settles it, and a park that is
    /// already a fact is still a park.
    #[test]
    #[serial]
    fn an_already_blocked_issue_reads_back_as_held_even_when_both_writes_fail() {
        let dir = tempdir().unwrap();
        std::env::remove_var("LOOM_REPO");
        let (mut reg, gh_log) = forge_registry_with_scripted_edit(dir.path(), 1, 1, "true");

        hold_at_threshold(&mut reg, 7893, 2, "builder crashed without opening a PR");

        assert!(
            reg.prless_retry_held(7893),
            "`loom:blocked` is present on the forge — the park stands (#9239)"
        );
        let log = std::fs::read_to_string(&gh_log).unwrap();
        assert!(log.contains("**Held after"), "a real park gets its notice: {log}");
        assert!(!log.contains(PRLESS_HOLD_FAILED_COMMENT_MARKER), "…and no failure alarm: {log}");
    }

    /// #9239's measurement defect: the attempt note and the hold notice shared
    /// one marker, so an audit grepping `prless-retry` counted notes as holds —
    /// which is how "174 hold comments vs far fewer label writes" was read off
    /// `rjwalters/loom#8812`, where four of five "holds" were `Attempt 2 of 3`
    /// notes (one per dispatch host) and the real hold labelled the issue in
    /// the same second. The two kinds must be distinguishable without parsing
    /// prose.
    #[test]
    #[serial]
    fn attempt_notes_and_hold_notices_carry_distinct_kind_markers() {
        let dir = tempdir().unwrap();
        std::env::remove_var("LOOM_REPO");
        let (mut reg, gh_log) = forge_registry(dir.path(), "open", "", "");
        reg.set_prless_retry_config(PrlessRetryConfig {
            threshold: 3,
            ..PrlessRetryConfig::default()
        });

        reg.record_prless_release(7893, "first");
        reg.record_prless_release(7893, "second");
        let notes = std::fs::read_to_string(&gh_log).unwrap();
        assert!(
            notes.contains(PRLESS_ATTEMPT_COMMENT_MARKER),
            "the sub-threshold note declares itself a note: {notes}"
        );
        assert!(
            !notes.contains(PRLESS_HOLD_COMMENT_MARKER),
            "…and never a hold, because it applies no label: {notes}"
        );

        reg.record_prless_release(7893, "third");
        let all = std::fs::read_to_string(&gh_log).unwrap();
        assert!(
            all.contains(PRLESS_HOLD_COMMENT_MARKER),
            "the threshold-th release posts the hold kind: {all}"
        );
        assert!(
            all.contains(PRLESS_RETRY_COMMENT_MARKER),
            "both kinds keep the #7972 marker, so existing greps still match"
        );
    }

    /// #9239: `gh` stderr reaches a log line and a forge comment, so it is
    /// bounded and never empty — a truncated cause is diagnosable, a missing
    /// one is not.
    #[test]
    fn gh_stderr_is_joined_bounded_and_never_empty() {
        assert_eq!(truncate_gh_stderr(b""), "(no stderr)");
        assert_eq!(truncate_gh_stderr(b"  \n\n "), "(no stderr)");
        assert_eq!(
            truncate_gh_stderr(b"HTTP 403: Resource not accessible\nTry again\n"),
            "HTTP 403: Resource not accessible; Try again"
        );
        let long = vec![b'x'; PRLESS_HOLD_STDERR_LIMIT * 3];
        let out = truncate_gh_stderr(&long);
        assert_eq!(out.chars().count(), PRLESS_HOLD_STDERR_LIMIT + 1);
        assert!(out.ends_with('…'));
    }
}
