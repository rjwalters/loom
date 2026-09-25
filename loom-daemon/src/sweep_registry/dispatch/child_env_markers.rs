//! The environment markers a dispatched sweep child is given: the two
//! `Issue`-scoped ones applied (and, for a `PrSet` child, explicitly denied)
//! by [`apply_issue_scoped_markers`], plus — tested here, set in
//! `spawn_process.rs` beside `LOOM_TERMINAL_ID` — the unconditional
//! sweep-identity marker [`SWEEP_ID_ENV`] (#8835).

use super::{SweepKind, LEASE_RENEW_STARTED_ENV};
use std::process::Command;

/// Claim-ownership marker (issue #3823): `dispatch()` flips `loom:issue` ->
/// `loom:building` on the forge BEFORE the child is spawned, for immediate
/// external visibility of the claim.
pub(super) const CLAIM_OWNED_ENV: &str = "LOOM_SWEEP_CLAIM_OWNED";

/// Sweep-identity marker (issue #8835): the dispatched sweep's own id, bare
/// and unprefixed, so anything the sweep spawns can attribute its work back to
/// the sweep that caused it.
///
/// The motivating consumer is 2am's batch submitter, which stamps `sweep_id`
/// onto the `ephemeral_compute` launch record it emits — that is what lets the
/// fleet dashboard nest a live Spot instance under the sweep paying for it
/// instead of listing it in a flat "running compute" pile.
///
/// Unlike the two markers above this is **not** `Issue`-scoped: it is plain
/// identity, every sweep has one, and a `PrSet` sweep can burn batch compute
/// exactly like an `Issue` sweep can. It is therefore set unconditionally by
/// `spawn_process.rs`'s `spawn_child` rather than through
/// [`apply_issue_scoped_markers`], immediately beside `LOOM_TERMINAL_ID`.
///
/// Deliberately a second variable rather than asking consumers to strip the
/// `daemon-` prefix off `LOOM_TERMINAL_ID`: a *terminal* id names the agent
/// slot, and coupling an unrelated cross-repo emitter to that naming
/// convention would break it the next time the convention moves.
pub(crate) const SWEEP_ID_ENV: &str = "LOOM_SWEEP_ID";

