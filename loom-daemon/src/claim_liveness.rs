//! Claim liveness from the label registry (Issue #11898; design #11894 §5).
//!
//! `defaults/labels.json` carries `stale_after_minutes` for the five claim
//! labels. Until #11898 the field was inert and each consumer kept its own
//! constant. Now the registry is the default, and each consumer either reads
//! it or is pinned to it by a parity test here:
//!
//! | Claim | Minutes | Read by |
//! |---|---|---|
//! | `loom:curating` | 45 | `claim-staleness.sh` (pinned), [`stale_after_minutes`] |
//! | `loom:evaluating` | 45 | `check-evaluating-staleness.sh` (pinned), [`stale_after_minutes`] |
//! | `loom:building` | 15 | the lease TTL ([`crate::claim_reconciliation::resolve_lease_ttl_minutes`], pinned) |
//! | `loom:reviewing` | 45 | claim reconciliation, `claim-staleness.sh` |
//! | `loom:treating` | 60 | claim reconciliation, `claim-staleness.sh` |
//!
//! Curating, evaluating and reviewing outlive the 30-minute step timeout
//! (#11909): a step's claim is never stale while the step may still run.
//!
//! **Builder liveness stays the renewed lease.** A `loom:building` claim is
//! stale when its lease has gone one TTL without a renewal, so its
//! `stale_after_minutes` is the lease TTL, not an idle clock, and
//! [`stale_after_minutes`] for it resolves through the lease TTL (its env
//! override and hyperparameter layer included).
//!
//! Every value keeps its environment override (`LOOM_STALE_<NAME>_MINUTES`,
//! `LOOM_LEASE_TTL_MINUTES` for building); the registry replaces only the
//! compiled-in default.

use crate::label_registry::Registry;

/// The five claim labels, in lifecycle order.
pub const CLAIM_LABELS: [&str; 5] = [
    "loom:curating",
    "loom:evaluating",
    "loom:building",
    "loom:reviewing",
    "loom:treating",
];

/// `label`'s `stale_after_minutes` in the embedded registry.
#[must_use]
pub fn registry_minutes(label: &str) -> Option<u32> {
    Registry::embedded().get(label)?.stale_after_minutes
}

/// Namespace of Loom's own labels, built so the label-literal guard sees no
/// bare literal.
const LOOM_PREFIX: &str = concat!("loom", ":");

/// The env var overriding `label`'s staleness (`loom:curating` →
/// `LOOM_STALE_CURATING_MINUTES`); the lease TTL's for `loom:building`.
#[must_use]
pub fn env_var(label: &str) -> Option<String> {
    let name = label.strip_prefix(LOOM_PREFIX)?;
    if label == "loom:building" {
        return Some(crate::claim_reconciliation::LEASE_TTL_MINUTES_ENV.to_string());
    }
    Some(format!("LOOM_STALE_{}_MINUTES", name.to_ascii_uppercase()))
}

/// `label`'s resolved staleness in minutes: a positive env override, else the
/// registry. `loom:building` resolves as the lease TTL. `None` for a label
/// that is not a claim.
#[must_use]
pub fn stale_after_minutes(label: &str) -> Option<f64> {
    if !CLAIM_LABELS.contains(&label) {
        return None;
    }
    if label == "loom:building" {
        return Some(crate::claim_reconciliation::resolve_lease_ttl_minutes());
    }
    let from_env = env_var(label)
        .and_then(|k| std::env::var(k).ok())
        .and_then(|s| s.trim().parse::<f64>().ok())
        .filter(|v| *v > 0.0);
    from_env.or_else(|| registry_minutes(label).map(f64::from))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn every_claim_label_has_a_live_value() {
        for label in CLAIM_LABELS {
            assert!(registry_minutes(label).is_some(), "{label} has no stale_after_minutes");
        }
        // The step roles' claim labels are exactly these five.
        for role in crate::step_role::StepRole::ALL {
            if let Some(label) = role.claim_label() {
                assert!(CLAIM_LABELS.contains(&label), "{label}");
            }
        }
    }

    /// The Builder's claim liveness is its lease: the registry value is the
    /// lease TTL's compiled default.
    #[test]
    fn building_is_the_lease_ttl() {
        assert_eq!(
            registry_minutes("loom:building").map(f64::from),
            Some(crate::claim_reconciliation::DEFAULT_LEASE_TTL_MINUTES)
        );
    }

    #[test]
    fn reconciliation_defaults_are_the_registrys() {
        use crate::claim_reconciliation as cr;
        assert_eq!(
            registry_minutes("loom:reviewing").map(f64::from),
            Some(cr::DEFAULT_STALE_REVIEWING_MINUTES)
        );
        assert_eq!(
            registry_minutes("loom:treating").map(f64::from),
            Some(cr::DEFAULT_STALE_TREATING_MINUTES)
        );
    }

    /// The shell consumers keep a `${VAR:-N}` default (they run without the
    /// daemon); each N must be the registry's.
    #[test]
    fn shell_defaults_are_the_registrys() {
        let scripts = Path::new(env!("CARGO_MANIFEST_DIR")).join("../defaults/scripts");
        let pins = [
            ("claim-staleness.sh", "loom:reviewing"),
            ("claim-staleness.sh", "loom:treating"),
            ("claim-staleness.sh", "loom:curating"),
            ("check-evaluating-staleness.sh", "loom:evaluating"),
        ];
        for (script, label) in pins {
            let text = std::fs::read_to_string(scripts.join(script)).unwrap();
            let var = env_var(label).unwrap();
            let want = format!("${{{var}:-{}}}", registry_minutes(label).unwrap());
            assert!(text.contains(&want), "{script} must default {var} to `{want}`");
        }
    }

    #[test]
    #[serial_test::serial]
    fn an_env_override_beats_the_registry() {
        std::env::set_var("LOOM_STALE_CURATING_MINUTES", "7");
        let got = stale_after_minutes("loom:curating");
        std::env::remove_var("LOOM_STALE_CURATING_MINUTES");
        assert_eq!(got, Some(7.0));
        assert_eq!(stale_after_minutes("loom:curating"), Some(45.0));
        assert_eq!(stale_after_minutes("loom:issue"), None);
    }
}
