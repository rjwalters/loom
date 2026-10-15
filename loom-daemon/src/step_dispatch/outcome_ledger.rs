//! The step-outcome ledger (#11900; design: #11894,
//! `docs/design/work-finder-step-dispatch.md` §5).
//!
//! A step ends in exactly one of #11868's four classes, `verdict | noop |
//! refused | failed`, with a reason token
//! (`defaults/docs/targeted-role-invocation.md`, "Outcome classes"). Champion
//! reports its own sets instead (`champion-targeted.md`: the `--pr` outcomes
//! and the promotion verdict set). This module reads both, folds them onto the
//! four classes, and decides what the engine does next with the `(item, step)`
//! the step ran on. The classes and tokens are #11868's; nothing here adds one.
//!
//! # Outcome → effect
//!
//! | Outcome | [`Effect`] |
//! |---|---|
//! | `verdict`, reason `closed` (landed), on an issue | [`Effect::Landed`]: reclassify; never a Builder step again |
//! | any other `verdict` | [`Effect::Advance`]: the next step comes from the labels |
//! | `noop` | [`Effect::Cooldown`] for [`LedgerConfig::noop_cooldown`], per `(item, step)` |
//! | `refused`, reason `pool_gate` or `claimed` | [`Effect::Cooldown`] for [`LedgerConfig::refusal_cooldown`] |
//! | `refused`, reason `claim_lost` | [`Effect::Redraw`], counted ([`LedgerCounters::claim_lost`]); a **second** one in a row arms a cooldown (below) |
//! | any other `refused` (a state refusal) | [`Effect::Reclassify`]; a **second identical** one in a row arms a cooldown (below) |
//! | `failed` inside the fast-fail window, Builder step | [`Effect::FastFailLadder`]: the #11854/#11872 ladder |
//! | `failed` inside the fast-fail window, any other step | [`Effect::FastFailWindow`]: a ledger-local doubling window, never the issue quarantine |
//! | `failed` later | [`Effect::Failed`]: counted toward the repeat cap only |
//! | Champion `held` / `blocked` | [`Effect::ForgeLadder`]: 6 h → 7 d, on the forge ([`super::reeval_ladder`]) |
//!
//! Champion's other outcomes fold as: `merged`, `rejected`, `promoted`,
//! `revise`, `operator`, `closed` → `verdict`; `requeued` → `noop`;
//! `head-moved`, `not-eligible` → `refused` (a state refusal); `failed` →
//! `failed`.
//!
//! # Repeated state refusals
//!
//! A state refusal usually means the listing was stale, and the next listing
//! fixes it, so the first one only reclassifies. A lasting disagreement
//! between the classifier and the role's own gate (Champion `not-eligible`
//! on a merge-state or CI condition the label classifier cannot see) would
//! otherwise cost a slot and a spawn on every tick on every host. So a
//! second state refusal of the same `(item, step)` with the **same reason**,
//! with no other outcome in between, arms [`LedgerConfig::refusal_cooldown`]
//! (10 min), doubling per further identical refusal up to
//! [`MAX_SHORT_COOLDOWN`], broadcast like every window. A verdict or any
//! other outcome on the pair resets the streak.
//!
//! `claim_lost` rides the same streak guard (#11969): one lost claim is a
//! race and only redraws, but a pair whose claim is lost on every draw (a
//! peer or an attended session holding it for longer than a tick) would
//! otherwise spawn and lose on every tick. A second consecutive `claim_lost`
//! on the pair arms the same doubling refusal cooldown; it still redraws.
//!
//! # Fast failures of non-Builder issue steps
//!
//! Only a Builder step charges the issue fast-fail ladder. Its park restores
//! `loom:issue` on release when the issue ever carried it, which is right for
//! an issue Builder was building and wrong for one Curator or Champion
//! promotion was working (a revised-then-curated issue would come back ready
//! and skip promotion). So Curator and promote fast failures take the same
//! ledger-local window PR steps use, with no forge park (#11969).
//!
//! # The repeat cap
//!
//! Every non-verdict run of an `(item, step)` that actually worked the item is
//! remembered. When [`LedgerConfig::repeat_cap`] of them fall inside
//! [`LedgerConfig::repeat_window`] (3 in 24 h), the pair cools down until the
//! oldest of them leaves the window (the design's pilot-to-fleet gate). A
//! verdict on the item is a state change: it clears every step's history and
//! cooldown for that item.
//!
//! Not runs ([`counts_as_run`]): every `refused` outcome (`pool_gate`,
//! `claimed`, `claim_lost`, a state refusal: the step never worked the item)
//! and a `noop` that waited on something outside the item (`ci_pending`), so
//! a PR sitting in the CI queue does not lose its Judge for a day.
//!
//! **Scope.** The run history is per host and in memory: it is not broadcast
//! and a restart or roll forgets it. The cap a host arms *is* broadcast, but
//! across N hosts an `(item, step)` can run up to about `2N + 1` times in
//! 24 h before any one host caps it. It enforces the gate exactly on one host
//! (the pilot); fleet-wide it is a bound per host.
//!
//! # Durability
//!
//! The ledger is local, in-memory state. Every window it arms is at most
//! [`MAX_SHORT_COOLDOWN`] (24 h, the landed cooldown's own ceiling), and the
//! registry broadcasts each one on the peer channel
//! (`ClaimKind::StepCooldownArmed`), so peers keep honouring a window after
//! this host restarts. The restarted host itself does not get its own windows
//! back (peers never re-send them, and a host ignores its own ads). Nothing
//! longer rides the broadcast: the Champion ladder's
//! rungs past 24 h live on the forge only (the item's body), and after a
//! rollback `release-stale-blocked` owns those parks.
//!
//! # The draw's query
//!
//! [`StepCooldowns`] is the pure answer to "is `(item, step)` cooling down at
//! time T?". The registry builds one per tick from this ledger plus the peers'
//! broadcasts; the cutover adds the forge ladder's due times
//! ([`super::reeval_ladder::due`]); the draw reads it as part of `actionable`
//! (`item_draw::WeighContext::cooldowns`).

