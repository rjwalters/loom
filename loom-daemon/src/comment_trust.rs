//! Whose comments count as control signals (#9548).
//!
//! Loom manages repos that accept outside contributions, so everything an
//! outsider can write (comments, reviews, bodies) is untrusted input. A marker
//! or phrase that changes Loom's behaviour (a `loom:verdict-sha` marker, a
//! `Champion Review: APPROVED` verdict) may only count when the forge says it
//! was authored by a trusted identity. Anything else is **content, not
//! control**: a well-formed marker from an untrusted author is prose, and
//! reads exactly as if it were absent.
//!
//! A comment counts only when its author is one of:
//!
//! 1. a repo insider by `author_association` (`OWNER`, `MEMBER`,
//!    `COLLABORATOR`), the people who can change labels anyway. Never
//!    `CONTRIBUTOR`, `FIRST_TIME_CONTRIBUTOR`, `FIRST_TIMER` or `NONE`: a
//!    merged fork PR makes anyone a contributor;
//! 2. one of **this** fleet's GitHub Apps
//!    ([`crate::forge_identity::FleetLogins`], matched exactly), and only when
//!    the forge spells the author as an App (`x[bot]` from REST, `app/x` from
//!    GraphQL, or a `Bot` type). A user account may register the bare slug;
//!    no user can be `x[bot]`. Fleet Apps appear as `NONE`/`CONTRIBUTOR`, so
//!    this rule is what keeps the fleet's own markers working. **Another Loom
//!    installation's Apps are not ours**: a foreign fleet emits perfectly
//!    well-formed markers, and they count for nothing here;
//! 3. this daemon's own identity, compared with the same account kind (the
//!    user `x` is never the App `x[bot]`); or
//! 4. an explicit allowlist, `forge.trustedCommenters` (logins, same
//!    account-kind rule: list `x[bot]` to allow an App); or
//! 5. a fleet admin from the fleet-store roster `fleet/admins.json`
//!    ([`crate::fleet_store::admins`], user accounts only). It is **unioned**
//!    with rule 4: neither can remove the other. Fails closed: an unreadable
//!    roster widens nothing and is reported by
//!    [`TrustPolicy::sources_consulted`] (#10303).
//!
//! # The GraphQL shape cannot name an App
//!
//! `gh … --json comments` reports an App author as the bare slug
//! (`{"login":"loom-fleet-dispatch"}`), indistinguishable from a user of that
//! name. A bare login is therefore treated as a user: in that shape the
//! fleet's own comments pass only through rules 1, 3 and 4. A reader that
//! must believe fleet-authored markers fetches the REST listing
//! (`gh api repos/{owner}/{repo}/issues/<n>/comments --paginate`), whose
//! `user.login` carries the `[bot]` suffix.
//!
//! `star_liveness::trust` delegates its decision to [`trusted_by`], and the
//! shell reaches the same predicate through `loom-daemon forge
//! trusted-comments`. [`promotion_gate`] applies it to an issue's *body
//! author* before any automatic `loom:issue` promotion (#10827).

use std::path::Path;

use serde_json::Value;

use crate::dep_recheck::extract::normalise_login;
use crate::fleet_store::ADMINS_PATH;
use crate::forge_identity::FleetLogins;

/// `author_association` values that can already write labels.
pub const TRUSTED_ASSOCIATIONS: &[&str] = &["OWNER", "MEMBER", "COLLABORATOR"];

/// The config key holding the explicit allowlist of trusted logins.
pub const TRUSTED_COMMENTERS_KEY: &str = "forge.trustedCommenters";

/// Whether the raw `login` is spelled as a GitHub App (`…[bot]` or `app/…`),
/// a spelling no user account can register.
#[must_use]
pub fn is_app_login(login: &str) -> bool {
    let l = login.trim().to_ascii_lowercase();
    l.ends_with("[bot]") || l.starts_with("app/")
}

/// Who wrote a comment or review, as the forge reported it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Author {
    /// The raw login, in whatever spelling the forge used.
    pub login: Option<String>,
    /// `author_association` / `authorAssociation`.
    pub association: Option<String>,
    /// The forge identified the author as a GitHub App.
    pub app: bool,
}

impl Author {
    /// An author from a raw login (App-ness read from its spelling).
    #[must_use]
    pub fn new(login: Option<&str>, association: Option<&str>) -> Self {
        let login = login.map(str::trim).filter(|l| !l.is_empty());
        Self {
            app: login.is_some_and(is_app_login),
            login: login.map(str::to_string),
            association: association.map(str::to_string),
        }
    }

