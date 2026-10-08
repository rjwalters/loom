//! The checkout half of fleet-sync: fast-forward each registered workspace's
//! main checkout to its default branch (#10869, tracker #10698).
//!
//! # Why
//!
//! A Loom resync ([`super::workspace_resync`], #10718) commits the new
//! installed files to a repo's default branch **on the forge**. Each host
//! dispatches from its **own main checkout**: the spawn script and the role
//! prompts are read from that working tree. Nothing else in the daemon
//! advances it, so without this step a host keeps running the old installed
//! files after the resync has landed.
//!
//! # What a pass does
//!
//! For every registered workspace, in order, the first rule that matches ends
//! the attempt:
//!
//! | # | Check | State when it fails |
//! |---|---|---|
//! | 1 | `origin/HEAD` names the default branch (never a guessed `main`) | `no-default-branch` |
//! | 2 | HEAD is on it (not another branch, not detached) | `wrong-branch` |
//! | 3 | no rebase, merge, cherry-pick, bisect or revert in progress | `mid-operation` |
//! | 4 | no main-health gate run is building in this checkout | `gate-in-flight` |
//! | 4 | the daemon's self-update is not running in this checkout | `self-update-in-flight` |
//! | 5 | `origin/<default>` is the remote's head (see below) | `fetch-failed` |
//! | 6 | nothing behind, nothing ahead | `current` (done) |
//! | 7 | ahead only: unpushed local commits | `ahead` |
//! | 8 | ahead and behind | `diverged` |
//! | 9 | no staged or unstaged change to any tracked file | `dirty` |
//! | 10 | `merge --ff-only` succeeds | `would-overwrite`, else `git-failure` |
//! | | | `fast-forwarded` |
//!
//! With `fleet.autoApply` off, row 10 is not attempted and a clean checkout
//! that is behind reports `behind`. The same holds, with `fleet.autoApply`
//! on, while the host is paused (see "When nothing is written").
//!
//! Row 5 has one more outcome that is not a failure: a fetch that is due on a
//! volume below the free-space floor is not run, and a checkout with nothing
//! else to report is `low-disk` (see "What a pass costs").
//!
//! # When nothing is written
//!
//! "Paused" means everything on the host is left as it is, in a state a
//! resume can pick up. So the fast-forward, though it is only a local write,
//! follows the pause just as the network does. With `fleet.autoApply` on, row
//! 10 is still not attempted while any of these holds
//! ([`HostGateInputs::write_hold`](super::workspace_resync::HostGateInputs::write_hold)):
//!
//! * dispatch is paused: a drain, a roll's pause or a fleet hold (`draining`);
//! * a pause roll is armed, committed or in progress (`roll pending`);
//! * a pause manifest found at startup is not yet finished by H5
//!   (`resume-pending`, #11016). This covers the startup pass, which runs
//!   before H5 is even spawned, and every timer pass until H5 ends.
//!
//! The question is asked again immediately before the merge, so a pause that
//! begins during a pass stops the checkouts the pass has not reached. A clean
//! checkout that is behind reports `behind` and names the reason. Nothing is
//! lost: the first pass after the pause ends does the fast-forward.
//!
//! Nothing else about the host stops the write. A host that is merely
//! offline (an outage hold, a remote that does not answer) or not in H0 for
//! another reason (not an official release build, a different binary staged,
//! below the fleet floor, its self-update stalled) asks no remote, and may
//! still fast-forward to what its clone already holds.
//!
//! # What a pass costs
//!
//! Every pass, on every tick, makes sure `origin/<default>` is the remote's
//! head before it compares, and it asks the network as little as it can:
//!
//! * A host the workspace resync keeps off the network makes **no network
//!   call at all** here either: no `ls-remote`, no fetch. It compares the
//!   checkout with the clone's own `origin/<default>` ref as it stands.
//!   Whether it may then fast-forward to that ref is a separate question
//!   (see "When nothing is written"). That is a host with `fleet.autoApply`
//!   off; one that is paused (a drain, a roll's pause or a fleet hold); one
//!   with a resume pending; one not in H0 for any other reason (not an
//!   official release build or not yet verified as one, a roll pending, a
//!   different binary staged, below the fleet floor, its self-update
//!   stalled); and one in an outage hold.
//!
//!   On the timer the step does not decide this itself. It takes the
//!   workspace pass's own decision
//!   ([`WorkspacePass::online`](super::workspace_resync::WorkspacePass::online)),
//!   so the two halves cannot disagree. At startup it applies the same
//!   [`host_gate`](super::workspace_resync::host_gate) to what is knowable at
//!   boot; a fresh daemon is exempt only from `Unverified`, since the startup
//!   pass is the one that verifies.
//! * **No fetch below the free-space floor** (#10995). A fetch on a full
//!   disk leaves a partial `tmp_pack_*` behind, and this step would retry it
//!   every tick. So every fetch asks
//!   [`fetch_headroom::skip_reason`](crate::fetch_headroom::skip_reason)
//!   first, on the timer and at startup, for a root the resync covers and for
//!   one it does not. Below the floor the fetch is not run and not counted,
//!   and the checkout is compared with the clone's ref as it stands. That is
//!   not a failure: it is never `fetch-failed` or `git-failure`, it sets no
//!   backoff, and it does not count toward the three-in-a-row stop. A
//!   checkout with nothing else to report is `low-disk`, as the resync
//!   reports the same skip.
//! * A repo the resync is backing off because its remote did not answer or
//!   refused
//!   ([`WorkspacePass::backing_off`](super::workspace_resync::WorkspacePass::backing_off))
//!   is not asked here either.
//! * A repo the workspace resync asked about in this same pass costs
//!   **nothing**: that pass already learned the head (one batched query per
//!   owner) and fetched it if it moved, and recorded it
//!   ([`note_remote_head`]). This step trusts it. Only if the clone's
//!   `origin/<default>` is somehow not that head does it fetch.
//! * Any other root (the Loom source repo and a non-GitHub origin, which the
//!   resync skips; one the resync did not reach; every root at startup,
//!   before any resync pass) gets one `git ls-remote` per pass, and a `git
//!   fetch` only when the head moved. The rate-limit breaker
//!   is read immediately before each such `ls-remote`, never once up front,
//!   and while it is open nothing more is asked (#11003).
//! * A pass stops starting workspaces at its budget ([`STARTUP_BUDGET`],
//!   [`PASS_BUDGET`], and on the timer whatever is left of the workspace
//!   pass's own deadline); the next pass starts where it stopped. Three
//!   remotes in a row that do not answer end the pass's network use. Every
//!   git child has a timeout.
//!
//! So on a host that may use the network, any commit on the default branch
//! reaches a clean checkout on the next tick, and a resync this host pushed in
//! the same pass. On one that may not, a checkout reaches what its clone
//! already has.
//!
//! # What it never does
//!
//! The only git write is the `merge --ff-only` of row 10 ([`git`]). A
//! checkout that is not clean and strictly behind is left exactly as it is
//! and reported. This is stricter than the main-health gate's own sync
//! (`main_health_gate::prepare_workspace_to_origin_main`), which resets and
//! may therefore ignore some dirt: this step discards nothing, so any tracked
//! change is `dirty`. The gate's sync is unchanged; after a fast-forward its
//! reset is a no-op.
//!
//! # The Loom source checkout
//!
//! The Loom repo's own checkout is fast-forwarded like any other workspace
//! (operator ruling on #10869). Two things keep that from colliding with the
//! self-update loop ([`crate::auto_update`]):
//!
//! - **The loop still never pulls.** Its clean-tree gate and its decisions
//!   are unchanged. What changes is that the checkout it compares the
//!   running binary against now advances on its own, on a host with
//!   `fleet.autoApply` on. That is not by itself a reason to roll: a fleet
//!   host moves when the floor moves (#10885). When a roll does build from
//!   source it builds the commit `loom-daemon-update.sh` would itself have
//!   fast-forwarded to. Both only ever move forward along the default branch.
//! - **A checkout never moves under a running update.** The update script
//!   verifies that the binary it built is a build of the checkout's HEAD, and
//!   treats a mismatch as terminal. [`hold_for_self_update`] is taken around
//!   every run of that script; while it is held this step skips that one
//!   checkout (`self-update-in-flight`), and an update that starts while an
//!   attempt is in progress waits for that one attempt to finish.
//!
//! # Reporting
//!
//! Each workspace's state is in the `fleet-sync-status.json` snapshot and in
//! `loom-daemon status`. A log line and an event on [`TOPIC`] are produced
//! only when a state is entered and when it clears, not once per pass
//! ([`Memory`]). `fetch-failed`, `git-failure` and the two in-flight states
//! are reported only once they have held for [`TRANSIENT_AFTER`] passes in a
//! row. A skipped checkout whose missing commits change an installed Loom
//! file is reported at ERROR.