use std::collections::BTreeMap;
use std::time::Duration;

use chrono::{DateTime, Utc};

use crate::step_role::StepRole;

/// Ceiling on any window the ledger arms locally or broadcasts: the landed
/// cooldown's 24 h (`sweep_registry::dispatch::landed_cooldown`), so a short
/// cooldown never outlives a day on any host.
pub const MAX_SHORT_COOLDOWN: Duration = Duration::from_secs(24 * 3600);

/// Default `noop` cooldown: the issue-keyed no-op cooldown's default
/// (`noop_cooldown::DEFAULT_NOOP_COOLDOWN_SECS`), applied per `(item, step)`.
pub const DEFAULT_NOOP_COOLDOWN: Duration =
    Duration::from_secs(crate::sweep_registry::DEFAULT_NOOP_COOLDOWN_SECS);

/// Default cooldown after a `pool_gate` or `claimed` refusal: long enough that
/// the next tick does not redraw the step, short enough that a freed pool or a
/// released peer claim is picked up within minutes.
pub const DEFAULT_REFUSAL_COOLDOWN: Duration = Duration::from_secs(10 * 60);

/// Default repeat cap: runs of one `(item, step)` without a state change.
pub const DEFAULT_REPEAT_CAP: usize = 3;

/// Default window the repeat cap counts over.
pub const DEFAULT_REPEAT_WINDOW: Duration = Duration::from_secs(24 * 3600);

/// The outcome line Judge, Builder and Curator end a targeted step with.
pub const OUTCOME_LINE_PREFIX: &str = "LOOM_STEP_OUTCOME";

/// The outcome line Champion ends every mode with.
pub const CHAMPION_LINE_PREFIX: &str = "CHAMPION-STEP";

/// The reason a step's outcome carries when nothing said one: a missing
/// outcome line on an unchanged item, or a verdict the forge does not show.
pub const UNCLASSIFIED: &str = "unclassified";

/// The reason of a step whose forge post-state could not be read.
pub const PROBE_FAILED: &str = "probe_failed";

/// `noop` reasons that waited on something outside the item; they are not
/// runs for the repeat cap.
pub const EXTERNAL_WAIT_NOOPS: [&str; 1] = ["ci_pending"];