    /// The author of one comment/review object in either shape: REST
    /// (`user.login`, `user.type`, `author_association`) or GraphQL / `gh
    /// --json` (`author.login`, `authorAssociation`, optional
    /// `author.__typename` / `author.is_bot`). A missing author (a deleted
    /// account) yields an author no rule trusts.
    #[must_use]
    pub fn from_json(v: &Value) -> Self {
        let who = v
            .get("user")
            .filter(|u| u.is_object())
            .or_else(|| v.get("author").filter(|a| a.is_object()));
        let login = who.and_then(|w| w.get("login")).and_then(Value::as_str);
        let association = ["author_association", "authorAssociation"]
            .iter()
            .find_map(|k| v.get(*k).and_then(Value::as_str));
        let typed_bot = who.is_some_and(|w| {
            ["type", "__typename"]
                .iter()
                .any(|k| w.get(*k).and_then(Value::as_str) == Some("Bot"))
                || w.get("is_bot").and_then(Value::as_bool) == Some(true)
        });
        let mut author = Self::new(login, association);
        author.app = author.login.is_some() && (author.app || typed_bot);
        author
    }
}

/// `entry` (a configured login: self or allowlist) names `author`: the same
/// normalised name AND the same account kind.
fn names(entry: &str, author_app: bool, author_norm: &str) -> bool {
    let entry = entry.trim();
    !entry.is_empty() && is_app_login(entry) == author_app && normalise_login(entry) == author_norm
}

/// The trust decision (see the module docs). `is_fleet_app` answers for a
/// normalised login already known to be App-spelled.
#[must_use]
pub fn trusted_by(
    author: &Author,
    is_fleet_app: impl Fn(&str) -> bool,
    self_login: Option<&str>,
    allowlist: &[String],
) -> bool {
    if author.association.as_deref().is_some_and(|a| {
        TRUSTED_ASSOCIATIONS
            .iter()
            .any(|t| a.trim().eq_ignore_ascii_case(t))
    }) {
        return true;
    }
    let Some(login) = author.login.as_deref() else {
        return false;
    };
    let norm = normalise_login(login);
    if norm.is_empty() {
        return false;
    }
    if author.app && is_fleet_app(&norm) {
        return true;
    }
    self_login
        .into_iter()
        .chain(allowlist.iter().map(String::as_str))
        .any(|entry| names(entry, author.app, &norm))
}

/// The configured allowlist (`forge.trustedCommenters`), trimmed, empties
/// dropped. A non-array value is ignored (logged): trust is never widened by
/// a malformed config.
#[must_use]
pub fn allowlist_from_config(effective: &Value) -> Vec<String> {
    match crate::config_resolver::get_path(effective, TRUSTED_COMMENTERS_KEY) {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Array(a)) => a
            .iter()
            .filter_map(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect(),
        Some(other) => {
            log::warn!(
                "comment_trust: {TRUSTED_COMMENTERS_KEY} must be an array of logins, got {other}; \
                 ignoring it (#9548)"
            );
            Vec::new()
        }
    }
}

/// The resolved trust rules for one workspace.
#[derive(Debug, Clone, Default)]
pub struct TrustPolicy {
    fleet: FleetLogins,
    self_login: Option<String>,
    allowlist: Vec<String>,
    /// Fleet admin roster logins (rule 5), empty when unavailable.
    admins: Vec<String>,
    /// How the roster resolved (`None`: not consulted, e.g. explicit rules).
    admins_state: Option<String>,
}

impl TrustPolicy {
    /// Explicit rules (tests, and callers that resolved them already).
    #[must_use]
    pub fn new(fleet: FleetLogins, self_login: Option<String>, allowlist: Vec<String>) -> Self {
        Self {
            fleet,
            self_login,
            allowlist,
            admins: Vec::new(),
            admins_state: None,
        }
    }

    /// Add a resolved fleet admin roster (rule 5).
    #[must_use]
    pub fn with_admins(mut self, admins: crate::fleet_store::admins::Admins) -> Self {
        self.admins = admins.logins;
        self.admins_state = Some(admins.state);
        self
    }

    /// The trust sources this policy consults, for the ignored-marker notice
    /// (#10291): says which rules applied and whether the roster loaded.
    #[must_use]
    pub fn sources_consulted(&self) -> String {
        format!(
            "author_association (OWNER/MEMBER/COLLABORATOR), fleet Apps, self identity, \
             forge.trustedCommenters ({} entries), fleet admin roster {ADMINS_PATH} ({})",
            self.allowlist.len(),
            self.admins_state.as_deref().unwrap_or("not consulted"),
        )
    }

