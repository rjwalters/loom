//! Return a PR held only by a machine CI gate to review once its exact head is
//! green (#10178).
//!
//! # The stall this closes
//!
//! Judge's CI-failing verdict (`judge-ci-red.md` step 5) leaves
//! `loom:changes-requested` + `loom:ci-failure` on the PR. When the CI later
//! goes green nothing takes them off: head-move invalidation
//! ([`super::verdict_invalidation`]) fires only on a head move and refuses a PR
//! carrying `loom:blocked`; a re-run that goes green on the same SHA, or `main`
//! recovering, moves no head at all; and `stale_blocked.rs` deliberately leaves
//! both labels in place when it unparks. PR #10073 sat about 18.5 hours.
//!
//! # What this pass does
//!
//! For each open PR it clears `loom:ci-failure` and `loom:changes-requested`,
//! and re-adds `loom:review-requested`, only when **every** rule holds:
//!
//! 1. the PR carries both labels and no approval/claim/hold label
//!    (`loom:pr`, `loom:reviewing`, `loom:treating`, `loom:review-requested`,
//!    any hold other than a bare `loom:blocked`);
//! 2. `loom:blocked`, if present, is a machine CI-gate park: the PR body has
//!    park records, every one names no blocker and carries the reason
//!    [`CI_GATE_PARK_REASON`]. A bare `loom:blocked`, a named blocker or any
//!    other reason is a real hold and is left alone;
//! 3. `forge wait-checks` semantics report GREEN for the **exact** head SHA the
//!    listing named (pending, none, unreadable: do nothing);
//! 4. the newest trusted verdict comment is a `changes-requested` verdict that
//!    declares itself a CI-only gate ([`CI_ONLY_MARKER`], or the header of
//!    `judge-ci-red.md`'s template), and no Champion hold is standing;
//! 5. the head is re-read just before the write and still equals the head that
//!    was green.
//!
//! The audit comment is posted first (same order as the stale-verdict clear),
//! then the labels are swapped. Nothing is approved or merged here: the Judge
//! reviews normally afterwards.
//!
//! A sibling file because `claim_reconciliation.rs` is frozen by the file-size
//! ratchet. The decision functions are pure; every forge touch goes through
//! [`Forge`], so the #10073 replay runs against an in-memory fake.

use std::path::Path;
use std::time::Duration;

use anyhow::{anyhow, Result};

use super::open_pr_listing::RestPull;
use super::{gh_call, VerdictKind};
use crate::forge_wait_checks::{self, reads::GhReads, Clock, Outcome, Selector};

/// The park record `reason` of a `loom:blocked` that only mirrors a CI gate.
/// Anything else on a `loom:blocked` PR is a real hold.
pub const CI_GATE_PARK_REASON: &str = "ci-gate";

/// Judge stamps this into a changes-requested verdict whose only finding is a
/// failing CI run (`judge-ci-red.md` step 5).
pub const CI_ONLY_MARKER: &str = "<!-- loom:ci-gate-only -->";

/// The header of that template, which verdicts posted before the marker carry.
const CI_ONLY_HEADER: &str = "Changes Requested - CI Failing";

/// Written by this pass into its audit comment; names the head it cleared for.
pub const RECOVERY_MARKER_PREFIX: &str = "<!-- loom:ci-gate-recovery sha=";

const CI_FAILURE: &str = "loom:ci-failure";
const CHANGES_REQUESTED: &str = "loom:changes-requested";
const BLOCKED: &str = "loom:blocked";
const REVIEW_REQUESTED: &str = "loom:review-requested";

/// A label that means somebody is acting on the PR or has approved it.
const ACTIVE_LABELS: [&str; 4] = [
    "loom:pr",
    "loom:reviewing",
    "loom:treating",
    REVIEW_REQUESTED,
];

/// Operator holds named explicitly, on top of the registry's hold set, so a
/// registry drift can never make this pass clear one.
const OPERATOR_LABELS: [&str; 4] = [
    "loom:operator",
    "loom:operator-only",
    "loom:operator-decision",
    "loom:frozen",
];

/// Why a PR was left alone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Skip {
    /// Does not carry the CI-gate label pair.
    NotCiGated,
    /// Carries an approval, claim or review label.
    Active(String),
    /// A human/operator hold, or any hold other than `loom:blocked`.
    Hold(String),
    /// `loom:blocked` without a machine CI-gate park record.
    BlockedNotCiGate,
    /// The newest trusted verdict is not a CI-only changes-requested one.
    VerdictNotCiOnly,
    /// A Champion hold is standing.
    ChampionHold,
    /// The exact head is not provably green.
    NotGreen(String),
    /// The head moved between the green read and the write.
    HeadMoved,
    /// A forge read the decision needs failed (fail closed).
    Unreadable(&'static str),
}

