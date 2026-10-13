//! Per-account limits an operator *declares* for an API-key account: a
//! concurrency cap (#8424 item 4), plus the plan's quota window and token
//! allowance (#11286) — configured, not probed, because no provider in scope
//! offers a plan-safe usage probe.
//!
//! # Why a sibling file and not the account's own `.env`
//!
//! An account file is exactly one `KEY=value` assignment, and the registry's
//! "file holds more than one assignment; one account is one key" refusal is
//! load-bearing (it is what makes a key file unambiguous). So the cap lives
//! beside the key, in one per-provider
//! JSON file — the same shape as `.disabled` / `.allowlist` /
//! `.bad_accounts.json`, written by the same atomic
//! [`super::registry::write_secret`] under the same `.control.lock`:
//!
//! ```json
//! { "alpha": { "maxConcurrent": 2, "exhaustionWindowSecs": 604800, "planTokenLimit": 50000000 } }
//! ```
//!
//! One file per provider rather than one per account so selection pays a
//! single read for the whole candidate set.
//!
//! # Absent means unbounded; unreadable means unknown
//!
//! A missing file, or an account with no entry, is **unbounded** — every
//! account registered before #8424 keeps behaving exactly as it did. A file
//! that exists but cannot be read or parsed is an **error**, and the selection
//! ladder turns that into [`super::registry::Ineligible::Unverifiable`]:
//! "I could not read the caps" must never read as "there are none", for the
//! same reason an unreadable `.disabled` must never read as "nothing is
//! disabled".

use std::collections::BTreeMap;
use std::path::Path;

use crate::tokens_pool::locking::MkdirLock;

use super::paths::{provider_dir, validate_account, validate_provider};
use super::registry::{restrict_dir, write_secret};

/// Per-provider file holding declared per-account limits.
pub const LIMITS_FILE: &str = ".limits.json";

/// What an operator has declared about one account.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountLimits {
    /// Most spawns that may hold this account at once, provider-side ceiling
    /// (a Z.ai coding-plan key's concurrent-request limit). `None` is
    /// unbounded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_concurrent: Option<u32>,
    /// The plan's quota window in seconds (#11286) — how long an exhausted
    /// seat stays out of selection when the provider's refusal names no reset
    /// instant of its own (see [`super::reset`]). `None` keeps the 6 h
    /// default. A weekly-capped Z.ai seat is `604800`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exhaustion_window_secs: Option<u64>,
    /// The plan's token allowance per [`Self::exhaustion_window_secs`]
    /// (#11286). Exported as the `loom.pool.plan_token_limit` gauge so the
    /// dashboard can divide `loom.llm.tokens.*` by it; nothing here enforces
    /// it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan_token_limit: Option<u64>,
}

impl AccountLimits {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.max_concurrent.is_none()
            && self.exhaustion_window_secs.is_none()
            && self.plan_token_limit.is_none()
    }
}

fn limits_path(root: &Path, provider: &str) -> std::path::PathBuf {
    provider_dir(root, provider).join(LIMITS_FILE)
}

/// Every declared limit for `provider`, keyed by account name.
///
/// # Errors
/// An absent file is an empty map. A file that exists but cannot be read or
/// parsed is an error — see the module docs.
pub fn read_limits(root: &Path, provider: &str) -> Result<BTreeMap<String, AccountLimits>, String> {
    let path = limits_path(root, provider);
    let body = match std::fs::read_to_string(&path) {
        Ok(body) => body,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(BTreeMap::new()),
        Err(e) => return Err(format!("cannot read {} ({:?})", path.display(), e.kind())),
    };
    serde_json::from_str(&body).map_err(|e| {
        format!(
            "{} is not a valid limits file ({:?} error at line {}, column {}); per-account \
             concurrency caps for provider {provider:?} are unknown. Repair the JSON, or delete \
             the file to drop every declared cap for this provider",
            path.display(),
            e.classify(),
            e.line(),
            e.column()
        )
    })
}

/// `name`'s declared concurrency cap, or `None` for unbounded.
///
/// # Errors
/// As [`read_limits`].
pub fn max_concurrent(root: &Path, provider: &str, name: &str) -> Result<Option<u32>, String> {
    Ok(read_limits(root, provider)?
        .get(name)
        .and_then(|l| l.max_concurrent))
}

