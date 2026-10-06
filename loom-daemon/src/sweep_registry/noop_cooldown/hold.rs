//! The durable hold behind the no-op cooldown (Issue #10156).
//!
//! # The gap
//!
//! The cooldown in [`super`] is a single-shot, flat, in-memory window: it
//! expires, and the issue is offered again. Nothing ever *acted* on
//! [`NoopCooldownState::consecutive`](super::NoopCooldownState). An issue whose
//! remaining work is a human gate, or a dependency that has not landed, was
//! therefore re-dispatched forever — 275 `unclassified:after-curator` outcomes
//! on 33 issues in one 24h window (~600M input tokens), starred issues worst
//! of all. Two shapes feed it, and the hold covers both:
//!
//! - the sweep calls `noop-cooldown record` (the `RecordNoopRelease` IPC):
//!   [`SweepRegistry::record_noop_release`] feeds the streak; and
//! - the sweep **never** calls it and just stops after the Curator: the
//!   reaper feeds the streak from [`SweepRegistry::note_curator_only_outcome`],
//!   from the sampled phase history. Without this half the IPC-only design
//!   would be exactly as prompt-dependent as the thing it replaces.
//!
//! # The mechanism
//!
//! Each no-op reads a fingerprint of everything the sweep's conclusion depends
//! on ([`IssueSnapshot::fingerprint`](super::fingerprint)): labels (minus the
//! daemon's own claim churn), non-bot comments, the linked-PR state, and the
//! state of every dependency the body names. An unchanged fingerprint extends
//! the streak; a changed one restarts it at 1. At
//! [`NoopCooldownConfig::hold_threshold`] the issue is parked with a forge
//! label — `loom:blocked` for dependencies, `loom:operator-only` +
//! `loom:operator-decision` for a human gate or when the kind is unclear — and
//! one marked comment says why and what unparks it. `loom:operator-priority`
//! never exempts an issue and is never touched; the park labels are already in
//! [`crate::work_finder::SKIP_LABELS`], which every candidate path applies.
//!
//! The label write is the deliverable, the comment is commentary (the #9239
//! lesson from `prless_retry::hold`): nothing is recorded as held or announced
//! unless the labels are confirmed on the forge, and a failed write is retried
//! on the next no-op without re-commenting.
//!
//! # Unparking
//!
//! The hold is released by the forge, not by daemon state. Any change to the
//! fingerprint restarts the streak, and a no-op arriving for an issue this
//! daemon already held means someone removed the park and re-offered it — it
//! starts a fresh streak at 1 rather than re-holding immediately.
//!
//! # State is host-local
//!
//! The streak lives in this process's memory like the cooldown itself. A
//! daemon restart loses it; the hold then trips again within
//! `hold_threshold` runs. The durable artefact — the park label — is
//! forge-visible, which is what every other host reads.

use super::fingerprint::{choose_park, IssueSnapshot, ParkKind, MAX_DEPENDENCY_READS};
use super::*;

/// Marker on the one comment posted per held streak.
pub const NOOP_HOLD_COMMENT_MARKER: &str = "<!-- loom:noop-hold (#10156) -->";

/// Kind marker on the notice posted when the park's label write failed.
pub const NOOP_HOLD_FAILED_COMMENT_MARKER: &str =
    "<!-- loom:noop-hold-kind=hold-failed (#10156) -->";

/// Kind marker on a real hold notice (posted only after the labels landed).
pub const NOOP_HOLD_APPLIED_COMMENT_MARKER: &str = "<!-- loom:noop-hold-kind=hold (#10156) -->";

/// Bound on `gh` stderr carried into a log line or a forge notice.
const STDERR_LIMIT: usize = 300;

/// One issue's consecutive-no-op streak.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NoopStreak {
    /// Consecutive no-ops with an unchanged fingerprint.
    pub(crate) count: u32,
    /// The fingerprint the streak is accumulating against.
    fingerprint: String,
    /// The sweep whose no-op was counted last, so the IPC report and the
    /// reaper's classification of one dispatch never count twice.
    last_sweep: Option<String>,
    /// The park chosen for this streak.
    pub(crate) park: ParkKind,
    /// The park's labels were confirmed on the forge.
    pub(crate) held: bool,
    /// The one-per-streak "could not park" notice has been posted.
    failure_notice_posted: bool,
}

