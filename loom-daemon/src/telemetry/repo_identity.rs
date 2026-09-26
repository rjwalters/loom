//! GitHub repository identity (`repo_id` + canonical `full_name`) for the D32
//! v1 story key (#9068), resolved once per repo and cached per process.
//!
//! Mirrors [`super::visibility`]'s shape — a process-global map keyed on the
//! lowercased `owner/repo`, one `gh api repos/{owner}/{repo}` probe (the same
//! endpoint visibility reads) — with two differences that follow from what is
//! cached:
//!
//! - A **failure is remembered** for [`NEGATIVE_TTL`]: every dispatch of an
//!   issue in an unresolvable repo would otherwise pay a blocking `gh` call.
//! - There is **no fallback**. An unresolvable repo has no story key, and the
//!   caller makes the execution its own root. A name-derived key would mint a
//!   second, wrong story trace that never joins the reconciler's.
//!
//! The failure is logged once per repo per process, not per dispatch.
use std::collections::{HashMap, HashSet};
use std::process::{Command, Stdio};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

/// A repo's durable id and its canonical name at resolution time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoIdentity {
    pub id: u64,
    /// `Owner/Name` as GitHub spells it (the `loom.story` / `Loom-Story` text).
    pub full_name: String,
}

/// How long a resolved identity is trusted. `repo_id` never changes for a
/// repo, but a slug can be renamed away and reused, so it is not forever.
pub const POSITIVE_TTL: Duration = Duration::from_secs(3600);
/// How long an unresolvable repo is not re-probed.
pub const NEGATIVE_TTL: Duration = Duration::from_secs(300);
const PROBE_TIMEOUT: Duration = Duration::from_secs(10);

struct Entry {
    identity: Option<RepoIdentity>,
    at: Instant,
}

fn cache() -> &'static Mutex<HashMap<String, Entry>> {
    static CACHE: OnceLock<Mutex<HashMap<String, Entry>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn key(owner_repo: &str) -> String {
    owner_repo.trim().to_ascii_lowercase()
}

/// The identity of `owner_repo`, probing the forge when uncached or expired.
/// Blocks on a cold cache (one bounded `gh` call).
#[must_use]
pub fn resolve(owner_repo: &str) -> Option<RepoIdentity> {
    resolve_with(owner_repo, fetch_via_gh)
}

/// [`resolve`] with the probe injected, for tests.
pub fn resolve_with<F>(owner_repo: &str, fetch: F) -> Option<RepoIdentity>
where
    F: FnOnce(&str) -> Option<RepoIdentity>,
{
    let key = key(owner_repo);
    let mut guard = cache()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(entry) = guard.get(&key) {
        let ttl = if entry.identity.is_some() {
            POSITIVE_TTL
        } else {
            NEGATIVE_TTL
        };
        if entry.at.elapsed() < ttl {
            return entry.identity.clone();
        }
    }
    let identity = fetch(owner_repo.trim());
    guard.insert(
        key.clone(),
        Entry {
            identity: identity.clone(),
            at: Instant::now(),
        },
    );
    drop(guard);
    if identity.is_none() {
        warn_once(&key);
    }
    identity
}

/// Seed the cache, bypassing the forge. A test seam (integration tests share
/// this process-global cache with the code under test); `None` records the
/// repo as unresolvable.
#[doc(hidden)]
pub fn seed(owner_repo: &str, identity: Option<RepoIdentity>) {
    cache()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(
            key(owner_repo),
            Entry {
                identity,
                at: Instant::now(),
            },
        );
}

fn warn_once(key: &str) {
    static WARNED: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    let first = WARNED
        .get_or_init(|| Mutex::new(HashSet::new()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(key.to_string());
    if first {
        log::warn!(
            "telemetry: cannot resolve the GitHub repo_id of {key}; its issue executions \
             are their own trace roots (no D32 story key) until it resolves"
        );
    }
}

fn fetch_via_gh(owner_repo: &str) -> Option<RepoIdentity> {
    let mut cmd = Command::new("gh");
    cmd.args([
        "api",
        &format!("repos/{owner_repo}"),
        "--jq",
        r#""\(.id) \(.full_name)""#,
    ])
    .stdin(Stdio::null())
    .stderr(Stdio::null());
    crate::credential_preflight::apply_gh_config_for_owner_slug(&mut cmd, owner_repo);
    match crate::proc_exec::run_bounded(cmd, PROBE_TIMEOUT) {
        Ok(crate::proc_exec::Completion::Exited(out)) if out.status.success() => {
            parse(&String::from_utf8_lossy(&out.stdout))
        }
        _ => None,
    }
}

/// Parse `"<id> <owner/name>"`. A zero or non-decimal id is no identity.
#[must_use]
pub fn parse(output: &str) -> Option<RepoIdentity> {
    let (id, full_name) = output.trim().split_once(' ')?;
    if id.is_empty() || !id.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let id: u64 = id.parse().ok().filter(|id| *id > 0)?;
    let full_name = full_name.trim();
    if full_name.split('/').filter(|s| !s.is_empty()).count() != 2 {
        return None;
    }
    Some(RepoIdentity {
        id,
        full_name: full_name.to_string(),
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::cell::Cell;

    #[test]
    fn parses_id_and_full_name() {
        assert_eq!(
            parse("1073994527 rjwalters/loom\n"),
            Some(RepoIdentity {
                id: 1_073_994_527,
                full_name: "rjwalters/loom".into()
            })
        );
        for bad in [
            "",
            "null null",
            "0 a/b",
            "-1 a/b",
            "12 loom",
            "x1 a/b",
            "12",
        ] {
            assert_eq!(parse(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn a_resolution_is_cached_case_insensitively() {
        let calls = Cell::new(0);
        let fetch = |_: &str| {
            calls.set(calls.get() + 1);
            parse("7 Owner-A/Cached")
        };
        assert_eq!(resolve_with("owner-a/cached", fetch).unwrap().id, 7);
        assert_eq!(resolve_with(" OWNER-A/Cached ", fetch).unwrap().full_name, "Owner-A/Cached");
        assert_eq!(calls.get(), 1);
    }

    #[test]
    fn a_failure_is_remembered_and_never_substituted() {
        let calls = Cell::new(0);
        let fetch = |_: &str| {
            calls.set(calls.get() + 1);
            None
        };
        assert_eq!(resolve_with("owner-b/unresolvable", fetch), None);
        assert_eq!(resolve_with("owner-b/unresolvable", fetch), None);
        assert_eq!(calls.get(), 1, "negative answers are cached for NEGATIVE_TTL");
    }
}