mod git;
mod host;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use chrono::{DateTime, SecondsFormat, Utc};
use serde::{Deserialize, Serialize};

pub(crate) use host::hold_for_self_update;
pub use host::BUSY_NOTE;
pub(super) use host::{after_resync, announce, note_remote_head, startup};
#[cfg(test)]
pub(in crate::fleet_sync) use host::{boot_step, online_at_boot, timer_step, Stepped};
pub use host::{hold_for_move, GateProbe, MoveHold};

/// Event-bus topic a checkout state transition is published on.
pub const TOPIC: &str = "fleet_sync.checkout";

/// How long the checkout half may run inside the startup pass. Workspaces it
/// has not started by then are done by the timer.
pub const STARTUP_BUDGET: Duration = Duration::from_secs(30);

/// How long a pass on the timer may run before it stops starting workspaces.
/// The next pass starts with the ones it did not reach.
pub const PASS_BUDGET: Duration = Duration::from_secs(45);

/// Remotes in a row that do not answer before a pass stops asking any.
pub const UNREACHABLE_STOP: u32 = 3;

/// Passes in a row a transient state must hold before it is reported.
pub const TRANSIENT_AFTER: u32 = 3;

// ============================================================================
// What a pass reports
// ============================================================================

/// A main checkout's state, as the last pass found it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CheckoutState {
    /// At the default branch's tip.
    Current,
    /// This pass fast-forwarded it.
    FastForwarded,
    /// Clean and behind, and not written to: `fleet.autoApply` is off, or the
    /// host is paused, rolling or has a resume pending.
    Behind,
    /// The default branch does not resolve.
    NoDefaultBranch,
    /// HEAD is on another branch, or detached.
    WrongBranch,
    /// A rebase, merge, cherry-pick, bisect or revert is in progress.
    MidOperation,
    /// A main-health gate run is building in this checkout.
    GateInFlight,
    /// The daemon's self-update is running in this checkout.
    SelfUpdateInFlight,
    /// The fetch failed.
    FetchFailed,
    /// The remote's head is not in the clone, and the fetch for it was not
    /// run because the checkout's volume is below the free-space floor
    /// (#10995). Not a failure.
    LowDisk,
    /// Unpushed local commits, and nothing to fast-forward to.
    Ahead,
    /// Local commits and missing commits.
    Diverged,
    /// A tracked file has a staged or unstaged change.
    Dirty,
    /// A local file sits where an incoming commit writes one.
    WouldOverwrite,
    /// git failed some other way.
    GitFailure,
}

