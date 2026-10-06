//! One ETA authority per fleet (#10498).
//!
//! Every host used to run its own ETA tracker and emit `eta.estimate`,
//! `eta.outcome` and `eta.snapshot` from its own partial view, so SigNoz held
//! two or three conflicting estimates per item. Now exactly one host, the
//! **ETA authority**, computes and emits them, and fits `eta-fit/v1` locally.
//! Every other host emits no ETA record (the `eta` CLI stays read-only
//! usable) and drops its pending-estimate store so a stale outcome is never
//! scored there.
//!
//! # The rule
//!
//! 1. `fleet.etaAuthority` (or `LOOM_ETA_AUTHORITY`) names the authority:
//!    [`Reason::Explicit`]. It wins over everything, and later moves to the
//!    captain (2AMLogic/2am#2814).
//! 2. Otherwise the authority is the fleet-wide refresher: the declared
//!    `fleet.captain` (the only host whose fleet refresh runs, #10329), or,
//!    with no captain declared, this host when its own
//!    `autonomous.eta.fleetRefresh.enabled` is on: [`Reason::FleetRefresh`].
//!    `fleetRefresh.enabled` defaults to `true`, so on a multi-host fleet
//!    declare `fleet.captain` or `fleet.etaAuthority`.
//! 3. More than one candidate with no explicit key: the lowest host id wins
//!    ([`Reason::LowestIdFallback`]) and the others log one warning per change
//!    naming the conflict.
//! 4. No candidate and no explicit key: nobody is authority
//!    ([`Reason::NoCandidate`]); `eta doctor` says so.
//!
//! # How a host learns its peers' state (decision)
//!
//! It does not. There is no fleet roster of peers' config, and adding a
//! network read to a per-tick gate would turn a peer outage into an ETA
//! outage. The candidate set is built from this host's own inputs only: its
//! own `fleetRefresh.enabled`, plus `fleet.captain` and `fleet.etaAuthority`
//! (committed config, identical on every host that shares the repo). The
//! multi-candidate path of [`resolve_eta_authority`] is exercised by the pure
//! function and reached in production only when a roster is supplied. The
//! documented limitation: two hosts that both default `fleetRefresh` on with
//! no captain and no `fleet.etaAuthority` both think they are the authority;
//! the fix is one config line, and `eta doctor` prints a reminder.
//!
//! The resolution is pure and re-read on every pass, so a config edit needs
//! no restart.

use std::path::Path;
use std::sync::Mutex;

/// Env override for `fleet.etaAuthority` (env > config).
pub const ENV: &str = "LOOM_ETA_AUTHORITY";

/// Why a host is (or is not) the authority.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reason {
    /// `fleet.etaAuthority` / `LOOM_ETA_AUTHORITY` names it.
    Explicit,
    /// The sole fleet-refresh candidate.
    FleetRefresh,
    /// Several candidates and no explicit key: the lowest host id.
    LowestIdFallback,
    /// No candidate and no explicit key.
    NoCandidate,
}

impl Reason {
    /// The wire / doctor name.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Reason::Explicit => "explicit",
            Reason::FleetRefresh => "fleet_refresh",
            Reason::LowestIdFallback => "lowest_id_fallback",
            Reason::NoCandidate => "no_candidate",
        }
    }
}

/// The fleet's ETA authority.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Authority {
    /// The authority host id; `None` for [`Reason::NoCandidate`].
    pub host: Option<String>,
    /// Why.
    pub reason: Reason,
    /// Qualifying hosts that are not the authority (a conflict when non-empty).
    pub others: Vec<String>,
}

/// Pick the authority from `explicit` and the sorted-or-not `candidates`.
/// Pure.
#[must_use]
pub fn resolve_eta_authority(explicit: Option<&str>, candidates: &[String]) -> Authority {
    let mut sorted: Vec<String> = candidates
        .iter()
        .map(|c| c.trim().to_string())
        .filter(|c| !c.is_empty())
        .collect();
    sorted.sort();
    sorted.dedup();
    if let Some(host) = explicit.map(str::trim).filter(|h| !h.is_empty()) {
        let others = sorted.into_iter().filter(|c| c != host).collect();
        return Authority {
            host: Some(host.to_string()),
            reason: Reason::Explicit,
            others,
        };
    }
    match sorted.len() {
        0 => Authority {
            host: None,
            reason: Reason::NoCandidate,
            others: Vec::new(),
        },
        1 => Authority {
            host: sorted.pop(),
            reason: Reason::FleetRefresh,
            others: Vec::new(),
        },
        _ => {
            let host = sorted.remove(0);
            Authority {
                host: Some(host),
                reason: Reason::LowestIdFallback,
                others: sorted,
            }
        }
    }
}

