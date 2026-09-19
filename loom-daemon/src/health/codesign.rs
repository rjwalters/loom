//! The conditional `codesign_identity` health section (Issue #7605) and the
//! preflight fact it reads (#8286).
//!
//! Split into its own file because `health.rs` sits at its
//! `.loom/docs/file-size-policy.md` ratchet: assessment logic that grows goes
//! in a sibling module and the parent keeps only the `mod` line plus the
//! re-export, exactly as [`super::busy`] already does.

use serde::Serialize;

use super::{HealthInputs, HealthSection, Verdict};

/// The outcome of a one-time, client-side, non-interactive preflight of a
/// configured `codesign.identity` (Issue #7605). Computed entirely in the
/// `loom-daemon health` CLI collector (`cli/health.rs`) — there is no
/// daemon-IPC involvement, so this is threaded into [`HealthInputs`] exactly
/// like [`HealthInputs::self_update`] and [`HealthInputs::gh_unavailable`]:
/// the assessment logic here just reads the already-collected fact.
///
/// `None` in [`HealthInputs::codesign_preflight`] means "nothing to report" —
/// covering every one of: non-Darwin host, no identity configured
/// (`LOOM_CODESIGN_IDENTITY` unset and no `codesign.identity` in the
/// resolved config), or `codesign`/`security` themselves unusable in this
/// process. All of those are exactly the conditions under which
/// `sign_daemon_binary` (`scripts/install/provision-daemon.sh`) silently
/// falls back to ad-hoc signing without complaint — this section exists only
/// to surface the ONE case that fallback masks: an identity that is
/// configured, present in the keychain, but cannot sign non-interactively
/// (almost always because its private key is missing `codesign` from its
/// keychain access control list, which raises a blocking SecurityAgent GUI
/// prompt instead of failing outright — the exact 10+ minute unattended-hang
/// this issue reports).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CodesignPreflightResult {
    /// The configured identity name that was preflighted.
    pub identity: String,
    /// Whether it signed a throwaway copy non-interactively within the cap.
    pub ok: bool,
    /// Human-readable detail: why the preflight failed (timeout / not found
    /// in keychain / codesign exit status), or empty when `ok`.
    pub detail: String,
}

/// Assess the codesign-identity preflight: `Some(DEGRADED)` only when a
/// configured identity actually failed the preflight, else `None` —
/// anomaly-only, the same shape as [`super::assess_observability`], since
/// there is nothing worth a permanent GREEN line for "no identity is
/// configured" (the overwhelmingly common case: ad-hoc signing, unconfigured
/// on purpose).
///
/// # Where this preflight actually ran (Issue #8286)
///
/// The probe behind [`HealthInputs::codesign_preflight`] runs entirely inside
/// **this `health` invocation's own process** (`probe_codesign_identity_preflight`
/// in `cli/health.rs`) — it never asks the long-running daemon (typically
/// `launchd`-supervised, in a logged-in GUI session with an unlocked login
/// keychain) what *it* would get signing with. Over a non-interactive ssh
/// session the login keychain routinely refuses non-interactive `codesign`
/// access outright (`errSecInternalComponent`/"User interaction is not
/// allowed"), so this section reports DEGRADED **every single time it is run
/// that way — independent of whether the identity actually works, or whether
/// the daemon itself can sign fine**. The message below says so explicitly so
/// a failing preflight over ssh does not get misread as "the identity fix
/// didn't take" (the exact misreading in example-org/tool-repo#202, which
/// cost three verification rounds before anyone caught it): the message
/// always names this invocation's own context and points at re-running from
/// an interactive/tty session as the actual check.
#[must_use]
pub fn assess_codesign_identity(inputs: &HealthInputs) -> Option<HealthSection> {
    let probe = inputs.codesign_preflight.as_ref()?;
    if probe.ok {
        return None;
    }
    Some(HealthSection::new(
        "codesign_identity",
        Verdict::Degraded,
        format!(
            "configured codesign identity '{}' fails a non-interactive preflight evaluated in \
             THIS `health` invocation's own process context ({}) — that context is NOT the \
             daemon's: over a non-interactive ssh session the login keychain routinely refuses \
             access here even when the identity is fine and the daemon itself (e.g. under \
             launchd, in an unlocked GUI-session keychain) signs without issue, so before \
             concluding the identity itself is broken, re-run this check from an \
             interactive/tty session. sign_daemon_binary falls back to ad-hoc signing rather \
             than hanging either way, but the identity should still be repaired: see \
             'Repairing an identity imported without codesign access' in \
             defaults/docs/macos-tcc-codesign.md",
            probe.identity, probe.detail
        ),
        serde_json::json!({
            "identity": probe.identity,
            "detail": probe.detail,
        }),
    ))
}