impl CheckoutState {
    /// The label `loom-daemon status` prints.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Current => "current",
            Self::FastForwarded => "fast-forwarded",
            Self::Behind => "behind",
            Self::NoDefaultBranch => "no-default-branch",
            Self::WrongBranch => "wrong-branch",
            Self::MidOperation => "mid-operation",
            Self::GateInFlight => "gate-in-flight",
            Self::SelfUpdateInFlight => "self-update-in-flight",
            Self::FetchFailed => "fetch-failed",
            Self::LowDisk => "low-disk",
            Self::Ahead => "ahead",
            Self::Diverged => "diverged",
            Self::Dirty => "dirty",
            Self::WouldOverwrite => "would-overwrite",
            Self::GitFailure => "git-failure",
        }
    }

    /// Expected to clear by itself: reported only after [`TRANSIENT_AFTER`]
    /// passes in a row.
    #[must_use]
    pub fn is_transient(self) -> bool {
        matches!(
            self,
            Self::FetchFailed | Self::GitFailure | Self::GateInFlight | Self::SelfUpdateInFlight
        )
    }
}

/// One workspace's main checkout, as the last pass found it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CheckoutReport {
    /// The registered root.
    pub root: PathBuf,
    /// Its state.
    pub state: CheckoutState,
    /// The default branch, when it resolved.
    pub branch: Option<String>,
    /// Commits on `origin/<branch>` the checkout lacks, when known.
    pub behind: Option<u64>,
    /// Commits the checkout has that `origin/<branch>` lacks, when known.
    pub ahead: Option<u64>,
    /// The short commit HEAD is at, when readable.
    pub head: Option<String>,
    /// One line: the dirty paths, the branch, git's refusal.
    pub detail: Option<String>,
    /// When the checkout entered this state.
    pub since: DateTime<Utc>,
    /// The missing commits change `.loom/` or `.claude/commands/loom/`: this
    /// host is running stale Loom scripts, not merely stale product code.
    #[serde(default)]
    pub installed_files_behind: bool,
}

