//! The `owner_drift` health section (#14223).
//!
//! A managed checkout whose `origin` names an owner the forge does not
//! answer under — a repo transfer the remote was never re-pointed after —
//! sheds its reads under a stale credential until someone notices. The
//! daemon's own refresh tick detects both drift shapes and repairs the
//! re-pointed one (`credential_preflight::owner_drift`); this section
//! reports the canonical-name case, which any process can see, in the
//! one-shot fleet-vitals view.
//!
//! Collected client-side from the daemon-reported roots (bounded git
//! remote reads + one cached `GET repos/{nwo}` per distinct repo), like
//! the `queues`/`throughput` fan-out it sits beside. Degraded when drift
//! is found — the same exit code as every other degraded section, no new
//! one. No section at all when the collection could not run (the daemon
//! did not answer, so there is no root list): unknown, not an all-clear.
//! A per-repo forge lookup failure yields no verdict for that repo, never
//! a false drift.

use super::{HealthInputs, HealthSection, Verdict};

/// Assess the `owner_drift` section from the collected findings: `Some`
/// whenever the collection ran (`inputs.owner_drift` is `Some`) — Degraded
/// when any checkout drifted, Green when the check ran clean.
#[must_use]
pub fn assess_owner_drift(inputs: &HealthInputs) -> Option<HealthSection> {
    let findings = inputs.owner_drift.as_ref()?;
    if findings.is_empty() {
        return Some(HealthSection::new(
            "owner_drift",
            Verdict::Green,
            "every managed checkout's origin owner matches the forge's canonical owner",
            serde_json::json!({ "drifted": [] }),
        ));
    }
    let named = findings
        .iter()
        .map(|d| format!("{}: {} → {}", d.root.display(), d.remote_nwo, d.canonical_nwo))
        .collect::<Vec<_>>()
        .join("; ");
    Some(HealthSection::new(
        "owner_drift",
        Verdict::Degraded,
        format!(
            "{} checkout(s) name an owner the forge does not answer under — {named} — re-point \
             the remote at the canonical name and restart the daemon (#14223)",
            findings.len()
        ),
        serde_json::json!({
            "drifted": findings
                .iter()
                .map(|d| serde_json::json!({
                    "root": d.root.display().to_string(),
                    "remote_nwo": d.remote_nwo,
                    "canonical_nwo": d.canonical_nwo,
                }))
                .collect::<Vec<_>>(),
        }),
    ))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::credential_preflight::owner_drift::CanonicalDrift;
    use std::path::PathBuf;

    fn drift(root: &str, remote: &str, canonical: &str) -> CanonicalDrift {
        CanonicalDrift {
            root: PathBuf::from(root),
            remote_nwo: remote.to_string(),
            canonical_nwo: canonical.to_string(),
        }
    }

    fn inputs(found: Vec<CanonicalDrift>) -> HealthInputs {
        HealthInputs {
            owner_drift: Some(found),
            ..Default::default()
        }
    }

    #[test]
    fn no_section_when_the_collection_could_not_run() {
        // The daemon did not answer, so there is no root list: unknown, not
        // an all-clear.
        assert!(assess_owner_drift(&HealthInputs::default()).is_none());
    }

    #[test]
    fn a_clean_run_is_a_green_all_clear() {
        let s = assess_owner_drift(&inputs(vec![])).unwrap();
        assert_eq!(s.key, "owner_drift");
        assert_eq!(s.verdict, Verdict::Green);
        assert_eq!(s.detail["drifted"], serde_json::json!([]));
    }

    #[test]
    fn a_drifted_checkout_is_a_degraded_line_naming_both_names_and_the_cure() {
        let s = assess_owner_drift(&inputs(vec![
            drift("/w/a", "Old-owner/widget", "New-owner/widget"),
            drift("/w/b", "Acme/gadget", "Globex/gadget"),
        ]))
        .unwrap();
        assert_eq!(s.verdict, Verdict::Degraded);
        assert!(s.summary.starts_with("2 checkout(s)"), "{}", s.summary);
        assert!(
            s.summary
                .contains("/w/a: Old-owner/widget → New-owner/widget"),
            "{}",
            s.summary
        );
        assert!(s.summary.contains("/w/b: Acme/gadget → Globex/gadget"));
        assert!(s.summary.contains("#14223"));
        assert_eq!(s.detail["drifted"][0]["remote_nwo"], "Old-owner/widget");
        assert_eq!(s.detail["drifted"][0]["canonical_nwo"], "New-owner/widget");
        assert_eq!(s.detail["drifted"][1]["root"], "/w/b");
    }

    #[test]
    fn the_degraded_verdict_rolls_into_the_existing_overall_exit_code() {
        let with = super::super::assess(&inputs(vec![drift(
            "/w/a",
            "Old-owner/widget",
            "New-owner/widget",
        )]));
        let clean = super::super::assess(&inputs(vec![]));
        assert_eq!(with.exit_code(), clean.exit_code(), "no new exit code");
        assert_ne!(with.overall, clean.overall, "but the roll-up degrades");
    }
}