/// This host's view of the candidate set (see the module doc): the declared
/// captain alone when there is one, else this host when its own fleet refresh
/// is on.
#[must_use]
pub fn local_candidates(
    captain: Option<&str>,
    host_id: &str,
    local_refresh_enabled: bool,
) -> Vec<String> {
    match captain.map(str::trim).filter(|c| !c.is_empty()) {
        Some(captain) => vec![captain.to_string()],
        None if local_refresh_enabled => vec![host_id.to_string()],
        None => Vec::new(),
    }
}

/// A resolution for this host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolution {
    /// The fleet's authority.
    pub authority: Authority,
    /// This host's id.
    pub host_id: String,
    /// The `fleet.captain` that fed it, for `eta doctor`.
    pub captain: Option<String>,
    /// This host's own `fleetRefresh.enabled`.
    pub local_refresh_enabled: bool,
}

impl Resolution {
    /// Whether this host is the authority.
    #[must_use]
    pub fn is_authority(&self) -> bool {
        self.authority.host.as_deref() == Some(self.host_id.as_str())
    }

    /// One line for logs and `eta doctor`.
    #[must_use]
    pub fn describe(&self) -> String {
        let who = self.authority.host.as_deref().unwrap_or("none");
        let role = if self.is_authority() {
            "this host is the authority"
        } else {
            "this host is NOT the authority and emits no ETA records"
        };
        let mut line = format!("{who} ({}); {role}", self.authority.reason.as_str());
        if !self.authority.others.is_empty() {
            line.push_str(&format!(
                "; also qualifying: {} (set fleet.etaAuthority to settle it)",
                self.authority.others.join(", ")
            ));
        }
        if self.authority.reason == Reason::NoCandidate {
            line.push_str(
                "; no host qualifies: enable autonomous.eta.fleetRefresh on one host, or set \
                 fleet.etaAuthority",
            );
        }
        if self.authority.reason == Reason::FleetRefresh
            && self.captain.is_none()
            && self.local_refresh_enabled
        {
            line.push_str(
                "; no fleet.captain/fleet.etaAuthority declared, so every host with fleetRefresh \
                 on believes it is the authority",
            );
        }
        line
    }
}

/// Resolve this host's authority from `root`'s effective config and `env`.
#[must_use]
pub fn resolve_with(
    root: &Path,
    host_id: &str,
    env: impl Fn(&str) -> Option<String>,
) -> Resolution {
    let explicit = env(ENV)
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .or_else(|| crate::config_resolver::fleet_eta_authority(root));
    let captain = crate::config_resolver::fleet_captain(root);
    let local_refresh_enabled = super::config::read(root).fleet_refresh.enabled;
    let candidates = local_candidates(captain.as_deref(), host_id, local_refresh_enabled);
    Resolution {
        authority: resolve_eta_authority(explicit.as_deref(), &candidates),
        host_id: host_id.to_string(),
        captain,
        local_refresh_enabled,
    }
}

/// [`resolve_with`] over the process environment and this host's identity.
#[must_use]
pub fn resolve(root: &Path) -> Resolution {
    resolve_with(root, &crate::sweep_registry::host_identity(), |k| std::env::var(k).ok())
}

static LAST_LOGGED: Mutex<Option<String>> = Mutex::new(None);

