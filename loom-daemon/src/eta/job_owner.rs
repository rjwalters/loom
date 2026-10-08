//! Who runs the ETA-only singleton jobs (#10918): the `eta-fleet-refresh`
//! refresh half and `eta-nightly-folds` (and the retirement filing that reads
//! the folds).
//!
//! The ETA authority (#10498) fits locally on its own snapshots and never
//! takes a published fit. When those jobs followed `fleet.captain` and
//! `fleet.etaAuthority` named another host, the authority's fit inputs went
//! stale. So:
//!
//! - **`fleet.etaAuthority` / `LOOM_ETA_AUTHORITY` set explicitly**: the jobs
//!   follow the authority. The named host runs them ([`Owner::Authority`]),
//!   and every other host, the captain included, stands down
//!   ([`Owner::AuthorityElsewhere`]). There is still one refresher fleet-wide.
//! - **Not set**: `fleet.captain` decides, exactly as before
//!   ([`Owner::Captain`]). The authority is then the captain anyway
//!   ([`super::authority`] rule 2).
//!
//! The resolution is pure and re-read every tick, so moving
//! `fleet.etaAuthority` needs no restart. It reads config and env only: it
//! never arms a singleton job. The callers do that.

use std::path::Path;

use crate::fleet_captain::{self, CaptainGate};

/// The owner of the ETA singleton jobs, as seen from one host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Owner {
    /// `fleet.etaAuthority` names this host: the jobs run here, whatever
    /// `fleet.captain` says.
    Authority,
    /// `fleet.etaAuthority` names another host: they run there, not here.
    AuthorityElsewhere {
        /// The explicit ETA authority's host id.
        authority: String,
    },
    /// No explicit authority: the `fleet.captain` gate, unchanged.
    Captain(CaptainGate),
}

impl Owner {
    /// Whether the explicit ETA authority owns the jobs, here or elsewhere.
    #[must_use]
    pub fn is_explicit(&self) -> bool {
        !matches!(self, Self::Captain(_))
    }
}

/// Pure: decide the owner from the explicit authority, the declared captain
/// and this host's id (exact string equality, like the captain gate).
#[must_use]
pub fn resolve(explicit: Option<&str>, captain: Option<&str>, host_id: &str) -> Owner {
    match explicit.map(str::trim).filter(|h| !h.is_empty()) {
        Some(authority) if authority == host_id => Owner::Authority,
        Some(authority) => Owner::AuthorityElsewhere {
            authority: authority.to_string(),
        },
        None => Owner::Captain(fleet_captain::resolve_gate(captain, host_id)),
    }
}

/// [`resolve`] over `root`'s effective config and `env` (env > config for
/// the authority, as in [`super::authority::resolve_with`]).
#[must_use]
pub fn resolve_with(root: &Path, host_id: &str, env: impl Fn(&str) -> Option<String>) -> Owner {
    let explicit = super::authority::explicit_with(root, env);
    let captain = crate::config_resolver::fleet_captain(root);
    resolve(explicit.as_deref(), captain.as_deref(), host_id)
}

/// [`resolve_with`] over the process environment.
#[must_use]
pub fn resolve_for_root(root: &Path, host_id: &str) -> Owner {
    resolve_with(root, host_id, |k| std::env::var(k).ok())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn an_explicit_authority_owns_the_jobs_whoever_the_captain_is() {
        assert_eq!(resolve(Some("w1"), Some("cap"), "w1"), Owner::Authority);
        assert_eq!(
            resolve(Some("w1"), Some("cap"), "cap"),
            Owner::AuthorityElsewhere {
                authority: "w1".into()
            },
            "the captain stands down: one refresher fleet-wide"
        );
        assert_eq!(resolve(Some(" w1 "), None, "w1"), Owner::Authority, "trimmed");
    }

    #[test]
    fn an_authority_that_is_the_captain_owns_them_too() {
        assert_eq!(resolve(Some("cap"), Some("cap"), "cap"), Owner::Authority);
        assert!(matches!(
            resolve(Some("cap"), Some("cap"), "w2"),
            Owner::AuthorityElsewhere { .. }
        ));
    }

    #[test]
    fn without_an_explicit_authority_the_captain_gate_is_unchanged() {
        for (captain, host) in [(Some("cap"), "cap"), (Some("cap"), "w2"), (None, "w2")] {
            let owner = resolve(None, captain, host);
            assert_eq!(owner, Owner::Captain(fleet_captain::resolve_gate(captain, host)));
            assert!(!owner.is_explicit());
        }
        assert_eq!(
            resolve(Some("  "), Some("cap"), "cap"),
            Owner::Captain(CaptainGate::Armed {
                captain: "cap".into()
            }),
            "a blank key is no key"
        );
    }

    #[test]
    fn the_env_override_and_the_config_key_are_both_read() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(crate::config_resolver::LEGACY_CONFIG_REL);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, r#"{"fleet": {"captain": "cap", "etaAuthority": "w1"}}"#).unwrap();
        assert_eq!(resolve_with(dir.path(), "w1", |_| None), Owner::Authority);
        let env = |k: &str| (k == super::super::authority::ENV).then(|| "w2".to_string());
        assert_eq!(resolve_with(dir.path(), "w2", env), Owner::Authority, "env > config");
    }
}
