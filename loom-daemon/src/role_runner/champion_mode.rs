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
//!   `autonomous.roleRunner.championMode` (#11859) is retired (#11911):
//!   there is no other mode, `"non-item"` is accepted silently, and any other
//!   value is ignored with one warning.
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

use super::RoleSpec;

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

/// Warn about a configured `championMode` (#11859), retired by #11911:
/// Champion runs its non-item queues only, so the key changes nothing.
/// `"non-item"` (any case, surrounding whitespace ignored) is that behaviour
/// already and stays quiet; any other value, of any type, is ignored with one
/// warning per distinct value ([`crate::retired_config::warn_once`]). Never
/// an error: a host whose config still sets it keeps starting.
pub fn warn_champion_mode(block: &serde_json::Value) {
    let Some(raw) = block.get(CHAMPION_MODE_KEY) else {
        return;
    };
    if raw
        .as_str()
        .is_some_and(|v| v.trim().eq_ignore_ascii_case("non-item"))
    {
        return;
    }
    crate::retired_config::warn_once(
        &format!("autonomous.roleRunner.{CHAMPION_MODE_KEY}={raw}"),
        &format!(
            "role_runner: autonomous.roleRunner.{CHAMPION_MODE_KEY} = {raw} is retired and \
             ignored: Champion runs its non-item queues only (#11911)"
        ),
    );
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