/// Log `resolution` once per change: `info` normally, `warn` when it names a
/// conflict or no authority.
pub fn log_change(resolution: &Resolution) {
    let line = resolution.describe();
    let mut last = LAST_LOGGED
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if last.as_deref() == Some(line.as_str()) {
        return;
    }
    let conflict = !resolution.authority.others.is_empty()
        || resolution.authority.reason == Reason::NoCandidate;
    if conflict {
        log::warn!("eta authority: {line} (#10498)");
    } else {
        log::info!("eta authority: {line} (#10498)");
    }
    *last = Some(line);
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn hosts(names: &[&str]) -> Vec<String> {
        names.iter().map(|n| (*n).to_string()).collect()
    }

    fn write_config(root: &Path, json: &str) {
        let path = root.join(crate::config_resolver::LEGACY_CONFIG_REL);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, json).unwrap();
    }

    fn no_env(_: &str) -> Option<String> {
        None
    }

    #[test]
    fn explicit_wins_over_every_candidate() {
        let a = resolve_eta_authority(Some("loom-worker-2"), &hosts(&["a", "b"]));
        assert_eq!(a.host.as_deref(), Some("loom-worker-2"));
        assert_eq!(a.reason, Reason::Explicit);
        assert_eq!(a.others, hosts(&["a", "b"]));
    }

    #[test]
    fn the_sole_candidate_is_the_authority() {
        let a = resolve_eta_authority(None, &hosts(&["robb-studio"]));
        assert_eq!(a.host.as_deref(), Some("robb-studio"));
        assert_eq!(a.reason, Reason::FleetRefresh);
        assert!(a.others.is_empty());
    }

    #[test]
    fn several_candidates_fall_back_to_the_lowest_id_and_name_the_conflict() {
        let a =
            resolve_eta_authority(None, &hosts(&["robb-studio", "loom-worker-2", "loom-worker-1"]));
        assert_eq!(a.host.as_deref(), Some("loom-worker-1"));
        assert_eq!(a.reason, Reason::LowestIdFallback);
        assert_eq!(a.others, hosts(&["loom-worker-2", "robb-studio"]));
    }

    #[test]
    fn duplicates_and_blanks_are_not_a_conflict() {
        let a = resolve_eta_authority(None, &hosts(&["h", " h ", ""]));
        assert_eq!(a.reason, Reason::FleetRefresh);
        assert!(a.others.is_empty());
    }

    #[test]
    fn no_candidate_and_no_explicit_key_means_no_authority() {
        let a = resolve_eta_authority(None, &[]);
        assert_eq!(a.host, None);
        assert_eq!(a.reason, Reason::NoCandidate);
        let a = resolve_eta_authority(Some("  "), &[]);
        assert_eq!(a.reason, Reason::NoCandidate);
    }

    #[test]
    fn a_declared_captain_is_the_only_local_candidate() {
        assert_eq!(local_candidates(Some("cap"), "me", true), hosts(&["cap"]));
        assert_eq!(local_candidates(None, "me", true), hosts(&["me"]));
        assert!(local_candidates(None, "me", false).is_empty());
        assert!(local_candidates(Some(" "), "me", false).is_empty());
    }

    #[test]
    fn a_worker_with_fleet_refresh_off_is_not_the_authority() {
        let dir = tempdir().unwrap();
        write_config(
            dir.path(),
            r#"{"autonomous": {"eta": {"fleetRefresh": {"enabled": false}}}}"#,
        );
        let r = resolve_with(dir.path(), "loom-worker-1", no_env);
        assert!(!r.is_authority());
        assert_eq!(r.authority.reason, Reason::NoCandidate);
    }

    #[test]
    fn the_fleet_refresh_host_is_the_authority() {
        let dir = tempdir().unwrap();
        write_config(dir.path(), r#"{}"#);
        let r = resolve_with(dir.path(), "robb-studio", no_env);
        assert!(r.is_authority());
        assert_eq!(r.authority.reason, Reason::FleetRefresh);
    }

    #[test]
    fn the_authority_is_independent_of_the_captain() {
        let dir = tempdir().unwrap();
        // The captain names another host (it refreshes the fleet), yet the
        // explicit authority is this one: it is the authority and fits here.
        write_config(
            dir.path(),
            r#"{"fleet": {"captain": "loom-worker-1", "etaAuthority": "robb-studio"}}"#,
        );
        let me = resolve_with(dir.path(), "robb-studio", no_env);
        assert!(me.is_authority());
        assert_eq!(me.authority.reason, Reason::Explicit);
        assert_eq!(me.authority.others, hosts(&["loom-worker-1"]));
        let captain = resolve_with(dir.path(), "loom-worker-1", no_env);
        assert!(!captain.is_authority(), "the captain is not the ETA authority here");
    }

    #[test]
    fn without_an_explicit_key_the_captain_is_the_authority() {
        let dir = tempdir().unwrap();
        write_config(dir.path(), r#"{"fleet": {"captain": "loom-worker-1"}}"#);
        assert!(resolve_with(dir.path(), "loom-worker-1", no_env).is_authority());
        assert!(!resolve_with(dir.path(), "robb-studio", no_env).is_authority());
    }

    #[test]
    fn the_env_override_beats_the_config_key() {
        let dir = tempdir().unwrap();
        write_config(dir.path(), r#"{"fleet": {"etaAuthority": "a"}}"#);
        let env = |k: &str| (k == ENV).then(|| "b".to_string());
        let r = resolve_with(dir.path(), "b", env);
        assert!(r.is_authority());
        assert_eq!(r.authority.reason, Reason::Explicit);
    }

    #[test]
    fn describe_names_the_authority_and_the_reason() {
        let dir = tempdir().unwrap();
        write_config(dir.path(), r#"{"fleet": {"etaAuthority": "robb-studio"}}"#);
        let line = resolve_with(dir.path(), "loom-worker-1", no_env).describe();
        assert!(line.contains("robb-studio (explicit)"), "{line}");
        assert!(line.contains("NOT the authority"), "{line}");
    }
}