/// Base of a PR step's fast-fail window, doubled per consecutive fast
/// failure (the #4485 ladder's default base).
pub const PR_FAST_FAIL_BASE: Duration =
    Duration::from_secs(crate::sweep_registry::DEFAULT_DISPATCH_BACKOFF_BASE_SECS);

/// #11868's four outcome classes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum OutcomeClass {
    Verdict,
    Noop,
    Refused,
    Failed,
}

impl OutcomeClass {
    /// The wire name (`class=` on the outcome line).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Verdict => "verdict",
            Self::Noop => "noop",
            Self::Refused => "refused",
            Self::Failed => "failed",
        }
    }

    fn parse(s: &str) -> Option<Self> {
        match s {
            "verdict" => Some(Self::Verdict),
            "noop" => Some(Self::Noop),
            "refused" => Some(Self::Refused),
            "failed" => Some(Self::Failed),
            _ => None,
        }
    }
}

/// One step's outcome: a class and its reason token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StepOutcome {
    pub class: OutcomeClass,
    /// The reason token (`snake_case`). Champion's outcomes keep their own
    /// word, with `-` read as `_` (`head-moved` → `head_moved`).
    pub reason: String,
}

impl StepOutcome {
    #[must_use]
    pub fn new(class: OutcomeClass, reason: &str) -> Self {
        Self {
            class,
            reason: reason.to_string(),
        }
    }
}

/// Champion's outcome word folded onto the four classes (module doc). `on_pr`
/// is `--pr` mode; `None` for a word outside that mode's set.
#[must_use]
pub fn champion_outcome(on_pr: bool, outcome: &str) -> Option<StepOutcome> {
    use OutcomeClass::{Failed, Noop, Refused, Verdict};
    let class = match (on_pr, outcome) {
        (true, "merged" | "rejected") | (false, "promoted" | "revise" | "operator" | "closed") => {
            Verdict
        }
        (true, "held" | "requeued") | (false, "blocked") => Noop,
        (true, "head-moved") | (_, "not-eligible") => Refused,
        (true, "failed") => Failed,
        _ => return None,
    };
    Some(StepOutcome::new(class, &outcome.replace('-', "_")))
}

/// `key=value` fields of one outcome line.
fn fields(rest: &str) -> BTreeMap<&str, &str> {
    rest.split_whitespace()
        .filter_map(|kv| kv.split_once('='))
        .collect()
}

/// The outcome a step session reported for `(target, step)`: the **last**
/// `LOOM_STEP_OUTCOME` (or, for a Champion step, `CHAMPION-STEP`) line in
/// `output` that names this role and target. A line for another role, mode or
/// target, an unknown class or Champion word, is not this step's. A
/// `LOOM_STEP_OUTCOME` line without `reason=` reads as [`UNCLASSIFIED`].
///
/// Advisory only: [`classify`] decides with the forge post-state.
#[must_use]
pub fn parse_outcome_line(output: &str, step: StepRole, target: u32) -> Option<StepOutcome> {
    let champion = step.role_name() == "champion";
    output.lines().rev().find_map(|line| {
        let line = line.trim();
        if champion {
            let f = fields(line.strip_prefix(CHAMPION_LINE_PREFIX)?);
            let want_mode = if step.acts_on_pr() { "pr" } else { "issue" };
            if f.get("mode") != Some(&want_mode) || f.get("item")?.parse::<u32>().ok()? != target {
                return None;
            }
            champion_outcome(step.acts_on_pr(), f.get("outcome")?)
        } else {
            let f = fields(line.strip_prefix(OUTCOME_LINE_PREFIX)?);
            if f.get("role") != Some(&step.role_name())
                || f.get("target")?.parse::<u32>().ok()? != target
            {
                return None;
            }
            let class = OutcomeClass::parse(f.get("class")?)?;
            Some(StepOutcome::new(class, f.get("reason").copied().unwrap_or(UNCLASSIFIED)))
        }
    })
}

/// The forge post-state a classifier reads after the session exits
/// (`targeted-role-invocation.md`, "How the daemon tells them apart"). The
/// caller computes it per role from one read of the item.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PostState {
    /// The item moved the way a verdict of this role moves it (a new label,
    /// a park, a linked PR, ...).
    pub moved: bool,
    /// The item was closed during the step.
    pub closed: bool,
    /// The step's claim (label or lease) is still held after exit.
    pub claim_held: bool,
}