/// How loud a transition is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Level {
    /// A fast-forward, or a skip state clearing.
    Info,
    /// A skip state was entered.
    Warn,
    /// A skip state was entered and installed Loom files are behind.
    Error,
}

/// One state change: exactly one log line and one event on [`TOPIC`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Transition {
    /// `entered`, `cleared` or `fast-forwarded`.
    pub kind: &'static str,
    /// How loud.
    pub level: Level,
    /// The skip state last reported for this checkout, if any.
    pub previous: Option<CheckoutState>,
    /// The log line.
    pub line: String,
    /// The checkout as this pass found it.
    #[serde(flatten)]
    pub report: CheckoutReport,
}

/// What one checkout pass found and did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CheckoutPass {
    /// Every workspace the pass reached.
    pub checkouts: Vec<CheckoutReport>,
    /// The state changes to log and publish.
    pub transitions: Vec<Transition>,
    /// Workspaces the budget left for the next pass. They keep their last
    /// report.
    pub deferred: usize,
    /// `git ls-remote` calls this pass made.
    pub probes: u32,
    /// `git fetch` calls this pass made.
    pub fetches: u32,
    /// Fetches this pass did not run because the volume is below the
    /// free-space floor (#10995). Never counted in `fetches`.
    pub low_disk: u32,
}

fn stamp(at: DateTime<Utc>) -> String {
    at.to_rfc3339_opts(SecondsFormat::Secs, true)
}

/// `, 3 behind, 1 ahead` for whichever counts are known and non-zero.
fn counts(report: &CheckoutReport) -> String {
    let mut out = String::new();
    if let Some(n) = report.behind.filter(|n| *n > 0) {
        out.push_str(&format!(", {n} behind"));
    }
    if let Some(n) = report.ahead.filter(|n| *n > 0) {
        out.push_str(&format!(", {n} ahead"));
    }
    out
}

/// The checkout lines of the `Fleet store:` block: every workspace that is
/// not `current`.
#[must_use]
pub fn lines(checkouts: &[CheckoutReport]) -> Vec<String> {
    checkouts
        .iter()
        .filter(|c| c.state != CheckoutState::Current)
        .map(|c| {
            let loom = if c.installed_files_behind {
                ", INSTALLED LOOM FILES BEHIND"
            } else {
                ""
            };
            let detail = c
                .detail
                .as_deref()
                .map_or_else(String::new, |d| format!(" ({d})"));
            format!(
                "  checkout {}: {}{}{loom}{detail}, since {}",
                c.root.display(),
                c.state.as_str(),
                counts(c),
                stamp(c.since)
            )
        })
        .collect()
}