/// The cooldown windows plus the hold streaks.
///
/// Dereferences to the window map so the pre-#10156 call sites (`get`,
/// `insert`, `remove`, `iter`) are unchanged. It is a wrapper rather than a
/// second field only because the registry's `mod.rs` is size-ratcheted
/// (`.loom/docs/file-size-policy.md`).
#[derive(Debug, Default)]
pub(crate) struct NoopCooldownTable {
    cooldowns: HashMap<u32, NoopCooldownState>,
    streaks: HashMap<u32, NoopStreak>,
}

impl std::ops::Deref for NoopCooldownTable {
    type Target = HashMap<u32, NoopCooldownState>;
    fn deref(&self) -> &Self::Target {
        &self.cooldowns
    }
}

impl std::ops::DerefMut for NoopCooldownTable {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.cooldowns
    }
}

/// What [`SweepRegistry::apply_noop_hold`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NoopHoldOutcome {
    /// Labels confirmed on the forge, notice posted.
    Applied,
    /// The issue is already closed — nothing to park.
    VetoedClosed,
    /// Label flips are disabled (hermetic fixture).
    VetoedNoForge,
    /// The write did not land and could not be confirmed; the issue is still
    /// a dispatch candidate.
    LabelWriteFailed,
}

/// Collapse a failed `gh` call's stderr to one bounded line.
fn trim_stderr(stderr: &[u8]) -> String {
    let joined = String::from_utf8_lossy(stderr)
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join("; ");
    if joined.is_empty() {
        "(no stderr)".to_string()
    } else {
        joined.chars().take(STDERR_LIMIT).collect()
    }
}

/// `jq` over the comments endpoint: one `id:updated_at` line per comment that
/// is neither from a bot account nor one of Loom's own marked comments.
const COMMENTS_JQ: &str = ".[] | select((.user.type // \"\") != \"Bot\") \
                           | select((.body // \"\") | contains(\"<!-- loom:\") | not) \
                           | \"\\(.id):\\(.updated_at)\"";

/// `jq` over one issue: the facts the fingerprint and dependency scan read.
const ISSUE_JQ: &str = "{state: .state, body: .body, labels: [(.labels // [])[] | .name]}";

impl SweepRegistry {
    /// Streak length for `issue` (0 when none). Observability and tests.
    #[cfg(test)]
    #[must_use]
    pub(crate) fn noop_streak_count(&self, issue: u32) -> u32 {
        self.noop_cooldown
            .streaks
            .get(&issue)
            .map_or(0, |s| s.count)
    }

    /// Whether this daemon holds `issue` for a no-op loop.
    #[cfg(test)]
    #[must_use]
    pub(crate) fn noop_hold_applied(&self, issue: u32) -> bool {
        self.noop_cooldown
            .streaks
            .get(&issue)
            .is_some_and(|s| s.held)
    }

    /// The loop kind to stamp on `sweep_id`'s outcome telemetry, or `None`
    /// when this is not (yet) a loop. The streak counts the sweep itself only
    /// once the hook has run, which depends on whether the sweep self-reported
    /// (IPC, before the reap) or was classified by the reaper (after the
    /// journal row), so the sweep is added here when it is not yet counted.
    #[must_use]
    pub(crate) fn noop_loop_kind(&self, issue: u32, sweep_id: &str) -> Option<ParkKind> {
        let streak = self.noop_cooldown.streaks.get(&issue)?;
        if streak.held {
            return Some(streak.park);
        }
        let counted = streak.last_sweep.as_deref() == Some(sweep_id);
        let effective = streak.count + u32::from(!counted);
        (effective >= 2).then_some(streak.park)
    }

