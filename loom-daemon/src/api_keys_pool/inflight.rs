//! Per-account **in-flight lease** store: how many spawns currently hold one
//! API-key account, so [`super::select`] can honour a declared
//! [`super::limits::AccountLimits::max_concurrent`] (#8424 item 4).
//!
//! # Why a new counter (the existing ones were checked first)
//!
//! #8424 asks explicitly whether `inflight.rs` or `admission_brake.rs` already
//! track per-credential concurrency. They do not, and neither can be widened
//! to:
//!
//! | Module | Keyed by | Why it cannot answer this |
//! |---|---|---|
//! | [`crate::inflight`] | fingerprint of (command, tree, branch) | identity of a *verification command*, deliberately deduplicating identical work; a credential is not in the key at all |
//! | [`crate::admission_brake`] | host load-per-core | one point-in-time reading for the whole host; says nothing about which account a spawn will pick, and never runs on the spawn path |
//! | [`crate::build_slot`] | anonymous machine-wide slot | count only, no identity — "may I run something heavy" |
//!
//! So this is the third, credential-keyed member of that family, and it reuses
//! their primitives rather than inventing new ones: one file per live spawn,
//! PID-liveness reaping via [`crate::live_claim::pid_is_live_process`], and an
//! age backstop.
//!
//! # The exec boundary is what makes this cheap
//!
//! A native harness spawn `exec`s (`worker_spawn::exec`), so the process that
//! *selected* the account **becomes** the harness: one PID covers selection and
//! the whole run, and the lease's owner PID dies exactly when the run ends.
//! There is therefore no release to forget — a dead owner is a released lease,
//! reaped lazily by the next reader ([`live_count`]). [`Lease::release`] exists
//! for the callers that do not exec (tests, and any future supervised path),
//! and like [`crate::inflight`] this is deliberately **not RAII**: dropping a
//! lease does nothing, because the common owner drops it microseconds before
//! `exec` hands the PID to the harness.
//!
//! # Degrades open, always
//!
//! An unusable lease store (unwritable provider dir, a file where a directory
//! belongs) yields "no leases" and lets the spawn proceed uncapped. This is the
//! **opposite** of how the pool treats `.disabled` / `.bad_accounts.json` /
//! `.limits.json`, and the distinction is deliberate: those encode an
//! operator's decision about eligibility, so failing to read one must never
//! silently widen it. A live-lease count is Loom's own bookkeeping — refusing
//! to spawn because a counter directory is broken would convert a politeness
//! throttle into an outage, exactly the reasoning [`crate::inflight`]'s
//! `DegradedOpen` and [`crate::build_slot`]'s degrade-open path already follow.

use std::path::{Path, PathBuf};

use super::paths::provider_dir;

/// Directory under a provider's pool dir holding one subdirectory per account.
pub const INFLIGHT_DIR: &str = ".inflight";

/// Env override (whole seconds, must parse `> 0`) for [`DEFAULT_STALE_SECS`].
pub const INFLIGHT_STALE_SECS_ENV: &str = "LOOM_API_KEY_INFLIGHT_STALE_SECS";

/// Age backstop for a lease whose owner PID cannot be trusted (recycled, or
/// never recorded): 4 hours, matching [`crate::inflight::DEFAULT_STALE_SECS`].
/// Long on purpose — a lease describes a whole sweep or role tick, and a short
/// threshold would let a peer over-admit against an account that is genuinely
/// still busy, which is the ceiling breach this module exists to prevent.
pub const DEFAULT_STALE_SECS: u64 = 14_400;

/// One live spawn's hold on an account.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct Holder {
    pid: u32,
    started_at: u64,
}

/// A registered hold. See the module docs: **not** released on drop.
#[derive(Debug)]
pub struct Lease {
    path: PathBuf,
}

