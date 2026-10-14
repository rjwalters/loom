//! `autonomous.roleRunner.championMode` (#11859): which of Champion's queues
//! an interval (or idle-edge) Champion tick works.
//!
//! The work-finder redesign moves Champion's **item** work (a merge decision
//! on one `loom:pr` PR, a promotion verdict on one `loom:curated` issue) onto
//! per-item steps (`/loom:champion --pr N`, `/loom:champion --issue N`).
//! Champion's other queues (architect/hermit/auditor proposals, epics,
//! Doctor-cap parked PRs, rollout checks, and the batch-wide Pass 0 / held-PR
//! census) have no item to draw, so they stay on the role runner's interval.
//! A root that hands its item work to steps therefore needs its interval
//! Champion to run **only** those queues, or the two would double-act on the
//! same PR or issue. That is this knob:
//!
//! | value | dispatched prompt | queues |
//! |---|---|---|
//! | `"all"` (default, or absent) | `/loom:champion` | every queue, byte-for-byte today's batch pass |
//! | `"non-item"` | `/loom:champion --non-item` | the non-item queues only (`champion-targeted.md`) |
//!
//! # Why a per-role option rather than a `roles` spelling
//!
//! `autonomous.roleRunner.roles` entries are matched by exact name against
//! [`super::DEFAULT_ROLES`] in half a dozen places (the resolved list, the
//! stale-allowlist diagnostic, `onIdle`, `onIdleMaxWait`,
//! `roleMaxConcurrent`, the lane rule). A `champion:non-item` spelling would
//! have to be normalised at every one of them, and any it missed would
//! silently treat Champion as unconfigured. A sibling key leaves the role's
//! name alone everywhere and changes only the argument the launch carries,
//! exactly as `architectMaxProposals` does for Architect.
//!
//! # Scope and fail direction
//!
//! Per root, read live on every tick from that root's own config (all tiers),
//! like every other role-runner knob. There is deliberately **no environment
//! override**: an env var is host-wide, and this knob exists to differ per
//! root during a staged rollout.
//!
//! An unrecognised value warns and falls back to `"all"`: today's behaviour,
//! and the soft-fail every other `autonomous.roleRunner` key uses. It never
//! fails toward skipping work.

/// The one role this knob applies to.
pub const CHAMPION_ROLE: &str = "champion";

/// The config key, under `autonomous.roleRunner`.
pub const CHAMPION_MODE_KEY: &str = "championMode";

/// The argument `/loom:champion` reads as "non-item queues only"
/// (`champion.md` → "Argument Handling").
pub const NON_ITEM_ARG: &str = "--non-item";

/// Which of Champion's queues a role-runner Champion tick works.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ChampionMode {
    /// Every queue: the batch pass, unchanged.
    #[default]
    All,
    /// The non-item queues only; item work is left to targeted steps.
    NonItem,
}

impl ChampionMode {
    /// The config spelling.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::All => "all",
            Self::NonItem => "non-item",
        }
    }

    /// Parse a config value. Case and surrounding whitespace are ignored;
    /// nothing else is accepted.
    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "all" => Some(Self::All),
            "non-item" => Some(Self::NonItem),
            _ => None,
        }
    }
}

/// Read `championMode` from an `autonomous.roleRunner` block. `None` when the
/// key is absent; an unrecognised or non-string value warns and is `None`
/// too, so it resolves to [`ChampionMode::All`].
#[must_use]
pub fn parse_champion_mode(block: &serde_json::Value) -> Option<ChampionMode> {
    let raw = block.get(CHAMPION_MODE_KEY)?;
    let parsed = raw.as_str().and_then(ChampionMode::parse);
    if parsed.is_none() {
        log::warn!(
            "role_runner: autonomous.roleRunner.{CHAMPION_MODE_KEY} = {raw} is not one of \
             \"all\" / \"non-item\" — ignored, Champion runs every queue (#11859)"
        );
    }
    parsed
}

/// The argument to append to `role`'s slash command, if any. Only Champion
/// in [`ChampionMode::NonItem`] carries one.
#[must_use]
pub fn prompt_arg(role: &str, mode: Option<ChampionMode>) -> Option<&'static str> {
    (role == CHAMPION_ROLE && mode == Some(ChampionMode::NonItem)).then_some(NON_ITEM_ARG)
}
