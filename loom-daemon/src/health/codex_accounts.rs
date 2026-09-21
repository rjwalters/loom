//! `loom-daemon health`'s Codex account section (issue #8407 AC5) — the
//! provider-scoped sibling of the Claude `tokens` section.
//!
//! Conditional, exactly like [`super::assess_observability`] and the codesign
//! section: a host with **no** Codex accounts renders no section at all, so
//! every Claude-only host's report is byte-identical to its pre-#8407 form.
//!
//! # What it reports, and what it deliberately does not
//!
//! The headline is the same question the `tokens` section answers for Claude —
//! how many accounts can take work — plus the per-status breakdown the
//! availability probe produces (`available` / `rate_limited` / `exhausted` /
//! `blocked` / `skipped`).
//!
//! Ranking freshness is reported as **detail only**, never as a verdict input.
//! `.ranking` staleness is a legitimate Claude-side alarm because the daemon
//! refreshes that file on a cadence, so a stale one means the refresher
//! stopped. Nothing yet refreshes the Codex ranking on a cadence, so alarming
//! on its absence would paint every codex host amber for not running a command
//! by hand. When a refresh cadence lands, this is the one line to change.
//!
//! No key material can reach this section: every field is a count, an age, or
//! a status word — the same `strings`-clean contract
//! [`crate::tokens_pool::codex_check`] holds for the probe itself.

use std::collections::BTreeMap;
use std::path::PathBuf;

use super::{HealthInputs, HealthSection, Verdict};
use crate::tokens_pool::ProviderCapacity;

/// The collector's Codex reading, gathered filesystem-only (no IPC).
#[derive(Debug, Clone, PartialEq)]
pub struct CodexAccountsSnapshot {
    /// The workspace whose account registry and health state were read.
    pub workspace: PathBuf,
    /// Raw/enabled/healthy/cooldown/reauth-required counts.
    pub capacity: ProviderCapacity,
    /// `status -> count` from a live (read-only) availability pass.
    pub statuses: BTreeMap<String, usize>,
    /// Whether `accounts check --ranking` has ever written this workspace's
    /// provider-namespaced ranking file, and how old it is.
    pub ranking_present: bool,
    pub ranking_age_secs: Option<u64>,
}

/// Render the section, or `None` when this host has no Codex accounts.
#[must_use]
pub fn assess(inputs: &HealthInputs) -> Option<HealthSection> {
    let snapshot = inputs.codex_accounts.as_ref()?;
    let capacity = &snapshot.capacity;
    if capacity.raw == 0 {
        return None;
    }

    let mut issues: Vec<String> = Vec::new();
    if capacity.enabled == 0 {
        issues.push("every codex account is DISABLED".to_string());
    } else if capacity.healthy == 0 {
        issues.push(
            "ZERO healthy codex accounts — codex-pinned work cannot be dispatched here".to_string(),
        );
    }
    if capacity.reauth_required > 0 {
        issues.push(format!(
            "{} awaiting re-auth (`loom-daemon accounts status codex <name>`)",
            capacity.reauth_required
        ));
    }

    let breakdown = if snapshot.statuses.is_empty() {
        String::new()
    } else {
        let rendered = snapshot
            .statuses
            .iter()
            .map(|(status, count)| format!("{count} {status}"))
            .collect::<Vec<_>>()
            .join(", ");
        format!(" [{rendered}]")
    };
    let ranking = match (snapshot.ranking_present, snapshot.ranking_age_secs) {
        (true, Some(age)) => {
            format!("ranking {} old", super::format_age(age.try_into().unwrap_or(i64::MAX)))
        }
        (true, None) => "ranking present (age unknown)".to_string(),
        (false, _) => "no ranking yet (`loom-daemon accounts check --ranking`)".to_string(),
    };
    let base = format!(
        "{}/{} healthy{breakdown} ({} cooling down), {ranking}",
        capacity.healthy, capacity.enabled, capacity.cooldown
    );

    let (verdict, summary) = if issues.is_empty() {
        (Verdict::Green, base)
    } else {
        (Verdict::Degraded, format!("{base}; {}", issues.join("; ")))
    };
    Some(HealthSection {
        key: "codex",
        verdict,
        summary,
        detail: serde_json::json!({
            "workspace": snapshot.workspace.display().to_string(),
            "raw": capacity.raw,
            "enabled": capacity.enabled,
            "healthy": capacity.healthy,
            "cooldown": capacity.cooldown,
            "reauth_required": capacity.reauth_required,
            "statuses": snapshot.statuses,
            "ranking_present": snapshot.ranking_present,
            "ranking_age_secs": snapshot.ranking_age_secs,
            "issues": issues,
        }),
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::tokens_pool::AccountProvider;

    fn snapshot(healthy: usize, enabled: usize, reauth: usize) -> CodexAccountsSnapshot {
        CodexAccountsSnapshot {
            workspace: PathBuf::from("/repo"),
            capacity: ProviderCapacity {
                provider: AccountProvider::Codex,
                raw: enabled,
                enabled,
                healthy,
                cooldown: enabled.saturating_sub(healthy + reauth),
                reauth_required: reauth,
                healthy_by_class: BTreeMap::new(),
                observed_at: 1_000,
            },
            statuses: BTreeMap::from([("available".to_string(), healthy)]),
            ranking_present: true,
            ranking_age_secs: Some(60),
        }
    }

    fn inputs_with(snapshot: Option<CodexAccountsSnapshot>) -> HealthInputs {
        let mut inputs = crate::health::tests::healthy_inputs();
        inputs.codex_accounts = snapshot;
        inputs
    }

    #[test]
    fn a_host_with_no_codex_accounts_renders_no_section() {
        assert!(assess(&inputs_with(None)).is_none());
        let mut empty = snapshot(0, 0, 0);
        empty.capacity.raw = 0;
        assert!(assess(&inputs_with(Some(empty))).is_none());
    }

    #[test]
    fn a_healthy_pool_is_green_and_names_its_breakdown() {
        let section = assess(&inputs_with(Some(snapshot(2, 2, 0)))).unwrap();
        assert_eq!(section.verdict, Verdict::Green);
        assert!(section.summary.starts_with("2/2 healthy [2 available]"), "{}", section.summary);
        assert_eq!(section.detail["healthy"], 2);
    }

    #[test]
    fn a_starved_pool_is_degraded() {
        let section = assess(&inputs_with(Some(snapshot(0, 2, 0)))).unwrap();
        assert_eq!(section.verdict, Verdict::Degraded);
        assert!(section.summary.contains("ZERO healthy codex accounts"));
    }

    #[test]
    fn a_reauth_hold_is_called_out() {
        let section = assess(&inputs_with(Some(snapshot(1, 2, 1)))).unwrap();
        assert_eq!(section.verdict, Verdict::Degraded);
        assert!(section.summary.contains("awaiting re-auth"));
    }

    #[test]
    fn a_missing_ranking_is_reported_without_changing_the_verdict() {
        let mut snap = snapshot(2, 2, 0);
        snap.ranking_present = false;
        snap.ranking_age_secs = None;
        let section = assess(&inputs_with(Some(snap))).unwrap();
        assert_eq!(section.verdict, Verdict::Green);
        assert!(section.summary.contains("no ranking yet"));
    }
}