    /// The reaper's feed for a sweep that stopped after the Curator and never
    /// called `noop-cooldown record` (Issue #10156). Counts it only when the
    /// sampled phase history (or the checkpoint's own phase) shows nothing
    /// past `curator`, no PR was sampled, and the dispatch did not already
    /// self-report.
    pub(crate) fn note_curator_only_outcome(
        &mut self,
        issue: u32,
        sweep_id: &str,
        checkpoint_phase: Option<&str>,
    ) {
        if self.noop_cooldown_config.hold_threshold == 0
            || self.noop_release_covers_dispatch(issue, sweep_id)
            || self.sampled_pr_number(sweep_id).is_some()
        {
            return;
        }
        let curator_only = self.sampled_only_curator(sweep_id).unwrap_or_else(|| {
            checkpoint_phase
                .is_some_and(|p| super::super::outcome_journal::phase_label(p) == "curator")
        });
        if curator_only {
            self.note_noop_for_hold(issue, Some(sweep_id), None);
        }
    }

    /// Count one no-op toward `issue`'s hold (Issue #10156) and park it when
    /// the streak reaches [`NoopCooldownConfig::hold_threshold`].
    ///
    /// Fails open throughout: an unreadable forge never counts, never resets
    /// and never parks. `0` threshold disables the hold entirely.
    pub(crate) fn note_noop_for_hold(
        &mut self,
        issue: u32,
        sweep_id: Option<&str>,
        reason: Option<&str>,
    ) {
        let threshold = self.noop_cooldown_config.hold_threshold;
        if threshold == 0 {
            return;
        }
        if sweep_id.is_some()
            && self
                .noop_cooldown
                .streaks
                .get(&issue)
                .is_some_and(|s| s.last_sweep.as_deref() == sweep_id)
        {
            return;
        }
        let snapshot = if self.config.skip_label_flip {
            Some(IssueSnapshot {
                open: true,
                linked_pr: "no-forge".into(),
                ..IssueSnapshot::default()
            })
        } else {
            self.fetch_noop_snapshot(issue)
        };
        let Some(snapshot) = snapshot else {
            log::debug!(
                "sweep_registry: no-op hold for #{issue}: forge snapshot unavailable — not \
                 counting this no-op (fail-open, #10156)"
            );
            return;
        };
        // Closed or already parked: nothing for the hold to do, and a stale
        // streak must not survive into a later reopen.
        let already_parked = snapshot.labels.iter().any(|l| {
            crate::work_finder::PARK_LABELS.contains(&l.as_str())
                || l == crate::work_finder::OPERATOR_HOLD_LABEL
        });
        if !snapshot.open || already_parked {
            self.noop_cooldown.streaks.remove(&issue);
            return;
        }
        let fingerprint = snapshot.fingerprint();
        let park = choose_park(reason, Some(&snapshot));
        let prev = self.noop_cooldown.streaks.get(&issue);
        // A held streak that is being no-op'd again means the park was lifted
        // and the issue re-offered: a fresh streak, not an instant re-hold.
        let continues = prev.is_some_and(|p| !p.held && p.fingerprint == fingerprint);
        let count = if continues {
            prev.map_or(1, |p| p.count.saturating_add(1))
        } else {
            1
        };
        let failure_notice_posted = continues && prev.is_some_and(|p| p.failure_notice_posted);
        self.noop_cooldown.streaks.insert(
            issue,
            NoopStreak {
                count,
                fingerprint,
                last_sweep: sweep_id.map(str::to_owned),
                park,
                held: false,
                failure_notice_posted,
            },
        );
        log::info!(
            "sweep_registry: issue #{issue} no-op streak {count}/{threshold} with an unchanged \
             fingerprint (#10156)"
        );
        if count < threshold {
            return;
        }
        let outcome = self.apply_noop_hold(issue, count, park, reason, &snapshot);
        if let Some(s) = self.noop_cooldown.streaks.get_mut(&issue) {
            match outcome {
                NoopHoldOutcome::Applied => s.held = true,
                NoopHoldOutcome::LabelWriteFailed => s.failure_notice_posted = true,
                NoopHoldOutcome::VetoedClosed => {
                    self.noop_cooldown.streaks.remove(&issue);
                }
                NoopHoldOutcome::VetoedNoForge => {}
            }
        }
    }