/// What a clean pass over one PR concluded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Plan {
    /// Clear the gate; `clear_blocked` when the `loom:blocked` is a CI-gate park.
    Recover {
        clear_blocked: bool,
    },
    Skip(Skip),
}

/// Rules 1 and 2: decided from the listing row alone (no forge call).
pub fn label_gate(labels: &[String], body: Option<&str>) -> Result<bool, Skip> {
    let has = |l: &str| labels.iter().any(|x| x == l);
    if !(has(CI_FAILURE) && has(CHANGES_REQUESTED)) {
        return Err(Skip::NotCiGated);
    }
    if let Some(l) = ACTIVE_LABELS.iter().find(|l| has(l)) {
        return Err(Skip::Active((*l).to_string()));
    }
    let blocked_names = &crate::work_finder::BLOCKED_HOLD_NAMES;
    let held = labels.iter().find(|l| {
        OPERATOR_LABELS.contains(&l.as_str())
            || (crate::work_finder::HOLD_NAMES.contains(&l.as_str())
                && !blocked_names.contains(&l.as_str()))
    });
    if let Some(l) = held {
        return Err(Skip::Hold(l.clone()));
    }
    if !crate::work_finder::is_blocked_hold(labels) {
        return Ok(false);
    }
    if is_ci_gate_park(body.unwrap_or_default()) {
        Ok(true)
    } else {
        Err(Skip::BlockedNotCiGate)
    }
}

/// Every park record in `body` is the machine CI-gate park: no blocker, the
/// [`CI_GATE_PARK_REASON`] reason. No record at all is not one.
#[must_use]
pub fn is_ci_gate_park(body: &str) -> bool {
    let records = crate::park_record::parse(body);
    !records.is_empty()
        && crate::park_record::blockers(body).is_empty()
        && records.iter().all(|r| {
            r.blocker.is_none() && r.reason.as_deref().map(str::trim) == Some(CI_GATE_PARK_REASON)
        })
}

/// Rule 4, from **trusted** comment bodies oldest first.
pub fn verdict_gate(trusted: &[String]) -> Result<(), Skip> {
    if crate::champion_hold::standing_hold(trusted).is_some() {
        return Err(Skip::ChampionHold);
    }
    let newest = trusted.iter().rev().find_map(|b| {
        let one = std::slice::from_ref(b);
        [VerdictKind::ChangesRequested, VerdictKind::Approved]
            .into_iter()
            .find(|k| super::extract_latest_verdict_sha(one, *k).is_some())
            .map(|k| (k, b))
    });
    match newest {
        Some((VerdictKind::ChangesRequested, b))
            if b.contains(CI_ONLY_MARKER) || b.contains(CI_ONLY_HEADER) =>
        {
            Ok(())
        }
        _ => Err(Skip::VerdictNotCiOnly),
    }
}

/// Whether the listing's own head SHA is green.
pub enum Ci {
    Green,
    NotGreen(String),
}

/// The forge touches the pass needs; production is [`GhForge`].
pub trait Forge {
    /// Is the exact `head` green (every observed and required check)?
    fn ci(&mut self, head: &str) -> Ci;
    /// The PR's head right now, `None` when unreadable.
    fn current_head(&mut self, number: u32) -> Option<String>;
    /// Trusted comment bodies, oldest first; `None` when unreadable.
    fn trusted_comments(&mut self, number: u32) -> Option<Vec<String>>;
    /// Stand down any armed auto-merge/queue entry; a line for the audit comment.
    fn disarm(&mut self, number: u32) -> Option<String>;
    fn comment(&mut self, number: u32, body: &str) -> Result<()>;
    fn swap_labels(&mut self, number: u32, remove: &[&str], add: &[&str]) -> Result<()>;
}

/// Counts for one pass.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Stats {
    pub candidates: usize,
    pub recovered: usize,
}

