//! Re-mint and retry ONCE when a forge write is refused with
//! `Resource not accessible by integration` (#9247).
//!
//! An installation token's repository set is fixed when it is minted. A token
//! minted before a repository joined the App installation can still READ a
//! public repository (any token can), so the workspace looks healthy, but
//! every WRITE to it is a `403` with that text. #6171 handles the private-repo
//! shape of the same staleness (every call `404`s) on two read paths; this is
//! the write-side sibling.
//!
//! The text is identical for a stale token and for a real permission gap, so
//! the response cannot tell them apart ([`crate::forge_denial`] keeps
//! classifying it as a permission denial). The only safe discriminator is
//! empirical, and it is bounded: **one** forced per-owner mint, **one** re-run
//! of the same write, then stop. A retry that fails surfaces the ORIGINAL
//! error unchanged.
//!
//! What never triggers it: a rate limit or secondary rate limit (also a
//! `403`), any other failure, a refresh that could do nothing, or an owner
//! that already had a forced mint inside [`DEBOUNCE_WINDOW`].
//!
//! After a mint that succeeds, the per-credential installation snapshot and
//! the write-scope probe verdicts cached for that credential are dropped
//! ([`invalidate_caches`]), so the #9548 gate re-reads under the new token
//! instead of serving a pre-mint answer for up to two hours.
//!
//! **Secrets.** Nothing here logs a forge response: log lines carry the
//! operation name, the workspace root and the owner, never `gh`'s stderr.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::Result;

use crate::forge_denial::{classify, Denial};

/// How long one owner's forced mint stands in for the next: a permanently
/// misconfigured repository costs at most one mint per owner per window.
/// Matches the shell helper's own re-mint window.
pub(crate) const DEBOUNCE_WINDOW: Duration = Duration::from_secs(600);

/// GitHub's refusal of a write the token's installation does not cover.
const SIGNATURE: &str = "not accessible by integration";

/// True when `error_message` (an error carrying `gh`'s stderr) is the
/// write-side stale-token signature: the integration refusal, and not a rate
/// limit of either kind. The write sibling of
/// [`crate::forge_listing::is_404_error`].
///
/// `gh issue|pr` reports a GraphQL refusal with no `(HTTP 403)` marker, so
/// the status is supplied here; the classifier still decides what a `403`
/// carrying this text is.
pub(crate) fn is_stale_token_signature(error_message: &str) -> bool {
    error_message.to_ascii_lowercase().contains(SIGNATURE)
        && !crate::rate_limit_breaker::indicates_rate_limit(error_message)
        && classify(Some(403), error_message, None) == Some(Denial::Permission)
}

/// Run `write`; when it fails with the stale-token signature and
/// `refresh(root)` reports a fresh credential, run it exactly once more.
///
/// The first error is what the caller gets whenever the write does not end up
/// succeeding: no refresh, a refresh that returned `false`, or a retry that
/// failed too. `write` therefore runs at most twice and `refresh` at most
/// once.
pub(crate) fn retry_once<T>(
    op: &str,
    root: &Path,
    refresh: &dyn Fn(&Path) -> bool,
    mut write: impl FnMut() -> Result<T>,
) -> Result<T> {
    let err = match write() {
        Ok(value) => return Ok(value),
        Err(e) => e,
    };
    if !is_stale_token_signature(&format!("{err:#}")) || !refresh(root) {
        return Err(err);
    }
    log::info!(
        "stale_token_retry: retrying {op} in {} after a forced per-owner credential refresh \
         (#9247)",
        root.display()
    );
    match write() {
        Ok(value) => Ok(value),
        Err(_) => {
            // At most once per owner per window: only a refresh that ran
            // reaches here, and refreshes are debounced.
            log::warn!(
                "stale_token_retry: {op} in {} was refused again after a fresh installation \
                 token; this is a real permission gap, not a stale token (#9247)",
                root.display()
            );
            Err(err)
        }
    }
}

/// [`retry_once`] with the daemon's own refresh ([`refresh`]): what a write
/// site calls.
pub(crate) fn run<T>(op: &str, root: &Path, write: impl FnMut() -> Result<T>) -> Result<T> {
    retry_once(op, root, &refresh, write)
}

/// When each owner last had a forced mint attempted from this path.
#[derive(Default)]
pub(crate) struct Debounce {
    last: Mutex<HashMap<String, Instant>>,
}

