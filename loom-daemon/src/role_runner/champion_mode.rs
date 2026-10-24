//! The role runner under step dispatch (#11909): Champion runs its non-item
//! queues only, and the item roles do not run on intervals.
//!
//! The work finder draws every item at every stage and spawns the step its
//! state calls for (`step_dispatch`, design #11894): a Curator, a Champion
//! promotion or merge, a Judge, a Doctor, a Builder. The role runner must
//! not act on the same items, or the two would double-act. So:
//!
//! - **Champion** is dispatched as `/loom:champion --non-item`
//!   (`champion-targeted.md`): proposals, epics, Doctor-cap parked PRs,
//!   rollout checks and the batch-wide census, which have no item to draw.
//!   `autonomous.roleRunner.championMode` (#11859) is retired: `"non-item"`
//!   is the only behaviour, and `"all"` (or any other value) is ignored with
//!   a warning.
//! - **Curator, Judge and Doctor** ([`STEP_ROLES`]) never tick on an
//!   interval or on the idle edge: the daemon spawns no interval loop for
//!   them (`daemon_service`), so naming one in `autonomous.roleRunner.roles`
//!   has no effect, and naming one in `onIdle` is ignored with a warning,
//!   once per name.
//!
//! Guide, Auditor, Hermit and Architect keep the interval: they have no item
//! to draw.

use std::collections::BTreeSet;
use std::sync::{Mutex, OnceLock};

use super::{resolve_roles, RoleRunnerConfig, RoleSpec};

/// The one role `championMode` applied to.
pub const CHAMPION_ROLE: &str = "champion";

/// The retired config key, under `autonomous.roleRunner`.
pub const CHAMPION_MODE_KEY: &str = "championMode";

/// The argument `/loom:champion` reads as "non-item queues only"
/// (`champion.md` → "Argument Handling").
pub const NON_ITEM_ARG: &str = "--non-item";

/// The roles whose item work the step engine runs, and which therefore never
/// tick on the role runner.
pub const STEP_ROLES: [&str; 3] = ["curator", "judge", "doctor"];

/// The value `championMode` was configured to, kept only so its retirement
/// can be warned about.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ChampionMode {
    /// `"all"`: retired, ignored.
    #[default]
    All,
    /// `"non-item"`: the only behaviour.
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

/// Read `championMode` from an `autonomous.roleRunner` block. Any value but
/// `"non-item"` is ignored with a warning: Champion runs non-item only.
#[must_use]
pub fn parse_champion_mode(block: &serde_json::Value) -> Option<ChampionMode> {
    let raw = block.get(CHAMPION_MODE_KEY)?;
    let parsed = raw.as_str().and_then(ChampionMode::parse);
    if parsed != Some(ChampionMode::NonItem) {
        log::warn!(
            "role_runner: autonomous.roleRunner.{CHAMPION_MODE_KEY} = {raw} is retired and \
             ignored: Champion runs its non-item queues only (#11909)"
        );
    }
    parsed
}

/// The argument to append to `role`'s slash command, if any: Champion's
/// `--non-item`.
#[must_use]
pub fn prompt_arg(role: &str) -> Option<&'static str> {
    (role == CHAMPION_ROLE).then_some(NON_ITEM_ARG)
}

/// Whether `role` is one of [`STEP_ROLES`].
#[must_use]
pub fn is_step_role(role: &str) -> bool {
    STEP_ROLES.contains(&role)
}

fn warned() -> &'static Mutex<BTreeSet<(String, String)>> {
    static WARNED: OnceLock<Mutex<BTreeSet<(String, String)>>> = OnceLock::new();
    WARNED.get_or_init(|| Mutex::new(BTreeSet::new()))
}

/// The roles `config` runs as interval loops: [`resolve_roles`] without the
/// [`STEP_ROLES`], which run as work-finder steps. What status and health
/// report as ticking (#12049).
#[must_use]
pub fn resolve_interval_roles(config: &RoleRunnerConfig) -> Vec<RoleSpec> {
    without_step_roles(resolve_roles(config), None)
}

/// `specs` without the [`STEP_ROLES`]. `key` names the config list they came
/// from (`roles`, `onIdle`), or is `None` for the built-in default, which
/// needs no warning; a configured step role is warned about once per name.
#[must_use]
pub fn without_step_roles(specs: Vec<RoleSpec>, key: Option<&str>) -> Vec<RoleSpec> {
    specs
        .into_iter()
        .filter(|s| {
            if !is_step_role(s.name) {
                return true;
            }
            if let Some(key) = key {
                let first = warned()
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .insert((key.to_string(), s.name.to_string()));
                if first {
                    log::warn!(
                        "role_runner: autonomous.roleRunner.{key} names {:?}, which the work \
                         finder now runs as steps; ignored (#11909)",
                        s.name
                    );
                }
            }
            false
        })
        .collect()
}