// ============================================================================
// What a host remembers between passes
// ============================================================================

#[derive(Debug, Clone)]
struct Seen {
    state: CheckoutState,
    since: DateTime<Utc>,
    /// Passes in a row in `state`.
    streak: u32,
    /// The skip state last logged and not yet cleared, and whether it was
    /// logged with installed files behind.
    reported: Option<(CheckoutState, bool)>,
    /// The last report, shown again by a pass that does not reach this root.
    report: CheckoutReport,
}

/// Per-checkout state kept in process memory, so a state is reported when it
/// changes and not once per pass. It resets on restart: a restarted daemon
/// reports each standing skip state once more.
#[derive(Debug, Default)]
pub struct Memory {
    seen: HashMap<PathBuf, Seen>,
    /// Where the next pass starts, so a pass that ran out of time does not
    /// starve the workspaces at the end of the list.
    cursor: usize,
}

impl Memory {
    /// Record what this pass found for `root`, and push the transition it
    /// amounts to, if any.
    fn record(
        &mut self,
        root: &Path,
        found: Decided,
        now: DateTime<Utc>,
        out: &mut Vec<Transition>,
    ) -> CheckoutReport {
        let previous = self.seen.get(root);
        let (since, streak) = match previous {
            Some(p) if p.state == found.state => (p.since, p.streak.saturating_add(1)),
            _ => (now, 1),
        };
        let mut reported = previous.and_then(|p| p.reported);
        let was = reported.map(|(state, _)| state);
        let report = CheckoutReport {
            root: root.to_path_buf(),
            state: found.state,
            branch: found.branch,
            behind: found.behind,
            ahead: found.ahead,
            head: found.head,
            detail: found.detail,
            since,
            installed_files_behind: found.installed_files_behind,
        };
        let state = report.state;
        let loom = report.installed_files_behind;
        let mut push = |kind: &'static str, level: Level, line: String| {
            out.push(Transition {
                kind,
                level,
                previous: was,
                line,
                report: report.clone(),
            });
        };
        let shown = root.display();
        if state == CheckoutState::FastForwarded {
            let detail = report.detail.as_deref().unwrap_or_default();
            push("fast-forwarded", Level::Info, format!("{shown}: fast-forwarded {detail}"));
            reported = None;
        } else if state == CheckoutState::Current {
            if let Some(was) = was {
                let line = format!("{shown}: {} cleared; the checkout is current", was.as_str());
                push("cleared", Level::Info, line);
                reported = None;
            }
        } else {
            let due = !state.is_transient() || streak >= TRANSIENT_AFTER;
            // News: a different state, or the same one newly found to be
            // holding back installed Loom files.
            let news = reported.is_none_or(|(s, loud)| s != state || (loom && !loud));
            if due && news {
                let level = match (state, loom) {
                    (CheckoutState::Behind, false) => Level::Info,
                    (CheckoutState::Behind, true) | (_, false) => Level::Warn,
                    (_, true) => Level::Error,
                };
                let line = format!(
                    "{shown}: not fast-forwarded: {}{}{}{}",
                    state.as_str(),
                    counts(&report),
                    if loom {
                        ", installed Loom files are behind"
                    } else {
                        ""
                    },
                    report
                        .detail
                        .as_deref()
                        .map_or_else(String::new, |d| format!(" ({d})")),
                );
                push("entered", level, line);
                reported = Some((state, loom));
            }
        }
        self.seen.insert(
            root.to_path_buf(),
            Seen {
                state,
                since,
                streak,
                reported,
                report: report.clone(),
            },
        );
        report
    }
}

// ============================================================================
// The pass
// ============================================================================