/// The step's outcome: the forge post-state decides, the outcome line only
/// chooses between classes the post-state allows.
///
/// - No post-state (`None`: the forge read failed) is `failed`, reason
///   `probe_failed`: the session's own line is never trusted alone.
/// - A claim still held after exit is `failed`.
/// - A moved or closed item is a `verdict` (reason: the line's, else `closed`
///   for a closed item, else [`UNCLASSIFIED`]).
/// - An unchanged item takes the line's class, except that a `verdict` claim
///   the forge does not show is `failed`; no line at all is `failed`
///   ([`UNCLASSIFIED`]).
#[must_use]
pub fn classify(line: Option<&StepOutcome>, post: Option<PostState>) -> StepOutcome {
    let failed_unclassified = StepOutcome::new(OutcomeClass::Failed, UNCLASSIFIED);
    let Some(post) = post else {
        return StepOutcome::new(OutcomeClass::Failed, PROBE_FAILED);
    };
    if post.claim_held {
        return match line {
            Some(l) if l.class == OutcomeClass::Failed => l.clone(),
            _ => failed_unclassified,
        };
    }
    if post.moved || post.closed {
        let reason = match line {
            Some(l) if !(post.closed && l.reason == UNCLASSIFIED) => l.reason.as_str(),
            _ if post.closed => "closed",
            _ => UNCLASSIFIED,
        };
        return StepOutcome::new(OutcomeClass::Verdict, reason);
    }
    match line {
        Some(l) if l.class != OutcomeClass::Verdict => l.clone(),
        _ => failed_unclassified,
    }
}

/// What the engine does with an `(item, step)` after its step ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Effect {
    /// A verdict moved the item: the next step comes from its labels.
    Advance,
    /// The item's issue was closed (landed): reclassify it, and never draw a
    /// Builder step on it again.
    Landed,
    /// A state refusal: the listing was stale; reclassify the item.
    Reclassify,
    /// A short `(item, step)` cooldown of this length.
    Cooldown(Duration),
    /// A fast failure on a Builder step: charge the #11854 fast-fail ladder.
    FastFailLadder,
    /// A fast failure on any other step (a PR step, Curator, Champion
    /// promotion): a ledger-local window doubling per
    /// consecutive fast failure from [`PR_FAST_FAIL_BASE`], capped at
    /// [`MAX_SHORT_COOLDOWN`]. Never the issue quarantine, whose park and
    /// release (`loom:issue` restored) are issue-only. The base and cap are
    /// fixed values, not the `dispatchBackoff` config: this is the ledger's
    /// own brake, so turning `dispatchBackoff` off does not turn it off.
    FastFailWindow,
    /// A slow failure: counted toward the repeat cap only (the watchdogs own
    /// slow deaths).
    Failed,
    /// Champion re-evaluation left the item held or blocked: advance the
    /// 6 h → 7 d ladder on the forge.
    ForgeLadder,
    /// The claim was lost to a peer: redraw, and count it. A second one in a
    /// row on the pair arms the refusal cooldown (module doc).
    Redraw,
}

/// The ledger's knobs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LedgerConfig {
    pub noop_cooldown: Duration,
    pub refusal_cooldown: Duration,
    pub repeat_cap: usize,
    pub repeat_window: Duration,
    /// A `failed` step that ran less than this is a fast failure.
    pub fast_fail_window: Duration,
}

impl Default for LedgerConfig {
    fn default() -> Self {
        Self {
            noop_cooldown: DEFAULT_NOOP_COOLDOWN,
            refusal_cooldown: DEFAULT_REFUSAL_COOLDOWN,
            repeat_cap: DEFAULT_REPEAT_CAP,
            repeat_window: DEFAULT_REPEAT_WINDOW,
            fast_fail_window: Duration::from_secs(
                u64::try_from(crate::sweep_registry::FAST_FAIL_WINDOW_SECS).unwrap_or(120),
            ),
        }
    }
}