impl Debounce {
    /// Whether `owner` may mint at `now`; a `true` opens its window. Owners
    /// compare case-insensitively, as GitHub's do.
    pub(crate) fn admit(&self, owner: &str, now: Instant) -> bool {
        let mut last = self
            .last
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let key = owner.to_ascii_lowercase();
        let inside = last
            .get(&key)
            .is_some_and(|at| now.saturating_duration_since(*at) < DEBOUNCE_WINDOW);
        if !inside {
            last.insert(key, now);
        }
        !inside
    }
}

/// The debounced refresh for `root`'s owner: at most one `mint` per owner per
/// [`DEBOUNCE_WINDOW`], whatever it returns, and `invalidate` only after one
/// that succeeded. `false` means "do not retry": no resolvable owner, an
/// owner inside its window, or a mint that produced nothing.
pub(crate) fn refresh_using(
    debounce: &Debounce,
    now: Instant,
    root: &Path,
    mint: &dyn Fn(&Path) -> bool,
    invalidate: &dyn Fn(&Path),
) -> bool {
    let Some(owner_repo) = crate::credential_preflight::nwo_from_git_remote(root) else {
        return false;
    };
    let owner = crate::credential_preflight::owner_of_nwo(&owner_repo);
    if !debounce.admit(owner, now) {
        log::debug!(
            "stale_token_retry: no forced mint for {owner} ({owner_repo}): one was already \
             attempted inside the {}s window (#9247)",
            DEBOUNCE_WINDOW.as_secs()
        );
        return false;
    }
    if !mint(root) {
        // `info`: a mint that FAILED already warned in `credential_preflight`,
        // so this path adds no second `warn` for the owner's window.
        log::info!(
            "stale_token_retry: a write to {owner_repo} was refused as not accessible by the \
             integration and no fresh installation token could be had for {owner}; not retrying, \
             and not minting for {owner} again for {}s (#9247)",
            DEBOUNCE_WINDOW.as_secs()
        );
        return false;
    }
    invalidate(root);
    true
}

/// Production refresh: [`refresh_using`] over the process-wide debounce, the
/// real forced per-owner mint, and [`invalidate_caches`].
#[cfg(not(test))]
pub(crate) fn refresh(root: &Path) -> bool {
    static DEBOUNCE: std::sync::OnceLock<Debounce> = std::sync::OnceLock::new();
    // The credential the failed write ran under, read before the mint
    // re-points `root` at its per-owner directory.
    let before = crate::credential_preflight::gh_config_dir_for_root(root);
    refresh_using(
        DEBOUNCE.get_or_init(Debounce::default),
        Instant::now(),
        root,
        &crate::credential_preflight::force_refresh_owner_credential,
        &|root| invalidate_caches(root, before.as_deref()),
    )
}

/// Test builds never reach the real minter from a write site: the refresh is
/// whatever THIS test thread installed with [`set_test_refresh`], and `false`
/// (no retry) when it installed nothing.
#[cfg(test)]
pub(crate) fn refresh(root: &Path) -> bool {
    TEST_REFRESH.with(|slot| slot.borrow().as_ref().is_some_and(|f| f(root)))
}

/// A test thread's stand-in for the forced mint.
#[cfg(test)]
pub(crate) type TestRefresh = Box<dyn Fn(&Path) -> bool>;

#[cfg(test)]
thread_local! {
    static TEST_REFRESH: std::cell::RefCell<Option<TestRefresh>> =
        const { std::cell::RefCell::new(None) };
}

/// Install (or, with `None`, remove) this test thread's refresh.
#[cfg(test)]
pub(crate) fn set_test_refresh(refresh: Option<TestRefresh>) {
    TEST_REFRESH.with(|slot| *slot.borrow_mut() = refresh);
}

/// Drop what the write-scope gate cached for the credentials `root`'s writes
/// ran under: `before` (the directory in force when the write failed; `None`
/// is the process's own) and the one the mint just registered for `root`.
/// For each: the installation snapshot is forgotten and every cached
/// write-scope verdict is expired, so the next gate re-probes under the new
/// token. A verified WRITE's stale-while-unverifiable grace is kept.
pub(crate) fn invalidate_caches(root: &Path, before: Option<&Path>) {
    use crate::forge_repo_facts::installation;
    use crate::write_scope::probe;
    let after = crate::credential_preflight::gh_config_dir_for_root(root);
    let mut dirs: Vec<Option<PathBuf>> = vec![before.map(Path::to_path_buf)];
    if after.as_deref() != before {
        dirs.push(after);
    }
    for dir in dirs {
        installation::forget(&installation::Credential::writer(dir.clone()));
        probe::expire_credential(dir.as_deref(), &probe::CacheScope::GitHub);
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
pub(crate) mod tests;