    /// Park `issue` and say why. Never touches `loom:building` or
    /// `loom:operator-priority`.
    fn apply_noop_hold(
        &self,
        issue: u32,
        count: u32,
        park: ParkKind,
        reason: Option<&str>,
        snapshot: &IssueSnapshot,
    ) -> NoopHoldOutcome {
        if self.config.skip_label_flip {
            return NoopHoldOutcome::VetoedNoForge;
        }
        if self.issue_is_closed_or_pr(issue) == Some(true) {
            return NoopHoldOutcome::VetoedClosed;
        }
        let labels = park.labels();
        let announced = snapshot.fingerprint();
        let already_noticed = self
            .noop_cooldown
            .streaks
            .get(&issue)
            .is_some_and(|s| s.failure_notice_posted);
        if let Err(cause) = self.write_park_labels(issue, labels) {
            log::error!(
                "sweep_registry: no-op hold for #{issue} FAILED to apply {labels:?} after {count} \
                 consecutive no-ops — NOT parked, still a dispatch candidate (#10156): {cause}"
            );
            if !already_noticed {
                let body = format!(
                    "{NOOP_HOLD_COMMENT_MARKER}\n{NOOP_HOLD_FAILED_COMMENT_MARKER}\n\
                     **Tried to hold this issue after {count} consecutive no-op sweeps — and \
                     could not.** The label write ({}) was rejected, so this issue is **not** \
                     parked and will be dispatched again (Issue #10156).\n\nCause: `{cause}`\n\n\
                     **What to do**: apply {} by hand, or fix the token's issue-write scope. The \
                     daemon retries on the next no-op without commenting again.",
                    labels.join(", "),
                    labels.join(" + "),
                );
                self.post_noop_hold_comment(issue, &body);
            }
            return NoopHoldOutcome::LabelWriteFailed;
        }
        let why = reason.map_or_else(String::new, |r| format!("Last reported reason: {r}\n\n"));
        let (needs, unparks) = match park {
            ParkKind::Blocked => (
                "its remaining work is blocked on dependencies",
                "Close or complete the dependencies it names, then flip `loom:blocked` back to \
                 `loom:issue`",
            ),
            ParkKind::HumanGate => (
                "its remaining work is a human gate (or the sweeps could not say what else it \
                 waits on)",
                "Decide it, then remove `loom:operator-only` and `loom:operator-decision` and \
                 add `loom:issue` back",
            ),
        };
        let body = format!(
            "{NOOP_HOLD_COMMENT_MARKER}\n{NOOP_HOLD_APPLIED_COMMENT_MARKER}\n\
             **Held after {count} consecutive no-op sweeps.** Each of the last {count} sweeps \
             concluded there was nothing to do, and nothing this issue depends on changed in \
             between — {needs}. It now carries {} instead of being offered again \
             (Issue #10156). A `loom:operator-priority` star does not exempt an issue from this \
             hold and has been left in place.\n\n{why}Fingerprint compared: `{announced}`\n\n\
             **What unparks it**: {unparks}. Any new comment, label change, linked-PR change or \
             dependency state change also restarts the count from zero.",
            labels.join(" + "),
        );
        self.post_noop_hold_comment(issue, &body);
        NoopHoldOutcome::Applied
    }

    /// Add `labels` (and drop `loom:issue`), reporting honestly whether the
    /// park exists afterwards: combined flip, then the add-only retry on a
    /// rejection, then a read-back. A timeout is never retried.
    fn write_park_labels(&self, issue: u32, labels: &[&str]) -> Result<(), String> {
        let first = match self.run_park_edit(issue, labels, true) {
            Ok(()) => return Ok(()),
            Err((true, detail)) => return Err(detail),
            Err((false, detail)) => detail,
        };
        let second = match self.run_park_edit(issue, labels, false) {
            Ok(()) => return Ok(()),
            Err((_, detail)) => detail,
        };
        if self
            .fetch_noop_snapshot(issue)
            .is_some_and(|s| labels.iter().all(|l| s.labels.iter().any(|x| x == l)))
        {
            return Ok(());
        }
        Err(format!("{first}; add-only retry also failed: {second}"))
    }