/// The effect of `outcome` on a `step` that ran for `ran_for`. Pure.
#[must_use]
pub fn effect_of(
    step: StepRole,
    outcome: &StepOutcome,
    ran_for: Duration,
    cfg: &LedgerConfig,
) -> Effect {
    let reason = outcome.reason.as_str();
    if step.role_name() == "champion" && matches!(reason, "held" | "blocked") {
        return Effect::ForgeLadder;
    }
    match outcome.class {
        OutcomeClass::Verdict if reason == "closed" && !step.acts_on_pr() => Effect::Landed,
        OutcomeClass::Verdict => Effect::Advance,
        OutcomeClass::Noop => Effect::Cooldown(cfg.noop_cooldown),
        OutcomeClass::Refused => match reason {
            "pool_gate" | "claimed" => Effect::Cooldown(cfg.refusal_cooldown),
            "claim_lost" => Effect::Redraw,
            _ => Effect::Reclassify,
        },
        OutcomeClass::Failed if ran_for < cfg.fast_fail_window && step == StepRole::Builder => {
            Effect::FastFailLadder
        }
        OutcomeClass::Failed if ran_for < cfg.fast_fail_window => Effect::FastFailWindow,
        OutcomeClass::Failed => Effect::Failed,
    }
}

/// Why an `(item, step)` is cooling down.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum CooldownCause {
    Noop,
    Refused,
    FastFail,
    Landed,
    RepeatCap,
    ForgeLadder,
}

/// Counters the ledger keeps (for `pick.decision` and the gates).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LedgerCounters {
    /// Outcomes recorded, by class.
    pub by_class: BTreeMap<OutcomeClass, u64>,
    /// `refused` / `claim_lost` redraws.
    pub claim_lost: u64,
    /// Times the repeat cap armed a cooldown.
    pub repeat_capped: u64,
}

#[derive(Debug, Clone, Default)]
struct Entry {
    /// Non-verdict runs inside the repeat window, oldest first.
    runs: Vec<DateTime<Utc>>,
    /// The cooldown in force, if any.
    until: Option<(DateTime<Utc>, CooldownCause)>,
    /// Consecutive fast failures of a non-Builder step ([`Effect::FastFailWindow`]):
    /// any other outcome on the pair resets it.
    fast_fails: u32,
    /// The last state refusal's (or `claim_lost`'s) reason, how many
    /// identical ones in a row, and when the last one was recorded (module
    /// doc, "Repeated state refusals"). Any other outcome on the pair resets
    /// it.
    refusals: Option<(String, u32, DateTime<Utc>)>,
}

/// Whether `outcome` is a run for the repeat cap (module doc).
#[must_use]
pub fn counts_as_run(outcome: &StepOutcome) -> bool {
    match outcome.class {
        OutcomeClass::Refused => false,
        OutcomeClass::Noop => !EXTERNAL_WAIT_NOOPS.contains(&outcome.reason.as_str()),
        OutcomeClass::Verdict | OutcomeClass::Failed => true,
    }
}

/// What [`OutcomeLedger::record`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Recorded {
    pub effect: Effect,
    /// The `(item, step)`'s cooldown in force after the record, if any.
    pub cooling_until: Option<DateTime<Utc>>,
    /// The repeat cap armed (or extended) the cooldown.
    pub repeat_capped: bool,
    /// Steps of the item whose **local** cooldown a verdict lifted. The
    /// registry lifts every step's cooldown fleet-wide regardless.
    pub lifted: Vec<StepRole>,
}

fn add(at: DateTime<Utc>, d: Duration) -> DateTime<Utc> {
    at + chrono::Duration::from_std(d).unwrap_or_else(|_| chrono::Duration::zero())
}

/// The per-`(item, step)` outcome ledger of one workspace.
#[derive(Debug, Clone, Default)]
pub struct OutcomeLedger {
    cfg: LedgerConfig,
    entries: BTreeMap<(u32, StepRole), Entry>,
    counters: LedgerCounters,
}

impl OutcomeLedger {
    #[must_use]
    pub fn new(cfg: LedgerConfig) -> Self {
        Self {
            cfg,
            ..Self::default()
        }
    }

    #[must_use]
    pub fn config(&self) -> &LedgerConfig {
        &self.cfg
    }

    #[must_use]
    pub fn counters(&self) -> &LedgerCounters {
        &self.counters
    }