/// Declare (or clear, with `None`) `name`'s concurrency cap.
///
/// `Some(0)` is refused: a zero cap means "never selectable", which is what
/// `api-keys disable` is for — expressing it as a cap would hide a disabled
/// account behind a number nothing reports as disabled.
///
/// # Errors
/// An unusable provider directory, an unreadable existing limits file (never
/// rewritten from a failed read, which would silently drop every other
/// account's cap), or a lock that cannot be taken.
pub fn set_max_concurrent(
    root: &Path,
    provider: &str,
    name: &str,
    cap: Option<u32>,
) -> Result<(), String> {
    if cap == Some(0) {
        return Err("maxConcurrent must be > 0 (use `api-keys disable` to take an account out of \
             selection entirely)"
            .to_string());
    }
    update(root, provider, name, cap.is_some(), |limits| limits.max_concurrent = cap)
}

/// Declare (or clear, with `None`) `name`'s plan window — see
/// [`AccountLimits::exhaustion_window_secs`] (#11286).
///
/// # Errors
/// `Some(0)` (a window that ends the instant it starts), plus everything
/// [`set_max_concurrent`] can fail on.
pub fn set_exhaustion_window(
    root: &Path,
    provider: &str,
    name: &str,
    secs: Option<u64>,
) -> Result<(), String> {
    if secs == Some(0) {
        return Err("exhaustionWindowSecs must be > 0".to_string());
    }
    update(root, provider, name, secs.is_some(), |limits| {
        limits.exhaustion_window_secs = secs;
    })
}

/// Declare (or clear, with `None`) `name`'s plan token allowance — see
/// [`AccountLimits::plan_token_limit`] (#11286).
///
/// # Errors
/// `Some(0)`, plus everything [`set_max_concurrent`] can fail on.
pub fn set_plan_token_limit(
    root: &Path,
    provider: &str,
    name: &str,
    tokens: Option<u64>,
) -> Result<(), String> {
    if tokens == Some(0) {
        return Err("planTokenLimit must be > 0".to_string());
    }
    update(root, provider, name, tokens.is_some(), |limits| {
        limits.plan_token_limit = tokens;
    })
}

/// Read-modify-write one account's entry under the provider's control lock.
/// `declaring` is whether the edit sets something (as opposed to clearing):
/// clearing on a provider that has no directory yet is a no-op, not a
/// `mkdir`.
fn update(
    root: &Path,
    provider: &str,
    name: &str,
    declaring: bool,
    edit: impl FnOnce(&mut AccountLimits),
) -> Result<(), String> {
    validate_provider(provider)?;
    validate_account(name)?;
    let dir = provider_dir(root, provider);
    if !dir.exists() {
        if !declaring {
            return Ok(());
        }
        std::fs::create_dir_all(&dir)
            .map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
        restrict_dir(&dir);
    }
    let _lock = MkdirLock::acquire(&dir.join(".control.lock"))
        .map_err(|e| format!("cannot lock {}: {e}", limits_path(root, provider).display()))?;
    let mut limits = read_limits(root, provider)?;
    edit(limits.entry(name.to_string()).or_default());
    limits.retain(|_, l| !l.is_empty());
    write_limits(root, provider, &limits)
}

/// Drop `name`'s limits entirely — called by `registry::remove` so a name
/// re-registered later does not inherit a stale cap.
pub(super) fn forget(root: &Path, provider: &str, name: &str) -> Result<(), String> {
    if !limits_path(root, provider).exists() {
        return Ok(());
    }
    let dir = provider_dir(root, provider);
    let _lock = MkdirLock::acquire(&dir.join(".control.lock"))
        .map_err(|e| format!("cannot lock {}: {e}", limits_path(root, provider).display()))?;
    let mut limits = read_limits(root, provider)?;
    if limits.remove(name).is_none() {
        return Ok(());
    }
    write_limits(root, provider, &limits)
}