/// Decide one PR, reading the forge in the cheapest-first order: labels and
/// body (free), the ETag'd check-runs, then comments, then the head re-read.
pub fn plan_one(f: &mut impl Forge, pr: &RestPull) -> Plan {
    let clear_blocked = match label_gate(&pr.labels, pr.body.as_deref()) {
        Ok(b) => b,
        Err(s) => return Plan::Skip(s),
    };
    let Some(head) = pr.head_sha.as_deref().filter(|h| !h.is_empty()) else {
        return Plan::Skip(Skip::Unreadable("listing has no head sha"));
    };
    if let Ci::NotGreen(why) = f.ci(head) {
        return Plan::Skip(Skip::NotGreen(why));
    }
    let Some(comments) = f.trusted_comments(pr.number) else {
        return Plan::Skip(Skip::Unreadable("comments"));
    };
    if let Err(s) = verdict_gate(&comments) {
        return Plan::Skip(s);
    }
    match f.current_head(pr.number) {
        Some(h) if h == head => Plan::Recover { clear_blocked },
        Some(_) => Plan::Skip(Skip::HeadMoved),
        None => Plan::Skip(Skip::Unreadable("head")),
    }
}

fn audit_body(head: &str, clear_blocked: bool, disarm_line: Option<&str>) -> String {
    let blocked = if clear_blocked {
        "\n- removed `loom:blocked`: its park record names no blocker and the reason \
         `ci-gate`, so it only mirrored the CI gate"
    } else {
        ""
    };
    let disarm = disarm_line.map(|l| format!("\n{l}")).unwrap_or_default();
    format!(
        "{RECOVERY_MARKER_PREFIX}{head} -->\n**CI gate cleared** (#10178). The check runs on \
         `{short}` are all green, and the last verdict on this PR was a CI-only \
         changes-requested.\n\n- removed `loom:ci-failure` and `loom:changes-requested`{blocked}\n\
         - added `loom:review-requested`{disarm}\n\nNothing is approved by this: the Judge \
         reviews the PR as usual. No human or operator hold was touched.",
        short = head.chars().take(9).collect::<String>(),
    )
}

/// Carry out one recovery: comment first (skipped when this head's notice is
/// already there), then the label swap.
fn apply(f: &mut impl Forge, pr: &RestPull, head: &str, clear_blocked: bool) -> Result<()> {
    let disarm = f.disarm(pr.number);
    let recorded = f.trusted_comments(pr.number).is_some_and(|c| {
        c.iter()
            .any(|b| b.contains(&format!("{RECOVERY_MARKER_PREFIX}{head} -->")))
    });
    if !recorded || disarm.is_some() {
        f.comment(pr.number, &audit_body(head, clear_blocked, disarm.as_deref()))?;
    }
    let mut remove = vec![CI_FAILURE, CHANGES_REQUESTED];
    if clear_blocked {
        remove.push(BLOCKED);
    }
    f.swap_labels(pr.number, &remove, &[REVIEW_REQUESTED])
}

/// One pass over `open` through `f`.
pub fn run_pass(f: &mut impl Forge, open: &[RestPull]) -> Stats {
    let mut stats = Stats::default();
    for pr in open {
        if label_gate(&pr.labels, pr.body.as_deref()).is_ok() {
            stats.candidates += 1;
        }
        match plan_one(f, pr) {
            Plan::Recover { clear_blocked } => {
                let head = pr.head_sha.clone().unwrap_or_default();
                match apply(f, pr, &head, clear_blocked) {
                    Ok(()) => {
                        stats.recovered += 1;
                        log::info!(
                            "claim_reconciliation: PR #{} was held only by a CI gate and its head \
                             {head} is green — returned to loom:review-requested (#10178)",
                            pr.number
                        );
                    }
                    Err(e) => log::warn!(
                        "claim_reconciliation: failed to return PR #{} to review after its CI \
                         gate cleared: {e}",
                        pr.number
                    ),
                }
            }
            Plan::Skip(Skip::NotCiGated) => {}
            Plan::Skip(s) => log::debug!(
                "claim_reconciliation: PR #{} stays on its CI gate: {s:?} (#10178)",
                pr.number
            ),
        }
    }
    stats
}

/// The production pass for one registered `root`. Gated by the stale-verdict
/// switch ([`super::VERDICT_STALENESS_ENABLED_ENV`]): off means no writes.
pub(super) fn reconcile(gh_bin: &Path, root: &Path, open: &[RestPull]) -> Stats {
    if !super::verdict_staleness_enabled() {
        return Stats::default();
    }
    // Nothing gated: no reader is even built.
    if !open
        .iter()
        .any(|p| label_gate(&p.labels, p.body.as_deref()).is_ok())
    {
        return Stats::default();
    }
    run_pass(&mut GhForge::new(gh_bin, root), open)
}

/// A clock for a single-poll `wait`: time never advances, so `--timeout 0`
/// semantics apply — the first poll is the only one.
struct OnePoll;