    /// Record `outcome` for `(item, step)`, which ran for `ran_for` and ended
    /// at `now`, and apply its local effect: a verdict clears the item's
    /// history, a cooldown is armed, a run is counted toward the repeat cap.
    /// The forge-side and ladder effects are the caller's (the registry).
    pub fn record(
        &mut self,
        item: u32,
        step: StepRole,
        outcome: &StepOutcome,
        ran_for: Duration,
        now: DateTime<Utc>,
    ) -> Recorded {
        *self.counters.by_class.entry(outcome.class).or_default() += 1;
        let effect = effect_of(step, outcome, ran_for, &self.cfg);
        let mut lifted = Vec::new();
        let mut repeat_capped = false;
        let counted = counts_as_run(outcome);
        match &effect {
            Effect::Advance | Effect::Landed => {
                let keys: Vec<_> = self
                    .entries
                    .range((item, StepRole::ALL[0])..)
                    .map(|(k, _)| *k)
                    .take_while(|(i, _)| *i == item)
                    .collect();
                for key in keys {
                    if let Some(e) = self.entries.remove(&key) {
                        if e.until.is_some_and(|(u, _)| u > now) {
                            lifted.push(key.1);
                        }
                    }
                }
                if effect == Effect::Landed {
                    self.arm(
                        item,
                        StepRole::Builder,
                        add(now, MAX_SHORT_COOLDOWN),
                        CooldownCause::Landed,
                        now,
                    );
                }
            }
            Effect::Redraw => {
                self.counters.claim_lost += 1;
                self.refusal_streak(item, step, &outcome.reason, now);
            }
            Effect::Cooldown(d) => {
                let cause = if outcome.class == OutcomeClass::Noop {
                    CooldownCause::Noop
                } else {
                    CooldownCause::Refused
                };
                self.arm(item, step, add(now, *d), cause, now);
            }
            Effect::FastFailWindow => {
                let entry = self.entries.entry((item, step)).or_default();
                entry.fast_fails = entry.fast_fails.saturating_add(1);
                let delay = crate::sweep_registry::backoff_delay(
                    entry.fast_fails,
                    PR_FAST_FAIL_BASE,
                    MAX_SHORT_COOLDOWN,
                );
                self.arm(item, step, add(now, delay), CooldownCause::FastFail, now);
            }
            Effect::Reclassify => self.refusal_streak(item, step, &outcome.reason, now),
            Effect::FastFailLadder | Effect::Failed | Effect::ForgeLadder => {}
        }
        if let Some(entry) = self.entries.get_mut(&(item, step)) {
            if effect != Effect::FastFailWindow {
                entry.fast_fails = 0;
            }
            if !matches!(effect, Effect::Reclassify | Effect::Redraw) {
                entry.refusals = None;
            }
        }
        if counted && !matches!(effect, Effect::Advance | Effect::Landed) {
            repeat_capped = self.count_run(item, step, now);
        }
        Recorded {
            effect,
            cooling_until: self.cooling_until(item, step, now),
            repeat_capped,
            lifted,
        }
    }

    /// Extend the pair's refusal streak by one `reason` refusal (a different
    /// reason starts a new streak); from the second identical one in a row,
    /// arm the refusal cooldown, doubling per further one, capped at
    /// [`MAX_SHORT_COOLDOWN`] (module doc, "Repeated state refusals").
    fn refusal_streak(&mut self, item: u32, step: StepRole, reason: &str, now: DateTime<Utc>) {
        let cooldown = self.cfg.refusal_cooldown;
        let entry = self.entries.entry((item, step)).or_default();
        let streak = match &entry.refusals {
            Some((last, n, _)) if last == reason => n.saturating_add(1),
            _ => 1,
        };
        entry.refusals = Some((reason.to_string(), streak, now));
        if streak >= 2 {
            let delay =
                crate::sweep_registry::backoff_delay(streak - 1, cooldown, MAX_SHORT_COOLDOWN);
            self.arm(item, step, add(now, delay), CooldownCause::Refused, now);
        }
    }