/// Everything a pass reads from outside itself. Production builds it in
/// [`host`]; tests inject each piece.
pub struct Env<'a> {
    /// May this pass fast-forward (`fleet.autoApply`)? Off: report only.
    pub write: bool,
    /// Why this host may not write to a checkout right now, whatever
    /// `fleet.autoApply` says: it is paused, rolling or has a resume pending.
    /// Read immediately before each merge.
    pub held: &'a dyn Fn() -> Option<&'static str>,
    /// Is a main-health gate run building in this root right now?
    pub gate_in_flight: &'a dyn Fn(&Path) -> bool,
    /// Take the hold that keeps a self-update from starting in this root
    /// while it is moved. `None`: one is already running there.
    pub hold: &'a dyn Fn(&Path) -> Option<MoveHold>,
    /// May this pass use the network at all? Only on a host the workspace
    /// resync would itself use it on: `fleet.autoApply` on, in H0 (so not
    /// paused, a release build, not rolling, not below the floor, its
    /// self-update not stalled) and in no outage hold.
    pub network: bool,
    /// Is the workspace resync backing this root off because its remote did
    /// not answer or refused? Then it is not asked here either.
    pub backing_off: &'a dyn Fn(&Path) -> bool,
    /// The head the workspace resync learned for this root and branch in this
    /// same pass, if it asked.
    pub confirmed: &'a dyn Fn(&Path, &str) -> Option<String>,
    /// Is the forge rate-limit breaker open? Read before each `ls-remote`.
    pub breaker_open: &'a dyn Fn() -> bool,
    /// How long the pass may run before it stops starting workspaces.
    pub budget: Option<Duration>,
    /// Time since the pass started.
    pub elapsed: &'a dyn Fn() -> Duration,
    /// The clock.
    pub clock: &'a dyn Fn() -> DateTime<Utc>,
}

impl Env<'_> {
    fn out_of_budget(&self) -> bool {
        self.budget.is_some_and(|b| (self.elapsed)() >= b)
    }

    /// `want`, shortened so a read cannot run past the budget.
    fn bound(&self, want: Duration) -> Duration {
        match self.budget {
            Some(b) => want.min(
                b.saturating_sub((self.elapsed)())
                    .max(Duration::from_secs(1)),
            ),
            None => want,
        }
    }
}

/// What an attempt has learned about a checkout so far.
#[derive(Debug, Default)]
struct Found {
    branch: Option<String>,
    behind: Option<u64>,
    ahead: Option<u64>,
    head: Option<String>,
    installed_files_behind: bool,
}

/// [`Found`] with its state decided: the end of an attempt.
#[derive(Debug)]
struct Decided {
    state: CheckoutState,
    branch: Option<String>,
    behind: Option<u64>,
    ahead: Option<u64>,
    head: Option<String>,
    detail: Option<String>,
    installed_files_behind: bool,
}

impl Found {
    fn end(self, state: CheckoutState, detail: impl Into<Option<String>>) -> Decided {
        Decided {
            state,
            branch: self.branch,
            behind: self.behind,
            ahead: self.ahead,
            head: self.head,
            detail: detail.into(),
            installed_files_behind: self.installed_files_behind,
        }
    }

    /// Fill the counts from the remote-tracking ref as it stands.
    fn measure(&mut self, env: &Env<'_>, root: &Path, branch: &str) -> Result<(), String> {
        let (ahead, behind) = git::ahead_behind(root, branch, env.bound(git::LOCAL_TIMEOUT))?;
        self.ahead = Some(ahead);
        self.behind = Some(behind);
        self.installed_files_behind =
            behind > 0 && git::installed_files_behind(root, branch, env.bound(git::LOCAL_TIMEOUT));
        Ok(())
    }
}

/// A pass's use of the network.
#[derive(Debug, Default)]
struct Net {
    probes: u32,
    fetches: u32,
    /// Fetches not run below the free-space floor.
    low_disk: u32,
    /// Remotes in a row that did not answer.
    misses: u32,
    /// The rate-limit breaker was found open: nothing more is asked.
    breaker_open: bool,
}