/// Apply the `Issue`-scoped child markers to a sweep child's [`Command`].
///
/// # `Issue` dispatch: both markers name the claimed issue
///
/// **`LOOM_SWEEP_CLAIM_OWNED` (#3823).** Without a signal, the child's own
/// `/loom:sweep` pre-flight would read the `loom:building` label this dispatch
/// just applied and skip issue N as "already being built by someone else" —
/// self-skipping the daemon's OWN claim, so no worktree, no build, no PR.
/// Exporting the issue number this sweep owns lets the pre-flight recognise an
/// existing `loom:building` as ITS OWN daemon claim and proceed. Scoped to
/// daemon-dispatched children only: an operator-run `/loom:sweep N` never sets
/// it, so the manual-terminal skip rule (honor any `loom:building`) is
/// unchanged.
///
/// Issue #4111 proved this env var alone insufficient — a daemon-dispatched
/// child reliably reasoned about `loom:building` label timing / PID tables /
/// `loom-daemon status` and self-skipped its own claim without ever consulting
/// it. The `--claim-owned <N>` argv flag is now the PRIMARY signal (positional,
/// in the model's context by construction); this env var is kept for backward
/// compatibility — `spawn-claude.sh` still logs it, and `work_finder.rs` /
/// `ipc.rs` still assert it producer-side.
///
/// **`LOOM_SWEEP_LEASE_RENEW_DISPATCHED` (#7672).** Tells the child's Step 1a
/// that the dispatch which spawned it will start the lease-renewal loop for
/// the one issue it claims, so the session must not start a second one. See
/// [`LEASE_RENEW_STARTED_ENV`] for why this is a marker rather than an
/// unconditional prose withdrawal in `sweep.md`.
///
/// # `PrSet` dispatch: both markers are *removed*, not merely unset
///
/// A `PrSet` child claims no single issue, so neither marker can honestly name
/// one: its `/loom:sweep --prs ...` pre-flight has no per-issue
/// `loom:building` self-claim to recognise (#5342), and it holds no lease to
/// renew (#7672).
///
/// Declining to call [`Command::env`] is not enough to achieve that (#7915).
/// A `Command` inherits the parent's environment by default, and a daemon (or
/// any dispatcher in the spawn chain) that is itself running inside a
/// Loom-dispatched sweep carries both variables set to the issue *it* is
/// building, for its entire lifetime. Without the explicit
/// [`Command::env_remove`], those values leak through to the `PrSet` child,
/// which then reads a claim marker and a renewal-loop hand-off for an issue it
/// has nothing to do with — exactly the lie the `Issue`-only scoping exists to
/// prevent. Removing makes the guarantee hold under inheritance, not merely
/// under a clean parent environment.
pub(crate) fn apply_issue_scoped_markers(cmd: &mut Command, kind: &SweepKind) {
    match kind {
        SweepKind::Issue(issue) => {
            cmd.env(CLAIM_OWNED_ENV, issue.to_string());
            cmd.env(LEASE_RENEW_STARTED_ENV, issue.to_string());
        }
        SweepKind::PrSet(_) => {
            cmd.env_remove(CLAIM_OWNED_ENV);
            cmd.env_remove(LEASE_RENEW_STARTED_ENV);
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::sweep_registry::test_support::{assert_child_wrote, fixture_registry};
    use serial_test::serial;
    use std::time::Duration;
    use tempfile::tempdir;

    /// RAII guard that **sets** both `Issue`-scoped markers in the test
    /// process's own environment, restoring whatever they were (usually
    /// absent) on drop — including across a mid-test assertion panic, since
    /// Rust unwinds through `Drop`.
    ///
    /// The inverse of the `Cleared*Env` guards in `dispatch/tests.rs`: rather
    /// than clearing ambient state so a test does not observe it, this
    /// *manufactures* the ambient state a Loom-dispatched daemon really runs
    /// under, so the `PrSet` arm's `env_remove` can be tested for what it is —
    /// a guarantee that holds under inheritance.
    struct AmbientIssueMarkersEnv {
        saved: Vec<(&'static str, Option<String>)>,
    }

    impl AmbientIssueMarkersEnv {
        fn set(value: &str) -> Self {
            let names = [CLAIM_OWNED_ENV, LEASE_RENEW_STARTED_ENV];
            let saved = names.iter().map(|n| (*n, std::env::var(n).ok())).collect();
            for n in names {
                std::env::set_var(n, value);
            }
            Self { saved }
        }
    }

    impl Drop for AmbientIssueMarkersEnv {
        fn drop(&mut self) {
            for (name, prior) in &self.saved {
                match prior {
                    Some(v) => std::env::set_var(name, v),
                    None => std::env::remove_var(name),
                }
            }
        }
    }

    /// Issue #7915: the `Issue`-only scoping of both markers must survive
    /// **inheritance**, not just a clean daemon environment.
    ///
    /// This is the regression test for the leak: it manufactures the parent
    /// environment a sweep-dispatched daemon has (#7915's own repro used
    /// `LOOM_SWEEP_CLAIM_OWNED=7895` / `LOOM_SWEEP_LEASE_RENEW_DISPATCHED=7895`)
    /// and asserts the `PrSet` child records **both** markers as absent. Before
    /// the `env_remove`, the child inherited the parent's values verbatim —
    /// which also turned `pr_set_dispatch_exports_no_lease_renewal_marker` red
    /// for any agent running this suite from inside a sweep.
    #[test]
    #[serial]
    fn pr_set_dispatch_clears_inherited_issue_markers() {
        let _ambient = AmbientIssueMarkersEnv::set("76727");
        let dir = tempdir().unwrap();
        let (mut registry, record_log) = fixture_registry(dir.path());

        let outcome = registry
            .dispatch(&SweepKind::PrSet(vec![76_728]), None, None, None, None)
            .expect("dispatch should succeed");

        let needle = format!("LOOM_TERMINAL_ID=daemon-{}", outcome.sweep_id);
        let recorded = assert_child_wrote(&record_log, &needle);
        assert!(
            recorded.contains(&format!("{LEASE_RENEW_STARTED_ENV}=unset")),
            "a PrSet child must not inherit the parent sweep's lease-renewal marker \
             (#7915, guarantee from #7672); got: {recorded}"
        );
        assert!(
            recorded.contains(&format!("{CLAIM_OWNED_ENV}=unset")),
            "a PrSet child must not inherit the parent sweep's claim-ownership marker \
             (#7915, scoping from #5342); got: {recorded}"
        );

        let ids: Vec<String> = registry.entries.keys().cloned().collect();
        for id in ids {
            let _ = registry.cancel(&id, Duration::from_millis(50));
        }
    }

    /// The `Issue` arm is unchanged by #7915 — both markers still name the
    /// claimed issue, and an ambient value for a *different* issue is
    /// overwritten rather than passed through.
    #[test]
    #[serial]
    fn issue_dispatch_overrides_inherited_issue_markers() {
        let _ambient = AmbientIssueMarkersEnv::set("76727");
        let dir = tempdir().unwrap();
        let (mut registry, record_log) = fixture_registry(dir.path());

        let outcome = registry
            .dispatch(&SweepKind::Issue(76_729), None, None, None, None)
            .expect("dispatch should succeed");

        let needle = format!("LOOM_TERMINAL_ID=daemon-{}", outcome.sweep_id);
        let recorded = assert_child_wrote(&record_log, &needle);
        assert!(
            recorded.contains(&format!("{CLAIM_OWNED_ENV}=76729"))
                && recorded.contains(&format!("{LEASE_RENEW_STARTED_ENV}=76729")),
            "an Issue child's markers must name the issue IT claims, not the parent's \
             (#7915); got: {recorded}"
        );

        let ids: Vec<String> = registry.entries.keys().cloned().collect();
        for id in ids {
            let _ = registry.cancel(&id, Duration::from_millis(50));
        }
    }

    /// Issue #8835: `spawn_child` exports the sweep's own id as
    /// [`SWEEP_ID_ENV`], alongside the pre-existing
    /// `LOOM_TERMINAL_ID=daemon-<sweep_id>`.
    ///
    /// The value is the **bare** sweep id — no `daemon-` prefix — because the
    /// downstream consumer stamps it verbatim onto an `ephemeral_compute`
    /// launch record that the dashboard joins against `activeSweeps`'
    /// `sweepId`. A prefix here would make every such join miss.
    #[test]
    #[serial]
    fn dispatch_exports_sweep_id_marker() {
        let dir = tempdir().unwrap();
        let (mut registry, record_log) = fixture_registry(dir.path());

        let outcome = registry
            .dispatch(&SweepKind::Issue(8835), None, None, None, None)
            .expect("dispatch should succeed");

        let needle = format!("{SWEEP_ID_ENV}={}", outcome.sweep_id);
        let recorded = assert_child_wrote(&record_log, &needle);
        assert!(
            recorded.contains(&needle),
            "expected {SWEEP_ID_ENV}={} on the child env; got: {recorded}",
            outcome.sweep_id
        );
        // Additive, not a rename: the terminal id keeps its `daemon-` prefix.
        assert!(
            recorded.contains(&format!("LOOM_TERMINAL_ID=daemon-{}", outcome.sweep_id)),
            "{SWEEP_ID_ENV} must not displace LOOM_TERMINAL_ID; got: {recorded}"
        );

        let ids: Vec<String> = registry.entries.keys().cloned().collect();
        for id in ids {
            let _ = registry.cancel(&id, Duration::from_millis(50));
        }
    }

    /// Issue #8835: a `PrSet` dispatch carries [`SWEEP_ID_ENV`] too.
    ///
    /// This is the marker's whole point of difference from the two above: they
    /// are deliberately *cleared* for a `PrSet` run (#7915) because neither can
    /// honestly name an issue such a run claims. A sweep id is not a claim —
    /// every sweep has one, and a Doctor/Judge PR-set sweep can submit batch
    /// compute exactly like an issue sweep can, so unsetting it here would
    /// silently un-attribute that compute.
    #[test]
    #[serial]
    fn pr_set_dispatch_exports_sweep_id_marker() {
        let dir = tempdir().unwrap();
        let (mut registry, record_log) = fixture_registry(dir.path());

        let outcome = registry
            .dispatch(&SweepKind::PrSet(vec![8835]), None, None, None, None)
            .expect("dispatch should succeed");

        let needle = format!("{SWEEP_ID_ENV}={}", outcome.sweep_id);
        let recorded = assert_child_wrote(&record_log, &needle);
        assert!(
            recorded.contains(&needle),
            "expected {SWEEP_ID_ENV}={} on a PrSet child env; got: {recorded}",
            outcome.sweep_id
        );

        let ids: Vec<String> = registry.entries.keys().cloned().collect();
        for id in ids {
            let _ = registry.cancel(&id, Duration::from_millis(50));
        }
    }
}