fn write_limits(
    root: &Path,
    provider: &str,
    limits: &BTreeMap<String, AccountLimits>,
) -> Result<(), String> {
    let path = limits_path(root, provider);
    if limits.is_empty() {
        if path.exists() {
            std::fs::remove_file(&path)
                .map_err(|e| format!("cannot update {}: {e}", path.display()))?;
        }
        return Ok(());
    }
    let body = serde_json::to_string_pretty(limits).map_err(|e| e.to_string())?;
    write_secret(&path, &format!("{body}\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pool(names: &[&str]) -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        for name in names {
            super::super::registry::add(tmp.path(), "zai", name, "ZAI_API_KEY", "fake-key", false)
                .unwrap();
        }
        tmp
    }

    #[test]
    fn an_account_with_no_declared_cap_is_unbounded() {
        let tmp = pool(&["alpha"]);
        assert_eq!(max_concurrent(tmp.path(), "zai", "alpha").unwrap(), None);
        assert_eq!(read_limits(tmp.path(), "zai").unwrap(), BTreeMap::new());
    }

    #[test]
    fn a_declared_cap_round_trips_and_clears() {
        let tmp = pool(&["alpha", "beta"]);
        set_max_concurrent(tmp.path(), "zai", "alpha", Some(3)).unwrap();
        assert_eq!(max_concurrent(tmp.path(), "zai", "alpha").unwrap(), Some(3));
        assert_eq!(max_concurrent(tmp.path(), "zai", "beta").unwrap(), None);
        let body = std::fs::read_to_string(limits_path(tmp.path(), "zai")).unwrap();
        assert!(body.contains("\"maxConcurrent\": 3"), "{body}");

        set_max_concurrent(tmp.path(), "zai", "alpha", None).unwrap();
        assert_eq!(max_concurrent(tmp.path(), "zai", "alpha").unwrap(), None);
        // The file is removed once nothing is declared, rather than left as
        // an empty object.
        assert!(!limits_path(tmp.path(), "zai").exists());
    }

    #[test]
    fn a_zero_cap_is_refused_in_favour_of_disable() {
        let tmp = pool(&["alpha"]);
        let err = set_max_concurrent(tmp.path(), "zai", "alpha", Some(0)).unwrap_err();
        assert!(err.contains("disable"), "{err}");
    }

    #[test]
    fn an_unparsable_limits_file_is_an_error_and_is_never_clobbered() {
        let tmp = pool(&["alpha", "beta"]);
        set_max_concurrent(tmp.path(), "zai", "alpha", Some(2)).unwrap();
        let path = limits_path(tmp.path(), "zai");
        for torn in ["", "{\"alpha\":", "[]"] {
            std::fs::write(&path, torn).unwrap();
            let err = read_limits(tmp.path(), "zai").unwrap_err();
            assert!(err.contains(&path.display().to_string()), "{err}");
            assert!(max_concurrent(tmp.path(), "zai", "alpha").is_err());
            assert!(set_max_concurrent(tmp.path(), "zai", "beta", Some(1)).is_err());
            assert_eq!(std::fs::read_to_string(&path).unwrap(), torn, "file was clobbered");
        }
    }

    /// #11286: the plan window and token allowance round-trip beside the cap
    /// and clear independently of it.
    #[test]
    fn the_plan_window_and_token_limit_round_trip_independently() {
        let tmp = pool(&["alpha"]);
        set_max_concurrent(tmp.path(), "zai", "alpha", Some(2)).unwrap();
        set_exhaustion_window(tmp.path(), "zai", "alpha", Some(604_800)).unwrap();
        set_plan_token_limit(tmp.path(), "zai", "alpha", Some(50_000_000)).unwrap();
        let got = read_limits(tmp.path(), "zai").unwrap()["alpha"].clone();
        assert_eq!(
            got,
            AccountLimits {
                max_concurrent: Some(2),
                exhaustion_window_secs: Some(604_800),
                plan_token_limit: Some(50_000_000),
            }
        );
        let body = std::fs::read_to_string(limits_path(tmp.path(), "zai")).unwrap();
        assert!(body.contains("\"exhaustionWindowSecs\": 604800"), "{body}");
        assert!(body.contains("\"planTokenLimit\": 50000000"), "{body}");

        set_max_concurrent(tmp.path(), "zai", "alpha", None).unwrap();
        assert_eq!(max_concurrent(tmp.path(), "zai", "alpha").unwrap(), None);
        assert_eq!(
            super::super::reset::configured_window(tmp.path(), "zai", "alpha"),
            Some(604_800)
        );
        set_exhaustion_window(tmp.path(), "zai", "alpha", None).unwrap();
        set_plan_token_limit(tmp.path(), "zai", "alpha", None).unwrap();
        assert!(!limits_path(tmp.path(), "zai").exists());
        assert!(set_exhaustion_window(tmp.path(), "zai", "alpha", Some(0)).is_err());
        assert!(set_plan_token_limit(tmp.path(), "zai", "alpha", Some(0)).is_err());
    }

    #[test]
    fn removing_an_account_forgets_its_cap() {
        let tmp = pool(&["alpha", "beta"]);
        set_max_concurrent(tmp.path(), "zai", "alpha", Some(2)).unwrap();
        set_max_concurrent(tmp.path(), "zai", "beta", Some(5)).unwrap();
        super::super::registry::remove(tmp.path(), "zai", "alpha").unwrap();
        assert_eq!(max_concurrent(tmp.path(), "zai", "alpha").unwrap(), None);
        assert_eq!(max_concurrent(tmp.path(), "zai", "beta").unwrap(), Some(5));
    }
}