    /// One `gh issue edit`; `Err((timed_out, detail))`.
    fn run_park_edit(
        &self,
        issue: u32,
        labels: &[&str],
        remove_loom_issue: bool,
    ) -> Result<(), (bool, String)> {
        let issue_arg = issue.to_string();
        let mut edit: Vec<&str> = vec!["issue", "edit", &issue_arg];
        for l in labels {
            edit.extend(["--add-label", l]);
        }
        if remove_loom_issue {
            edit.extend(["--remove-label", "loom:issue"]);
        }
        let repo_flag = crate::claim_reconciliation::gh_call::loom_repo_flag();
        edit.extend(repo_flag.iter().map(String::as_str));
        match self.gh_write("noop_hold.label", edit) {
            Ok(Some(out)) if out.status.success() => Ok(()),
            Ok(Some(out)) => Err((
                false,
                format!("`gh issue edit` exited {}: {}", out.status, trim_stderr(&out.stderr)),
            )),
            Ok(None) => Err((true, "`gh issue edit` timed out and was killed (#3973)".into())),
            Err(e) => Err((false, format!("`gh issue edit` could not be run: {e}"))),
        }
    }

    /// Best-effort `gh issue comment`.
    fn post_noop_hold_comment(&self, issue: u32, body: &str) {
        let issue_arg = issue.to_string();
        let mut comment = vec!["issue", "comment", &issue_arg, "--body", body];
        let repo_flag = crate::claim_reconciliation::gh_call::loom_repo_flag();
        comment.extend(repo_flag.iter().map(String::as_str));
        if let Err(e) = self.gh_write("noop_hold.comment", comment) {
            log::debug!("sweep_registry: no-op hold comment for #{issue} failed: {e}");
        }
    }

    /// Read one issue as `(state-is-open, body, labels)` over REST.
    fn read_issue_rest(&self, slug: &str, number: u32) -> Option<(bool, String, Vec<String>)> {
        let path = format!("repos/{slug}/issues/{number}");
        let out = self
            .gh_read("noop_hold.issue", ["api", path.as_str(), "--jq", ISSUE_JQ])
            .ok()??;
        if !out.status.success() {
            return None;
        }
        let v: serde_json::Value =
            serde_json::from_str(String::from_utf8_lossy(&out.stdout).trim()).ok()?;
        let open = v.get("state")?.as_str()?.eq_ignore_ascii_case("open");
        let body = v.get("body").and_then(|b| b.as_str()).unwrap_or_default();
        let labels = v
            .get("labels")
            .and_then(|l| l.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default();
        Some((open, body.to_owned(), labels))
    }

    /// Build the [`IssueSnapshot`] for `issue`, or `None` when any part cannot
    /// be read (REST throughout: GraphQL exhaustion is routine at fleet
    /// scale). A dependency that cannot be read is recorded as `?`, which
    /// merely restarts the streak.
    fn fetch_noop_snapshot(&self, issue: u32) -> Option<IssueSnapshot> {
        let (owner, repo) = self.resolve_owner_repo()?;
        let slug = format!("{owner}/{repo}");
        let (open, body, labels) = self.read_issue_rest(&slug, issue)?;
        let comments_path = format!("repos/{slug}/issues/{issue}/comments");
        let out = self
            .gh_read(
                "noop_hold.comments",
                [
                    "api",
                    comments_path.as_str(),
                    "--paginate",
                    "--jq",
                    COMMENTS_JQ,
                ],
            )
            .ok()??;
        if !out.status.success() {
            return None;
        }
        let comments: Vec<String> = String::from_utf8_lossy(&out.stdout)
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(str::to_owned)
            .collect();
        let linked_pr = match self.probe_open_linked_pr(issue) {
            OpenPrProbe::Open(pr) => format!("open:#{pr}"),
            OpenPrProbe::NoneOpen => "none".to_string(),
            OpenPrProbe::ProbeFailed => return None,
        };
        let dependencies = crate::dep_classify::refs::parse_named_blocker_refs(&body, &slug)
            .into_iter()
            .take(MAX_DEPENDENCY_READS)
            .map(|r| {
                let state = r
                    .split_once('#')
                    .and_then(|(s, n)| Some((s, n.parse::<u32>().ok()?)))
                    .and_then(|(s, n)| self.read_issue_rest(s, n))
                    .map_or("?", |(open, _, _)| if open { "open" } else { "closed" });
                (r, state.to_string())
            })
            .collect();
        Some(IssueSnapshot {
            labels,
            open,
            comments,
            linked_pr,
            dependencies,
        })
    }
}

#[cfg(test)]
mod tests;
