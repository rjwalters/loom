//! The claim-ownership marker a sweep's Builder worker is launched with
//! (Issue #11863). Split from `phase_run.rs`, which sits at the file-size
//! threshold.

use super::phase_run::Role;

/// The `LOOM_SWEEP_CLAIM_OWNED` value a Builder worker must be launched with
/// (#11863), or `None` to leave the inherited env alone.
///
/// `worker run --role builder` is only called from a sweep's Builder phase,
/// after that sweep's pre-flight published its lease on the issue (and the
/// daemon, when it dispatched the sweep, flipped `loom:building`). The
/// worker's whole prompt is `/loom:builder <N>`, whose targeted gate runs
/// `forge check-claim` and refuses `BUILDING` / `LEASE_ALREADY_HELD` unless
/// `LOOM_SWEEP_CLAIM_OWNED` names `N`. A daemon-dispatched sweep already
/// exports it; an attended `/loom:sweep` does not, so its Builder refused its
/// own sweep's lease. An inherited non-empty value is the daemon's and is
/// kept as is.
#[must_use]
pub fn builder_claim_owned(role: Role, inherited: Option<&str>, issue: u64) -> Option<String> {
    let unset = inherited.is_none_or(|v| v.trim().is_empty());
    (role == Role::Builder && unset).then(|| issue.to_string())
}

/// Set [`builder_claim_owned`]'s marker on the worker `cmd`, read against
/// this process's own `LOOM_SWEEP_CLAIM_OWNED`.
pub fn apply(cmd: &mut std::process::Command, role: Role, issue: u64) {
    let inherited = std::env::var("LOOM_SWEEP_CLAIM_OWNED").ok();
    if let Some(owned) = builder_claim_owned(role, inherited.as_deref(), issue) {
        cmd.env("LOOM_SWEEP_CLAIM_OWNED", owned);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// #11863: an attended sweep's Builder worker gets the ownership marker
    /// for its own issue; a daemon's inherited marker is kept; Doctor never
    /// gets one from here.
    #[test]
    fn builder_worker_is_told_its_sweep_owns_the_claim() {
        assert_eq!(builder_claim_owned(Role::Builder, None, 42), Some("42".into()));
        assert_eq!(builder_claim_owned(Role::Builder, Some(" "), 42), Some("42".into()));
        assert_eq!(builder_claim_owned(Role::Builder, Some("42"), 42), None);
        assert_eq!(builder_claim_owned(Role::Builder, Some("7"), 42), None);
        assert_eq!(builder_claim_owned(Role::Doctor, None, 42), None);
    }
}
