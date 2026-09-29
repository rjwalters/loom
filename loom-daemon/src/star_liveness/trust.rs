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
//!   `forge.githubApp.slug`, see [`configured_app_slug`]), every identity in
//!   the forge roster (#9537: the writer, each reader, and `legacyLogins`), and
//!   the default `loom-fleet-dispatch` name and its numbered members
//!   (`-<digits>` exactly — never a bare prefix, which trusted `-evil`).
//!   The raw login must carry an App spelling ([`is_app_login`]: `…[bot]`
//!   from REST, `app/…` from GraphQL), which only a GitHub App can have: a
//!   plain user may register `loom-fleet-dispatch-evil` or the bare slug,
//!   never `…[bot]`. Only then is the name compared, through
//!   [`crate::dep_recheck::extract::normalise_login`]; or
//! - this daemon's own forge identity, when the forge could name it (an App
//!   identity, e.g. the `<slug>[bot]` fallback, again only as an App login);
//!   or
//! - a login in the workspace's `forge.trustedCommenters` allowlist.
//!
//! The decision itself is [`crate::comment_trust::trusted_by`] (#9548), the
//! one predicate every marker reader shares; this module only supplies the
//! liveness pass's view of "the fleet App" (the process-wide registry below).

use std::collections::BTreeSet;
use std::path::Path;
use std::sync::{Mutex, OnceLock};

use super::forge::ForgeComment;
use crate::dep_recheck::extract::normalise_login;

/// Env override naming the fleet's GitHub App slug (e.g. `acme-dispatch`).
pub const APP_SLUG_ENV: &str = "LOOM_GITHUB_APP_SLUG";

fn registered() -> &'static Mutex<BTreeSet<String>> {
    static APPS: OnceLock<Mutex<BTreeSet<String>>> = OnceLock::new();
    APPS.get_or_init(|| Mutex::new(BTreeSet::new()))
}

/// The `forge.trustedCommenters` allowlist, registered by
/// [`configured_app_slug`] (raw spellings: the account kind is part of it).
fn allowlisted() -> &'static Mutex<BTreeSet<String>> {
    static ALLOW: OnceLock<Mutex<BTreeSet<String>>> = OnceLock::new();
    ALLOW.get_or_init(|| Mutex::new(BTreeSet::new()))
}

/// The fleet App slug configured for the workspace at `root`:
/// [`APP_SLUG_ENV`] > `forge.githubApp.slug` (then `.name`). Registered for
/// [`trusted_author`] as a side effect, so every later decision in this
/// process believes it.
pub fn configured_app_slug(root: &Path) -> Option<String> {
    let effective = crate::config_resolver::resolve_effective_config(root);
    allowlisted()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .extend(crate::comment_trust::allowlist_from_config(&effective));
    let from_config = || {
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
        .filter(|s| !s.is_empty());
    // #9537: believe the whole roster, not only the writer's slug — readers'
    // and renamed Apps' past comments carry their current names.
    for name in crate::forge_identity::FleetLogins::for_root(root).names() {
        if !name.contains('(') {
            register_fleet_app(&name);
        }
    }
    let slug = slug?;
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

pub use crate::comment_trust::{is_app_login, TRUSTED_ASSOCIATIONS};

/// Whether the normalised `login` names the fleet App. Callers must first
/// check [`is_app_login`] on the raw login.
fn is_fleet_app(norm: &str) -> bool {
    let env = std::env::var(APP_SLUG_ENV)
        .ok()
        .map(|s| normalise_login(s.trim()));
    env.as_deref().is_some_and(|e| !e.is_empty() && e == norm)
        || registered()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains(norm)
        || crate::forge_identity::is_default_family(norm)
}

/// Whether a comment by `login` with `association` is believed.
#[must_use]
pub fn trusted_author(
    login: Option<&str>,
    association: Option<&str>,
    self_login: Option<&str>,
) -> bool {
    let allow: Vec<String> = allowlisted()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .iter()
        .cloned()
        .collect();
    crate::comment_trust::trusted_by(
        &crate::comment_trust::Author::new(login, association),
        is_fleet_app,
        self_login,
        &allow,
    )
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
    use super::{is_app_login, register_fleet_app, trusted_author};

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

    #[test]
    fn the_app_rule_needs_an_app_spelled_login() {
        assert!(is_app_login("loom-fleet-dispatch-2[BOT]") && is_app_login("app/x"));
        assert!(!is_app_login("loom-fleet-dispatch-1"));
        // Plain users squatting the App's names (all unregistered on GitHub).
        for login in [
            "loom-fleet-dispatch-evil",
            "loom-fleet-dispatch-1",
            "loom-fleet-dispatch",
        ] {
            for association in ["NONE", "CONTRIBUTOR"] {
                assert!(
                    !trusted_author(Some(login), Some(association), None),
                    "{login}/{association}"
                );
            }
        }
        assert!(trusted_author(Some("loom-fleet-dispatch-2[bot]"), Some("NONE"), None));
        assert!(trusted_author(Some("app/loom-fleet-dispatch-2"), Some("NONE"), None));
        // #9537: the default family is the exact name or `-<digits>`, never a
        // bare prefix, even App-spelled (the old `starts_with` trusted these).
        for app in [
            "loom-fleet-dispatch-evil[bot]",
            "app/loom-fleet-dispatcher",
            "loom-fleet-dispatch-[bot]",
        ] {
            assert!(!trusted_author(Some(app), Some("NONE"), None), "{app}");
        }
        // A configured slug: only the App spelling.
        register_fleet_app("zeta-dispatch");
        assert!(trusted_author(Some("zeta-dispatch[bot]"), Some("NONE"), None));
        assert!(!trusted_author(Some("zeta-dispatch"), Some("NONE"), None));
        // Insiders still pass through association, whatever the login.
        for association in ["OWNER", "MEMBER", "COLLABORATOR"] {
            assert!(trusted_author(Some("loom-fleet-dispatch-evil"), Some(association), None));
        }
    }

    #[test]
    fn the_self_login_matches_only_the_same_account_kind() {
        // The `<slug>[bot]` fallback identity: the user `<slug>` is not it.
        let me = Some("omega-dispatch[bot]");
        assert!(trusted_author(Some("omega-dispatch[bot]"), Some("NONE"), me));
        assert!(trusted_author(Some("app/omega-dispatch"), Some("NONE"), me));
        assert!(!trusted_author(Some("omega-dispatch"), Some("NONE"), me));
        // A user-token identity: the App of the same name is not it either.
        assert!(trusted_author(Some("Robb-Bot"), Some("NONE"), Some("robb-bot")));
        assert!(!trusted_author(Some("robb-bot[bot]"), Some("NONE"), Some("robb-bot")));
    }
}
