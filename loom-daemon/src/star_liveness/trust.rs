//! Whose comments the liveness pass believes.
//!
//! Every decision this module family draws from a comment (an escalation
//! marker that suppresses an ask, an intent marker whose `requested_at` sets
//! starred-at order, a merge-refusal report that names an incident) is a
//! decision an outside commenter could otherwise forge on a public repo. A
//! comment counts only when its author is:
//!
//! - a repo insider by `author_association` (`OWNER`, `MEMBER`,
//!   `COLLABORATOR`), the people who can change labels anyway;
//! - the fleet's GitHub App, which GitHub reports as `CONTRIBUTOR` or
//!   `NONE`: the **configured** App slug ([`APP_SLUG_ENV`] >
//!   `forge.githubApp.slug`, see [`configured_app_slug`]), and as a fallback
//!   the default `loom-fleet-dispatch` name and its numbered pool members,
//!   all compared with [`crate::dep_recheck::extract::normalise_login`] so
//!   the `app/` and `[bot]` spellings match; or
//! - this daemon's own forge identity, when the forge could name it.

use std::collections::BTreeSet;
use std::path::Path;
use std::sync::{Mutex, OnceLock};

use super::forge::ForgeComment;
use crate::dep_recheck::extract::{normalise_login, DEFAULT_BOT_LOGIN};

/// Env override naming the fleet's GitHub App slug (e.g. `acme-dispatch`).
pub const APP_SLUG_ENV: &str = "LOOM_GITHUB_APP_SLUG";

fn registered() -> &'static Mutex<BTreeSet<String>> {
    static APPS: OnceLock<Mutex<BTreeSet<String>>> = OnceLock::new();
    APPS.get_or_init(|| Mutex::new(BTreeSet::new()))
}

/// The fleet App slug configured for the workspace at `root`:
/// [`APP_SLUG_ENV`] > `forge.githubApp.slug` (then `.name`). Registered for
/// [`trusted_author`] as a side effect, so every later decision in this
/// process believes it.
pub fn configured_app_slug(root: &Path) -> Option<String> {
    let from_config = || {
        let effective = crate::config_resolver::resolve_effective_config(root);
        let app = crate::config_resolver::get_path(&effective, "forge.githubApp")?;
        ["slug", "name"]
            .iter()
            .find_map(|k| app.get(*k).and_then(serde_json::Value::as_str))
            .map(str::to_string)
    };
    let slug = std::env::var(APP_SLUG_ENV)
        .ok()
        .or_else(from_config)
        .map(|s| normalise_login(s.trim()))
        .filter(|s| !s.is_empty())?;
    register_fleet_app(&slug);
    Some(slug)
}

/// Believe `slug` as the fleet App for the rest of this process.
pub fn register_fleet_app(slug: &str) {
    let norm = normalise_login(slug.trim());
    if !norm.is_empty() {
        registered()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(norm);
    }
}

/// Whether the normalised `login` is the fleet App.
fn is_fleet_app(norm: &str) -> bool {
    let env = std::env::var(APP_SLUG_ENV)
        .ok()
        .map(|s| normalise_login(s.trim()));
    env.as_deref().is_some_and(|e| !e.is_empty() && e == norm)
        || registered()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains(norm)
        || norm.starts_with(DEFAULT_BOT_LOGIN)
}

/// `author_association` values that can already write labels.
pub const TRUSTED_ASSOCIATIONS: &[&str] = &["OWNER", "MEMBER", "COLLABORATOR"];

/// Whether a comment by `login` with `association` is believed.
#[must_use]
pub fn trusted_author(
    login: Option<&str>,
    association: Option<&str>,
    self_login: Option<&str>,
) -> bool {
    if association.is_some_and(|a| {
        TRUSTED_ASSOCIATIONS
            .iter()
            .any(|t| a.eq_ignore_ascii_case(t))
    }) {
        return true;
    }
    let Some(login) = login.filter(|l| !l.trim().is_empty()) else {
        return false;
    };
    let norm = normalise_login(login);
    if is_fleet_app(&norm) {
        return true;
    }
    self_login.is_some_and(|me| normalise_login(me) == norm)
}

/// Whether `comment` is believed.
#[must_use]
pub fn trusted(comment: &ForgeComment, self_login: Option<&str>) -> bool {
    trusted_author(comment.author.as_deref(), comment.author_association.as_deref(), self_login)
}

/// The believed comments, in order.
#[must_use]
pub fn only_trusted(comments: &[ForgeComment], self_login: Option<&str>) -> Vec<ForgeComment> {
    comments
        .iter()
        .filter(|c| trusted(c, self_login))
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{register_fleet_app, trusted_author};

    #[test]
    fn insiders_the_fleet_app_and_self_are_trusted_outsiders_are_not() {
        assert!(trusted_author(Some("turian"), Some("COLLABORATOR"), None));
        assert!(trusted_author(Some("x"), Some("owner"), None));
        assert!(trusted_author(Some("loom-fleet-dispatch-1[bot]"), Some("CONTRIBUTOR"), None));
        assert!(trusted_author(Some("app/loom-fleet-dispatch"), None, None));
        assert!(trusted_author(Some("robb-bot"), Some("NONE"), Some("robb-bot")));
        assert!(!trusted_author(Some("outsider"), Some("NONE"), Some("robb-bot")));
        assert!(!trusted_author(Some("drive-by"), Some("CONTRIBUTOR"), None));
        assert!(!trusted_author(None, None, None));
    }

    #[test]
    fn a_configured_app_slug_is_trusted_beyond_the_default_prefix() {
        let login = Some("acme-dispatch-9f3[bot]");
        assert!(!trusted_author(login, Some("NONE"), None));
        register_fleet_app("app/acme-dispatch-9f3");
        assert!(trusted_author(login, Some("NONE"), None));
        assert!(!trusted_author(Some("acme-dispatch"), Some("NONE"), None), "exact, not prefix");
    }
}
