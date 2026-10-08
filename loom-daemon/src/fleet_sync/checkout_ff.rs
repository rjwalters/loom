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
//! | 5 | the remote answers (asked only when due, see below) | `fetch-failed` |
//! | 6 | nothing behind, nothing ahead | `current` (done) |
//! | 7 | ahead only: unpushed local commits | `ahead` |
//! | 8 | ahead and behind | `diverged` |
//! | 9 | no staged or unstaged change to any tracked file | `dirty` |
//! | 10 | `merge --ff-only` succeeds | `would-overwrite`, else `git-failure` |
//! | | | `fast-forwarded` |
//!
//! With `fleet.autoApply` off, row 10 is not attempted and a clean checkout
//! that is behind reports `behind`.
//!
//! # What a pass costs
//!
//! * A host with `fleet.autoApply` off, or with dispatch paused, makes **no
//!   network call at all**. It compares the checkout with the clone's own
//!   `origin/<default>` ref as it stands.
//! * Otherwise the remote is asked for its head with one `git ls-remote`, and
//!   only when nothing has confirmed `origin/<default>` within
//!   [`super::workspace_resync::recheck_after`] (15 sync intervals). The
//!   workspace half's own probes count ([`note_remote_head`]), so a repo both
//!   halves cover is asked once, not twice.
//! * `git fetch` runs only when the head the remote named is not already the
//!   clone's `origin/<default>`.
//! * A resync this host pushed moved `origin/<default>` in this clone, so the
//!   fast-forward to it costs one `git ls-remote` and no fetch. On another
//!   host the workspace half probes a stale repo every tick and fetches when
//!   its head moves; this step follows that ref and asks nothing itself.
//! * A pass stops at its budget ([`STARTUP_BUDGET`], [`PASS_BUDGET`]); the
//!   next one starts where it stopped. Three remotes in a row that do not
//!   answer end the pass's network use. Every git child has a timeout.
//!
//! So a commit that is not a resync reaches a clean checkout within 15 sync
//! intervals, not one. A resync reaches it within one.
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
pub(super) use host::{after_resync, announce, note_remote_head, startup};
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
    /// Clean and behind, on a host with `fleet.autoApply` off.
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

/// When the remote last named `head` as `branch`'s head.
#[derive(Debug, Clone)]
struct Probed {
    branch: String,
    head: String,
    at: DateTime<Utc>,
}

/// Per-checkout state kept in process memory, so a state is reported when it
/// changes and not once per pass. It resets on restart: a restarted daemon
/// reports each standing skip state once more.
#[derive(Debug, Default)]
pub struct Memory {
    seen: HashMap<PathBuf, Seen>,
    /// This step's own probes, so a settled checkout is not asked about again
    /// until the recheck window has passed.
    probed: HashMap<PathBuf, Probed>,
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

/// A commit the remote named as a branch's head, and when it did.
pub type Confirmed = (String, DateTime<Utc>);

/// Everything a pass reads from outside itself. Production builds it in
/// [`host`]; tests inject each piece.
pub struct Env<'a> {
    /// May this pass fast-forward (`fleet.autoApply`)? Off: report only.
    pub write: bool,
    /// Is a main-health gate run building in this root right now?
    pub gate_in_flight: &'a dyn Fn(&Path) -> bool,
    /// Take the hold that keeps a self-update from starting in this root
    /// while it is moved. `None`: one is already running there.
    pub hold: &'a dyn Fn(&Path) -> Option<MoveHold>,
    /// May this pass use the network at all? Not with `fleet.autoApply` off,
    /// and not on a paused host.
    pub network: bool,
    /// How long a confirmed remote head is trusted before the remote is asked
    /// again.
    pub recheck: Duration,
    /// The head the workspace half last heard the remote name for this root
    /// and branch, and when.
    pub confirmed: &'a dyn Fn(&Path, &str) -> Option<Confirmed>,
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
    /// Remotes in a row that did not answer.
    misses: u32,
}

/// A stable per-root offset in `[0, window)`, so sixty checkouts first asked
/// about in one pass are not all asked again in the same later pass.
fn spread(root: &Path, window: Duration) -> chrono::Duration {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    root.hash(&mut hasher);
    let secs = hasher.finish() % window.as_secs().max(1);
    chrono::Duration::seconds(i64::try_from(secs).unwrap_or(0))
}

/// Run one checkout pass over `roots`. Never fails: every failure is a state.
pub fn run(env: &Env<'_>, roots: &[PathBuf], memory: &mut Memory) -> CheckoutPass {
    let mut pass = CheckoutPass::default();
    memory.seen.retain(|root, _| roots.contains(root));
    memory.probed.retain(|root, _| roots.contains(root));
    let mut net = Net::default();
    let start = memory.cursor % roots.len().max(1);
    let mut reached = 0;
    for root in roots.iter().cycle().skip(start).take(roots.len()) {
        if env.out_of_budget() {
            break;
        }
        let found = attempt(env, root, memory, &mut net);
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
    (pass.probes, pass.fetches) = (net.probes, net.fetches);
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

/// Rule 5: make sure `origin/<branch>` is the remote's head, asking the
/// remote only when nothing has confirmed it within the recheck window.
fn refresh(
    env: &Env<'_>,
    root: &Path,
    branch: &str,
    memory: &mut Memory,
    net: &mut Net,
) -> Result<(), String> {
    if !env.network || net.misses >= UNREACHABLE_STOP {
        return Ok(());
    }
    let now = (env.clock)();
    let window = chrono::Duration::from_std(env.recheck).unwrap_or(chrono::Duration::MAX);
    let tracking = git::tracking_head(root, branch, env.bound(git::LOCAL_TIMEOUT));
    let own = memory
        .probed
        .get(root)
        .filter(|p| p.branch == branch)
        .map(|p| (p.head.clone(), p.at));
    // Confirmed: the remote named this very commit as the head, recently.
    let confirmed = [own, (env.confirmed)(root, branch)]
        .into_iter()
        .flatten()
        .any(|(head, at)| Some(&head) == tracking.as_ref() && now >= at && now - at < window);
    if confirmed {
        return Ok(());
    }
    net.probes += 1;
    let head = git::remote_head(root, branch, env.bound(git::PROBE_TIMEOUT)).inspect_err(|_| {
        net.misses += 1;
    })?;
    net.misses = 0;
    if tracking.as_deref() != Some(head.as_str()) {
        net.fetches += 1;
        git::fetch(root, branch, env.bound(git::FETCH_TIMEOUT))?;
    }
    // The first confirmation is back-dated by a per-root offset.
    let at = if memory.probed.contains_key(root) {
        now
    } else {
        now - spread(root, env.recheck)
    };
    let branch = branch.to_string();
    memory
        .probed
        .insert(root.to_path_buf(), Probed { branch, head, at });
    Ok(())
}

/// The rules, in order, for one checkout.
fn attempt(env: &Env<'_>, root: &Path, memory: &mut Memory, net: &mut Net) -> Decided {
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
    if let Err(e) = refresh(env, root, &branch, memory, net) {
        let _ = found.measure(env, root, &branch);
        return found.end(S::FetchFailed, e);
    }

    // 6-8. Compare.
    if let Err(e) = found.measure(env, root, &branch) {
        return found.end(S::GitFailure, e);
    }
    let (ahead, behind) = (found.ahead.unwrap_or(0), found.behind.unwrap_or(0));
    match (ahead, behind) {
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