/// Run one checkout pass over `roots`. Never fails: every failure is a state.
pub fn run(env: &Env<'_>, roots: &[PathBuf], memory: &mut Memory) -> CheckoutPass {
    let mut pass = CheckoutPass::default();
    memory.seen.retain(|root, _| roots.contains(root));
    let mut net = Net::default();
    let start = memory.cursor % roots.len().max(1);
    let mut reached = 0;
    for root in roots.iter().cycle().skip(start).take(roots.len()) {
        if env.out_of_budget() {
            break;
        }
        let found = attempt(env, root, &mut net);
        memory.record(root, found, (env.clock)(), &mut pass.transitions);
        reached += 1;
    }
    pass.deferred = roots.len() - reached;
    memory.cursor = if pass.deferred == 0 {
        0
    } else {
        log::info!(
            "checkout_ff: the pass's budget is spent; {} workspace(s) keep their last report and \
             the next pass starts with them",
            pass.deferred
        );
        start + reached
    };
    (pass.probes, pass.fetches, pass.low_disk) = (net.probes, net.fetches, net.low_disk);
    // In registry order, whatever order they were visited in. A root no pass
    // has reached yet has nothing to show.
    pass.checkouts = roots
        .iter()
        .filter_map(|root| memory.seen.get(root).map(|seen| seen.report.clone()))
        .collect();
    for t in &pass.transitions {
        match t.level {
            Level::Info => log::info!("checkout_ff: {}", t.line),
            Level::Warn => log::warn!("checkout_ff: {}", t.line),
            Level::Error => log::error!("checkout_ff: {}", t.line),
        }
    }
    pass
}

/// Rule 5: make sure `origin/<branch>` is the remote's head. A head the
/// workspace resync learned in this pass is trusted; otherwise the remote is
/// asked once. Fetches only when the head moved.
///
/// `Ok(Some(reason))`: the head moved, and the fetch for it was not run
/// because the checkout's volume is below the free-space floor (#10995). That
/// is not a failure, and `origin/<branch>` is left as the clone has it.
fn refresh(
    env: &Env<'_>,
    root: &Path,
    branch: &str,
    net: &mut Net,
) -> Result<Option<String>, String> {
    if !env.network || (env.backing_off)(root) {
        return Ok(None);
    }
    let head = match (env.confirmed)(root, branch) {
        Some(head) => head,
        None => {
            if net.misses >= UNREACHABLE_STOP || net.breaker_open {
                return Ok(None);
            }
            // Read now, not once per pass: a refusal a moment ago may have
            // opened it (#11003).
            if (env.breaker_open)() {
                net.breaker_open = true;
                log::info!(
                    "checkout_ff: the forge rate-limit breaker is open; no more remotes are asked \
                     this pass"
                );
                return Ok(None);
            }
            net.probes += 1;
            let head = git::remote_head(root, branch, env.bound(git::PROBE_TIMEOUT))
                .inspect_err(|_| net.misses += 1)?;
            net.misses = 0;
            head
        }
    };
    let tracking = git::tracking_head(root, branch, env.bound(git::LOCAL_TIMEOUT));
    if tracking.as_deref() != Some(head.as_str()) {
        // The one gate every daemon fetch into a managed checkout asks first.
        // Asked here, where the fetch is decided, so it covers the timer and
        // the startup pass, a head the resync learned and one this step asked
        // for itself. Before the count, and with no error: a fetch that is
        // not run is neither a fetch nor a remote that did not answer.
        if let Some(low) = crate::fetch_headroom::skip_reason(root) {
            net.low_disk += 1;
            return Ok(Some(low));
        }
        net.fetches += 1;
        git::fetch(root, branch, env.bound(git::FETCH_TIMEOUT))?;
    }
    Ok(None)
}