impl Clock for OnePoll {
    fn elapsed(&self) -> Duration {
        Duration::ZERO
    }
    fn sleep(&mut self, _: Duration) {}
}

/// [`Forge`] over `gh`.
pub struct GhForge<'a> {
    gh_bin: &'a Path,
    root: &'a Path,
    reads: Option<Option<GhReads>>,
}

impl<'a> GhForge<'a> {
    fn new(gh_bin: &'a Path, root: &'a Path) -> Self {
        Self {
            gh_bin,
            root,
            reads: None,
        }
    }

    fn reads(&mut self) -> Option<&mut GhReads> {
        let (gh_bin, root) = (self.gh_bin, self.root);
        self.reads
            .get_or_insert_with(|| {
                let repo = std::env::var("LOOM_REPO").ok().filter(|s| !s.is_empty());
                let store = crate::forge_etag_store::daemon_store_dir();
                GhReads::new(
                    gh_bin.to_path_buf(),
                    Some(root.to_path_buf()),
                    store.clone().unwrap_or_default(),
                    repo.as_deref(),
                )
                .map(|r| {
                    let r = r.with_caller("claim_reconciliation.ci_gate");
                    if store.is_some() {
                        r
                    } else {
                        r.ephemeral()
                    }
                })
                .map_err(|e| log::debug!("ci gate recovery: no reads for {}: {e}", root.display()))
                .ok()
            })
            .as_mut()
    }
}

impl Forge for GhForge<'_> {
    fn ci(&mut self, head: &str) -> Ci {
        if crate::rate_limit_breaker::global_skip_pass("claim_reconciliation") {
            return Ci::NotGreen("rate-limit breaker suppressing reads".into());
        }
        let Some(reads) = self.reads() else {
            return Ci::NotGreen("no forge reader".into());
        };
        let opts = forge_wait_checks::Opts {
            timeout: Duration::ZERO,
            required_only: false,
            settle_red: false,
            base: None,
            min_interval: forge_wait_checks::DEFAULT_MIN_INTERVAL,
            max_interval: forge_wait_checks::DEFAULT_MAX_INTERVAL,
            settle_polls: crate::merge_pr::zero_checks::settle_polls(None),
            settle_interval: crate::merge_pr::zero_checks::settle_interval(
                None,
                forge_wait_checks::DEFAULT_MIN_INTERVAL,
            ),
            no_run_grace: Duration::from_secs(forge_wait_checks::no_run::DEFAULT_GRACE),
        };
        let (outcome, _) =
            forge_wait_checks::wait(reads, &Selector::Sha(head.to_string()), &opts, &mut OnePoll);
        match outcome {
            Outcome::Green { sha } if sha == head => Ci::Green,
            other => Ci::NotGreen(other.sentinel()),
        }
    }

    fn current_head(&mut self, number: u32) -> Option<String> {
        self.reads()?.pull(number).ok().map(|h| h.sha)
    }

    fn trusted_comments(&mut self, number: u32) -> Option<Vec<String>> {
        super::forge::fetch_comment_bodies(self.gh_bin, self.root, number)
    }

    fn disarm(&mut self, number: u32) -> Option<String> {
        super::auto_merge_disarm::disarm_before_invalidation(self.gh_bin, self.root, number)
    }

    fn comment(&mut self, number: u32, body: &str) -> Result<()> {
        let n = number.to_string();
        let out = gh_call::output(
            gh_call::write("ci_gate.recovery_comment", self.gh_bin, self.root)
                .args(["pr", "comment", &n, "--body", body])
                .args(gh_call::loom_repo_flag()),
        )?;
        if out.status.success() {
            Ok(())
        } else {
            Err(anyhow!("gh pr comment failed for #{n}: {}", gh_call::stderr(&out)))
        }
    }

    fn swap_labels(&mut self, number: u32, remove: &[&str], add: &[&str]) -> Result<()> {
        let n = number.to_string();
        let mut inv = gh_call::write("ci_gate.recovery_labels", self.gh_bin, self.root)
            .args(["pr", "edit", &n]);
        for l in remove {
            inv = inv.args(["--remove-label", l]);
        }
        for l in add {
            inv = inv.args(["--add-label", l]);
        }
        let out = gh_call::output(inv.args(gh_call::loom_repo_flag()))?;
        if out.status.success() {
            Ok(())
        } else {
            Err(anyhow!("gh pr edit failed for #{n}: {}", gh_call::stderr(&out)))
        }
    }
}

#[cfg(test)]
#[path = "ci_gate_recovery_tests.rs"]
mod tests;