impl Lease {
    /// The lease file, for a caller that wants to log or assert on it.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Release explicitly. Best-effort: a failure means the lease ages out or
    /// is reaped on the owner's death instead, never that a caller must retry.
    pub fn release(self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

#[must_use]
fn resolve_stale() -> u64 {
    std::env::var(INFLIGHT_STALE_SECS_ENV)
        .ok()
        .and_then(|raw| raw.trim().parse::<u64>().ok())
        .filter(|secs| *secs > 0)
        .unwrap_or(DEFAULT_STALE_SECS)
}

fn account_dir(root: &Path, provider: &str, name: &str) -> PathBuf {
    provider_dir(root, provider).join(INFLIGHT_DIR).join(name)
}

/// How many live spawns currently hold `provider/name`, reaping dead and
/// over-age leases as it counts.
///
/// Never fails: an unreadable store counts as `0` (see the module docs on
/// degrading open).
#[must_use]
pub fn live_count(root: &Path, provider: &str, name: &str) -> u32 {
    let dir = account_dir(root, provider, name);
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return 0;
    };
    let now = super::bad_marks::epoch_now();
    let stale = resolve_stale();
    let mut live = 0u32;
    for entry in entries.filter_map(Result::ok) {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let holder = std::fs::read_to_string(&path)
            .ok()
            .and_then(|body| serde_json::from_str::<Holder>(&body).ok());
        let alive = match &holder {
            // An unparsable lease file is reaped on age alone: it proves a
            // spawn touched this account, but nothing about whose PID.
            None => path
                .metadata()
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.elapsed().ok())
                .is_some_and(|age| age.as_secs() < stale),
            Some(holder) => {
                crate::live_claim::pid_is_live_process(holder.pid)
                    && now.saturating_sub(holder.started_at) < stale
            }
        };
        if alive {
            live = live.saturating_add(1);
        } else {
            let _ = std::fs::remove_file(&path);
        }
    }
    live
}

/// Register this process as holding `provider/name`.
///
/// Returns `None` when the store could not be written — the spawn proceeds
/// uncapped rather than failing (module docs). The lease is keyed on this
/// process's PID plus a uuid, so several holds by one PID (a supervised caller
/// that does not exec) each get their own file.
#[must_use]
pub fn register(root: &Path, provider: &str, name: &str) -> Option<Lease> {
    register_for(root, provider, name, std::process::id())
}

/// [`register`] for an explicit owner PID — the seam tests use to fabricate a
/// live or dead holder without spawning processes.
#[must_use]
pub fn register_for(root: &Path, provider: &str, name: &str, pid: u32) -> Option<Lease> {
    let dir = account_dir(root, provider, name);
    std::fs::create_dir_all(&dir).ok()?;
    super::registry::restrict_dir(&dir);
    let path = dir.join(format!("{pid}-{}.json", uuid::Uuid::new_v4()));
    let holder = Holder {
        pid,
        started_at: super::bad_marks::epoch_now(),
    };
    let body = serde_json::to_string(&holder).ok()?;
    // `write_secret`: atomic replace at `0600`, keeping the whole provider
    // directory uniform. A lease holds no key material, only a PID.
    super::registry::write_secret(&path, &body).ok()?;
    Some(Lease { path })
}