    /// Count one non-verdict run; arm the repeat-cap cooldown when the window
    /// holds `repeat_cap` of them. Returns whether it armed.
    fn count_run(&mut self, item: u32, step: StepRole, now: DateTime<Utc>) -> bool {
        let window = self.cfg.repeat_window;
        let cap = self.cfg.repeat_cap.max(1);
        let entry = self.entries.entry((item, step)).or_default();
        entry.runs.retain(|at| add(*at, window) > now);
        entry.runs.push(now);
        if entry.runs.len() < cap {
            return false;
        }
        let oldest = entry.runs[entry.runs.len() - cap];
        self.arm(item, step, add(oldest, window), CooldownCause::RepeatCap, now);
        self.counters.repeat_capped += 1;
        true
    }

    /// Arm `(item, step)` until `until`, capped at `now + MAX_SHORT_COOLDOWN`.
    /// Extend-only: a later, shorter window never shortens one in force.
    /// Returns the window in force.
    pub fn arm(
        &mut self,
        item: u32,
        step: StepRole,
        until: DateTime<Utc>,
        cause: CooldownCause,
        now: DateTime<Utc>,
    ) -> DateTime<Utc> {
        let until = until.min(add(now, MAX_SHORT_COOLDOWN));
        let entry = self.entries.entry((item, step)).or_default();
        match entry.until {
            Some((prev, _)) if prev >= until => prev,
            _ => {
                entry.until = Some((until, cause));
                until
            }
        }
    }

    /// When `(item, step)`'s local cooldown ends, if it is cooling at `at`.
    #[must_use]
    pub fn cooling_until(
        &self,
        item: u32,
        step: StepRole,
        at: DateTime<Utc>,
    ) -> Option<DateTime<Utc>> {
        self.entries
            .get(&(item, step))
            .and_then(|e| e.until)
            .map(|(u, _)| u)
            .filter(|u| *u > at)
    }

    /// The cause of `(item, step)`'s cooldown in force at `at`.
    #[must_use]
    pub fn cooling_cause(
        &self,
        item: u32,
        step: StepRole,
        at: DateTime<Utc>,
    ) -> Option<CooldownCause> {
        self.entries
            .get(&(item, step))
            .and_then(|e| e.until)
            .filter(|(u, _)| *u > at)
            .map(|(_, c)| c)
    }

    /// Drop entries with no live cooldown and no run inside the window.
    pub fn prune(&mut self, now: DateTime<Utc>) {
        let window = self.cfg.repeat_window;
        self.entries.retain(|_, e| {
            e.runs.retain(|at| add(*at, window) > now);
            !e.runs.is_empty()
                || e.until.is_some_and(|(u, _)| u > now)
                || e.refusals
                    .as_ref()
                    .is_some_and(|(_, _, at)| add(*at, window) > now)
        });
    }

    /// Every local cooldown in force at `at`, as the draw's query.
    #[must_use]
    pub fn snapshot(&self, at: DateTime<Utc>) -> StepCooldowns {
        let mut out = StepCooldowns::default();
        for (&(item, step), e) in &self.entries {
            if let Some((until, _)) = e.until.filter(|(u, _)| *u > at) {
                out.insert(item, step, until);
            }
        }
        out
    }
}

/// The draw's query: which `(item, step)` pairs are cooling down, and until
/// when. Built per tick from the local ledger, the peers' broadcasts and the
/// forge ladder; merged extend-only.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StepCooldowns {
    until: BTreeMap<(u32, StepRole), DateTime<Utc>>,
}

impl StepCooldowns {
    /// Add a window; a longer one already present is kept.
    pub fn insert(&mut self, item: u32, step: StepRole, until: DateTime<Utc>) {
        let e = self.until.entry((item, step)).or_insert(until);
        *e = (*e).max(until);
    }

    /// Whether `(item, step)` is cooling down at `at`.
    #[must_use]
    pub fn is_cooling(&self, item: u32, step: StepRole, at: DateTime<Utc>) -> bool {
        self.until(item, step).is_some_and(|u| u > at)
    }

    /// The window's end for `(item, step)`, if one is recorded.
    #[must_use]
    pub fn until(&self, item: u32, step: StepRole) -> Option<DateTime<Utc>> {
        self.until.get(&(item, step)).copied()
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.until.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.until.is_empty()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
#[path = "outcome_ledger_tests.rs"]
mod tests;