/// The rules, in order, for one checkout.
fn attempt(env: &Env<'_>, root: &Path, net: &mut Net) -> Decided {
    use CheckoutState as S;
    let mut found = Found::default();
    let local = || env.bound(git::LOCAL_TIMEOUT);

    // 1. The default branch, from the clone's own `origin/HEAD`: no remote is
    // asked, and `main` is never guessed.
    let Some(branch) = git::default_branch(root, local()) else {
        let why = "origin/HEAD does not name a usable default branch; run `git remote set-head \
                   origin --auto`";
        return found.end(S::NoDefaultBranch, why.to_string());
    };
    found.branch = Some(branch.clone());
    found.head = git::head(root, local());

    // 2. HEAD is on it.
    match git::current_branch(root, local()) {
        Ok(Some(on)) if on == branch => {}
        Ok(on) => {
            let _ = found.measure(env, root, &branch);
            let detail = match on {
                Some(on) => format!("on `{on}`, not `{branch}`"),
                None => format!("detached HEAD, not on `{branch}`"),
            };
            return found.end(S::WrongBranch, detail);
        }
        Err(e) => return found.end(S::GitFailure, e),
    }

    // 3. Nothing in progress. Fails closed.
    if crate::primary_checkout_reaper::in_special_git_state(root) {
        let _ = found.measure(env, root, &branch);
        let why = "a rebase, merge, cherry-pick, bisect or revert is in progress";
        return found.end(S::MidOperation, why.to_string());
    }

    // 4. Nothing is building in this checkout.
    let gated = |found: Found| {
        found.end(S::GateInFlight, "a main-health gate run is building here".to_string())
    };
    if (env.gate_in_flight)(root) {
        return gated(found);
    }
    let Some(_hold) = (env.hold)(root) else {
        let why = "the daemon's self-update is running in this checkout";
        return found.end(S::SelfUpdateInFlight, why.to_string());
    };

    // 5. Ask the remote, when that is due and allowed.
    let low_disk = match refresh(env, root, &branch, net) {
        Ok(low_disk) => low_disk,
        Err(e) => {
            let _ = found.measure(env, root, &branch);
            return found.end(S::FetchFailed, e);
        }
    };

    // 6-8. Compare.
    if let Err(e) = found.measure(env, root, &branch) {
        return found.end(S::GitFailure, e);
    }
    let (ahead, behind) = (found.ahead.unwrap_or(0), found.behind.unwrap_or(0));
    match (ahead, behind) {
        // The clone's own ref is all this pass could compare with: the
        // remote's head is newer, and there was no room to fetch it.
        (0, 0) if low_disk.is_some() => return found.end(S::LowDisk, low_disk),
        (0, 0) => return found.end(S::Current, None),
        (_, 0) => {
            let why = format!("{ahead} unpushed commit(s) on `{branch}`");
            return found.end(S::Ahead, why);
        }
        (0, _) => {}
        _ => {
            let why = format!("`{branch}` and origin/{branch} have diverged");
            return found.end(S::Diverged, why);
        }
    }

    // 9. Clean: no staged or unstaged change to any tracked file.
    match git::tracked_changes(root, local()) {
        Ok(paths) if paths.is_empty() => {}
        Ok(paths) => return found.end(S::Dirty, git::summarize(&paths)),
        Err(e) => return found.end(S::GitFailure, e),
    }
    if !env.write {
        return found.end(S::Behind, "fleet.autoApply is off; nothing written".to_string());
    }
    // The gate may have started since rule 4. Ask again, right before the write.
    if (env.gate_in_flight)(root) {
        return gated(found);
    }
    // And so may a pause. A paused host writes nothing, not even locally.
    if let Some(why) = (env.held)() {
        return found.end(S::Behind, format!("the host is held ({why}); nothing written"));
    }

    // 10. The one write.
    let old = found.head.clone().unwrap_or_else(|| "unknown".to_string());
    match git::fast_forward(root, &branch) {
        Ok(()) => {
            found.head = git::head(root, local());
            let new = found.head.clone().unwrap_or_else(|| "unknown".to_string());
            found.behind = Some(0);
            found.installed_files_behind = false;
            found.end(S::FastForwarded, format!("`{branch}` {old} -> {new} ({behind} commit(s))"))
        }
        Err(git::Refusal::WouldOverwrite(paths)) => {
            found.end(S::WouldOverwrite, format!("a local file is in the way: {paths}"))
        }
        Err(git::Refusal::Other(e)) => found.end(S::GitFailure, e),
    }
}

#[cfg(test)]
#[path = "tests/checkout_ff.rs"]
mod tests;