/// Drop every lease recorded for `provider/name` — called by
/// `registry::remove` so a name re-registered later does not inherit phantom
/// holds. Best-effort.
pub(super) fn forget(root: &Path, provider: &str, name: &str) {
    let _ = std::fs::remove_dir_all(account_dir(root, provider, name));
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

    /// A PID that is certainly not a live process. `u32::MAX` is above every
    /// platform's `pid_max`, so it can never be recycled onto a real process
    /// mid-test.
    const DEAD_PID: u32 = u32::MAX;

    #[test]
    fn an_account_with_no_leases_counts_zero() {
        let tmp = pool(&["alpha"]);
        assert_eq!(live_count(tmp.path(), "zai", "alpha"), 0);
    }

    #[test]
    fn registering_counts_this_process_and_releasing_stops_counting_it() {
        let tmp = pool(&["alpha"]);
        let first = register(tmp.path(), "zai", "alpha").unwrap();
        assert_eq!(live_count(tmp.path(), "zai", "alpha"), 1);
        let second = register(tmp.path(), "zai", "alpha").unwrap();
        assert_eq!(live_count(tmp.path(), "zai", "alpha"), 2);
        // Leases are per-account, not per-provider.
        assert_eq!(live_count(tmp.path(), "zai", "beta"), 0);
        first.release();
        assert_eq!(live_count(tmp.path(), "zai", "alpha"), 1);
        second.release();
        assert_eq!(live_count(tmp.path(), "zai", "alpha"), 0);
    }

    /// The property the exec boundary relies on: nobody has to release. A
    /// lease whose owner PID is gone is not counted, and is reaped.
    #[cfg(unix)]
    #[test]
    fn a_dead_owners_lease_is_not_counted_and_is_reaped() {
        let tmp = pool(&["alpha"]);
        let lease = register_for(tmp.path(), "zai", "alpha", DEAD_PID).unwrap();
        let path = lease.path().to_path_buf();
        assert!(path.exists());
        assert_eq!(live_count(tmp.path(), "zai", "alpha"), 0);
        assert!(!path.exists(), "dead lease was not reaped");
    }

    /// Dropping a lease must NOT release it — the owner drops it immediately
    /// before `exec` replaces the process image.
    #[test]
    fn dropping_a_lease_keeps_it() {
        let tmp = pool(&["alpha"]);
        let path = {
            let lease = register(tmp.path(), "zai", "alpha").unwrap();
            lease.path().to_path_buf()
        };
        assert!(path.exists(), "drop released the lease");
        assert_eq!(live_count(tmp.path(), "zai", "alpha"), 1);
    }

    #[test]
    fn an_over_age_lease_is_reaped_even_with_a_live_owner() {
        let tmp = pool(&["alpha"]);
        let lease = register(tmp.path(), "zai", "alpha").unwrap();
        // Backdate the holder past the staleness threshold.
        let body = std::fs::read_to_string(lease.path()).unwrap();
        let mut holder: Holder = serde_json::from_str(&body).unwrap();
        holder.started_at = super::super::bad_marks::epoch_now() - (DEFAULT_STALE_SECS + 1);
        std::fs::write(lease.path(), serde_json::to_string(&holder).unwrap()).unwrap();
        assert_eq!(live_count(tmp.path(), "zai", "alpha"), 0);
        assert!(!lease.path().exists());
    }

    /// A lease file nothing can parse still proves a spawn touched the
    /// account, so it counts until it ages out — over-counting is the safe
    /// direction for a ceiling.
    #[test]
    fn an_unparsable_lease_file_counts_until_it_ages_out() {
        let tmp = pool(&["alpha"]);
        let lease = register(tmp.path(), "zai", "alpha").unwrap();
        std::fs::write(lease.path(), "{torn").unwrap();
        assert_eq!(live_count(tmp.path(), "zai", "alpha"), 1);
        assert!(lease.path().exists());
    }

    #[test]
    #[serial_test::serial]
    fn the_staleness_threshold_is_env_overridable_and_rejects_junk() {
        assert_eq!(resolve_stale(), DEFAULT_STALE_SECS);
        for (raw, expected) in [
            ("60", 60),
            ("0", DEFAULT_STALE_SECS),
            ("", DEFAULT_STALE_SECS),
            ("x", DEFAULT_STALE_SECS),
        ] {
            std::env::set_var(INFLIGHT_STALE_SECS_ENV, raw);
            let resolved = resolve_stale();
            std::env::remove_var(INFLIGHT_STALE_SECS_ENV);
            assert_eq!(resolved, expected, "{raw:?}");
        }
    }

    #[test]
    fn removing_an_account_forgets_its_leases() {
        let tmp = pool(&["alpha"]);
        register(tmp.path(), "zai", "alpha").unwrap();
        assert_eq!(live_count(tmp.path(), "zai", "alpha"), 1);
        super::super::registry::remove(tmp.path(), "zai", "alpha").unwrap();
        assert_eq!(live_count(tmp.path(), "zai", "alpha"), 0);
    }

    #[test]
    fn an_unwritable_store_degrades_open_rather_than_erroring() {
        // A file where the `.inflight` directory belongs: `create_dir_all`
        // fails, `register` reports no lease, and counting reports zero.
        let tmp = pool(&["alpha"]);
        let dir = super::super::paths::provider_dir(tmp.path(), "zai").join(INFLIGHT_DIR);
        std::fs::write(&dir, "not a directory").unwrap();
        assert!(register(tmp.path(), "zai", "alpha").is_none());
        assert_eq!(live_count(tmp.path(), "zai", "alpha"), 0);
    }
}