    /// The rules for the workspace at `root`: its fleet roster, its writer
    /// App as the self identity, and its configured allowlist.
    #[must_use]
    pub fn for_root(root: &Path) -> Self {
        let roster = crate::forge_identity::cached(root);
        let self_login = roster
            .writer
            .as_ref()
            .and_then(|w| w.slug.as_deref())
            .map(|s| format!("{s}[bot]"));
        let effective = crate::config_resolver::resolve_effective_config(root);
        Self {
            fleet: FleetLogins::of(&roster),
            self_login,
            allowlist: allowlist_from_config(&effective),
            admins: Vec::new(),
            admins_state: None,
        }
        .with_admins(crate::fleet_store::admins::resolve(root))
    }

    /// Replace the self identity (an explicit `--self-login`).
    #[must_use]
    pub fn with_self_login(mut self, login: Option<String>) -> Self {
        if login.is_some() {
            self.self_login = login;
        }
        self
    }

    /// Whether the fleet admin roster (rule 5) loaded: `None` when it was not
    /// consulted (explicit rules), `Some(false)` when it was unreadable.
    #[must_use]
    pub fn admins_loaded(&self) -> Option<bool> {
        self.admins_state
            .as_deref()
            .map(|s| !s.starts_with("unavailable"))
    }

    /// Whether `author` is believed.
    #[must_use]
    pub fn trusts(&self, author: &Author) -> bool {
        let allow: Vec<String> = self.allowlist.iter().chain(&self.admins).cloned().collect();
        trusted_by(author, |norm| self.fleet.contains(norm), self.self_login.as_deref(), &allow)
    }

    /// Whether the comment/review object `v` (either shape) is believed.
    #[must_use]
    pub fn trusts_json(&self, v: &Value) -> bool {
        self.trusts(&Author::from_json(v))
    }

    /// The believed objects, in order.
    #[must_use]
    pub fn filter(&self, items: Vec<Value>) -> Vec<Value> {
        items.into_iter().filter(|v| self.trusts_json(v)).collect()
    }

    /// The bodies of the believed comments in a REST comment listing
    /// (`gh api …/comments --paginate` stdout), oldest first. `None` when the
    /// listing does not parse; a caller treats that as "could not read", never
    /// as "no marker".
    #[must_use]
    pub fn trusted_bodies(&self, listing: &[u8]) -> Option<Vec<String>> {
        let items = parse_listing(listing)?;
        Some(
            self.filter(items)
                .into_iter()
                .filter_map(|v| v.get("body").and_then(Value::as_str).map(str::to_string))
                .collect(),
        )
    }
}

/// Filter a whole document the way `forge trusted-comments` does: an array
/// (or concatenated arrays) of comments → the trusted subset as one array; an
/// object (`gh … --json comments,reviews,…`) → the same object with its
/// `comments` and `reviews` arrays filtered in place. `None` when the input is
/// neither.
#[must_use]
pub fn filter_document(policy: &TrustPolicy, bytes: &[u8]) -> Option<Value> {
    if let Ok(Value::Object(mut obj)) = serde_json::from_slice::<Value>(bytes) {
        // An object is a comment document only when it carries at least one
        // of `comments` / `reviews` and EVERY one it carries is a plain array.
        // Anything else (a forge error object, a GraphQL `{nodes: [...]}`
        // connection, a document nested under `data`) is rejected rather than
        // echoed back, so no caller can mistake unfiltered text for filtered.
        let keys: Vec<&str> = ["comments", "reviews"]
            .into_iter()
            .filter(|k| obj.contains_key(*k))
            .collect();
        if keys.is_empty() || keys.iter().any(|k| !obj[*k].is_array()) {
            return None;
        }
        for key in keys {
            if let Some(Value::Array(items)) = obj.get_mut(key) {
                *items = policy.filter(std::mem::take(items));
            }
        }
        return Some(Value::Object(obj));
    }
    parse_listing(bytes).map(|items| Value::Array(policy.filter(items)))
}

/// Parse a comment listing: one JSON array, or several concatenated (a
/// paginated `gh api` without `--slurp`, or `--slurp`'s array of pages),
/// flattened oldest first. `None` on anything else, including empty or
/// whitespace-only input: a listing with no comments is `[]`, so nothing at
/// all means the fetch did not happen (Judge #9566).
#[must_use]
pub fn parse_listing(bytes: &[u8]) -> Option<Vec<Value>> {
    if bytes.iter().all(u8::is_ascii_whitespace) {
        return None;
    }
    let mut out = Vec::new();
    for value in serde_json::Deserializer::from_slice(bytes).into_iter::<Value>() {
        match value.ok()? {
            Value::Array(items) => {
                for item in items {
                    match item {
                        Value::Array(page) => out.extend(page),
                        other => out.push(other),
                    }
                }
            }
            _ => return None,
        }
    }
    Some(out)
}

pub mod promotion_gate;
pub mod records;

#[cfg(test)]
mod structure_tests;
#[cfg(test)]
mod tests;
