//! The discovery chain: which tiers a daemon walks each tick, set by config
//! (#12311, operator rulings 2026-10-15).
//!
//! | Install | Chain |
//! |---|---|
//! | Standalone (no `forge.mirror` block) | `[gh]`, today's behaviour |
//! | Fleet, `forge.mirror.discovery = "worklist"` | `[mirror, gateway, gh]` |
//! | Fleet, `= "direct"` (break-glass: a mirror that is wrong, not down) | `[gateway, gh]` |
//! | Fleet, `= "per_call"` or unset | the same as `"direct"`: the per-call mirror leg is gone (#12307), so this value is only accepted |
//!
//! There is never a writer tier. A tick walks its chain from the first tier
//! and records the tier that answered; every tier failing is [`Tier::None`]
//! (no draw that tick).

use serde_json::Value;

/// `forge.mirror.discovery`.
pub const MODE_KEY: &str = "forge.mirror.discovery";

/// The key #12306 shipped, kept as an alias: `"mirror"` there is `"worklist"`
/// here, when [`MODE_KEY`] is unset.
const LEGACY_KEY: &str = "autonomous.workFinder.discovery";

/// Which tier answered a repo's discovery.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Tier {
    /// The fleet mirror (the worklist, or a per-call listing it vouched for).
    Mirror,
    /// The direct listing through the read gateway.
    Gateway,
    /// The direct listing with the installation's own credential.
    Gh,
    /// Every configured tier failed: no draw for the repo this tick.
    None,
}

impl Tier {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Mirror => "mirror",
            Self::Gateway => "gateway",
            Self::Gh => "gh",
            Self::None => "none",
        }
    }
}

/// `forge.mirror.discovery`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Mode {
    /// One worklist read per tick.
    Worklist,
    /// Per-repo listings only; the mirror is not asked. The default, and the
    /// break-glass for a wrong mirror. `"per_call"`, the retired mode that
    /// asked the mirror inside each listing, reads as this (#12307).
    #[default]
    Direct,
}

/// Which backend builds a tick's snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BackendKind {
    /// Per-repo listings with an ETag cache.
    #[default]
    Direct,
    /// One worklist read per tick.
    Mirror,
}

impl BackendKind {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Direct => "direct",
            Self::Mirror => "mirror",
        }
    }
}

/// The resolved chain for one tick.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Chain {
    /// `None`: standalone, no `forge.mirror` block.
    mode: Option<Mode>,
    /// The tier a direct listing is recorded under: [`Tier::Gateway`] when
    /// `forge.gateway` is configured, else [`Tier::Gh`].
    listing: Tier,
}

impl Default for Chain {
    fn default() -> Self {
        Self::standalone()
    }
}

impl Chain {
    /// No mirror: `[gh]`.
    #[must_use]
    pub fn standalone() -> Self {
        Self {
            mode: None,
            listing: Tier::Gh,
        }
    }

    /// A fleet chain in `mode`, whose direct listings are `listing`.
    #[must_use]
    pub fn fleet(mode: Mode, listing: Tier) -> Self {
        Self {
            mode: Some(mode),
            listing,
        }
    }

    /// The chain an effective config selects. Pure.
    #[must_use]
    pub fn from_effective(effective: &Value) -> Self {
        let listing = if crate::forge_gateway::parse(effective).0.is_some() {
            Tier::Gateway
        } else {
            Tier::Gh
        };
        if crate::forge_mirror::parse(effective).0.is_none() {
            return Self {
                mode: None,
                listing,
            };
        }
        let text = |key: &str| {
            crate::config_resolver::get_path(effective, key)
                .and_then(Value::as_str)
                .map(|s| s.trim().to_ascii_lowercase())
        };
        let mode = match text(MODE_KEY).as_deref() {
            Some("worklist") => Mode::Worklist,
            Some("direct") => Mode::Direct,
            Some(_) => Mode::Direct,
            None if text(LEGACY_KEY).as_deref() == Some("mirror") => Mode::Worklist,
            None => Mode::Direct,
        };
        Self {
            mode: Some(mode),
            listing,
        }
    }

    /// The chain for the daemon rooted at `root`, read live.
    #[must_use]
    pub fn for_root(root: &std::path::Path) -> Self {
        Self::from_effective(&crate::config_resolver::resolve_effective_config(root))
    }

    /// The tiers walked, first to last.
    #[must_use]
    pub fn tiers(&self) -> Vec<Tier> {
        match self.mode {
            None => vec![Tier::Gh],
            Some(Mode::Worklist) => vec![Tier::Mirror, Tier::Gateway, Tier::Gh],
            Some(Mode::Direct) => vec![Tier::Gateway, Tier::Gh],
        }
    }

    #[must_use]
    pub fn mode(&self) -> Option<Mode> {
        self.mode
    }

    /// The tier a direct listing is recorded under.
    #[must_use]
    pub fn listing_tier(&self) -> Tier {
        self.listing
    }

    #[must_use]
    pub fn backend(&self) -> BackendKind {
        match self.mode {
            Some(Mode::Worklist) => BackendKind::Mirror,
            _ => BackendKind::Direct,
        }
    }

    /// Whether a failed or partial worklist falls back to a paced direct
    /// listing (the fleet's outage modes are capped, see `fallback`).
    #[must_use]
    pub fn is_worklist(&self) -> bool {
        self.mode == Some(Mode::Worklist)
    }
}
