//! launchd domain resolution and `launchctl print` probing.
//!
//! Split out of `daemon_install_state.rs` so the over-threshold parent shrinks
//! rather than grows (`.loom/docs/file-size-policy.md`). Adding the
//! LOADED-but-not-running branch (#8086) pushed the parent past its frozen
//! size; this is the sibling-module remedy the policy asks for, not a raised
//! baseline.

use super::{probe_output, PROBE_TIMEOUT};
use std::process::Command;

/// Current uid via `id -u`. Bounded by [`PROBE_TIMEOUT`]; a hung `id` degrades
/// to `None`, exactly like an absent one (#4548).
pub(super) fn current_uid() -> Option<String> {
    let mut cmd = Command::new("id");
    cmd.arg("-u");
    probe_output(cmd, PROBE_TIMEOUT)
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Parse `launchctl print <domain>/<label>` output for a live pid — mirrors
/// the watchdog's `awk -F'= ' '/^[[:space:]]*pid = /{...; print $2; exit}'`.
/// `domain` is an already-resolved launchd domain (see
/// [`resolve_launchd_domain_detailed`]) — the caller resolves it once and
/// reuses it for both this probe and any human-readable detail string,
/// avoiding a duplicate `launchctl`/`id` round trip per [`check_liveness`]
/// call.
///
/// Bounded by [`PROBE_TIMEOUT`]: a `launchctl print` that stalls on XPC
/// degrades to `None` — "no live pid" — exactly like an absent `launchctl`
/// (#4548).
/// What `launchctl print <service>` said.
///
/// `loaded` and `pid` are SEPARATE answers and must stay that way. A job that
/// launchd knows about but is not running prints successfully with no `pid =`
/// line — "LOADED but NOT running" — and that is the ONLY state the #4232
/// bounded auto-remediation gate may act on. Collapsing both into
/// `Option<u32>` (as the first cut of this did) makes it indistinguishable
/// from "launchd has never heard of this job", so the gate never fires, the
/// report says "not loaded/alive", and `last_exit_status` is never even asked
/// for — because it is gated on `job_loaded`.
pub(super) struct LaunchctlProbe {
    /// `launchctl print` exited 0: launchd knows this job.
    pub(super) loaded: bool,
    /// Its `pid = ` line, when it had one.
    pub(super) pid: Option<u32>,
}

pub(super) fn launchctl_probe(domain: &str, label: &str) -> LaunchctlProbe {
    let service = format!("{domain}/{label}");
    let mut cmd = Command::new("launchctl");
    cmd.args(["print", &service]);
    let Some(output) = probe_output(cmd, PROBE_TIMEOUT) else {
        return LaunchctlProbe {
            loaded: false,
            pid: None,
        };
    };
    if !output.status.success() {
        return LaunchctlProbe {
            loaded: false,
            pid: None,
        };
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut pid = None;
    for line in stdout.lines() {
        let trimmed = line.trim_start();
        if let Some(rest) = trimmed.strip_prefix("pid = ") {
            let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
            if let Ok(p) = digits.parse::<u32>() {
                pid = Some(p);
                break;
            }
        }
    }
    LaunchctlProbe { loaded: true, pid }
}

pub(super) fn launchctl_pid(domain: &str, label: &str) -> Option<u32> {
    launchctl_probe(domain, label).pid
}

/// [`resolve_launchd_domain_detailed`]'s result, carrying enough detail for
/// callers to cross-check a negative verdict against the domain that domain
/// resolution *skipped* (#4694).
///
/// The `gui/<uid>` → `user/<uid>` fallback itself is intentional (#4130,
/// headless-SSH support) — this struct does not change which domain is
/// *primary*, it only lets a caller know when a second, skipped domain
/// exists and is worth a cross-check before declaring a negative (dead /
/// not-loaded / not-provisioned) verdict: the single reachability probe used
/// to decide whether `gui/<uid>` is usable cannot distinguish "genuinely
/// unreachable" from "a transient hang/flake within `PROBE_TIMEOUT`" —
/// folding a flaky probe into a permanent domain choice for the rest of the
/// call previously produced false negatives (#4694).
pub(super) struct DomainResolution {
    /// The primary domain to probe. `None` only when the uid itself is
    /// undeterminable.
    pub(super) domain: Option<String>,
    /// The `gui/<uid>` domain that was skipped because its reachability probe
    /// came back non-success, when that is why `domain` is `user/<uid>`.
    /// `Some` only in that exact case — never when an explicit
    /// `LOOM_LAUNCHD_DOMAIN` override was honored (AC6: no cross-check
    /// fallback for an explicit override) and never when `gui/<uid>` was
    /// itself the resolved domain (nothing was skipped).
    pub(super) fallback_check_domain: Option<String>,
}

/// Resolve the launchd domain to probe in, mirroring
/// `lib/launchd-domain.sh::resolve_launchd_domain`: an explicit
/// `LOOM_LAUNCHD_DOMAIN` wins, else `gui/<uid>` when that domain resolves, else
/// the SSH-reachable background `user/<uid>` domain. `None` only when the uid
/// itself is undeterminable (⇒ the caller degrades to `Unknown`). The result
/// also carries the skipped-domain detail described in [`DomainResolution`],
/// for callers that need to cross-check a negative verdict (#4694) — every
/// caller in this module uses that detail, so there is no separate
/// domain-only accessor.
/// The launchd domain to probe in: the explicit override, else `gui/<uid>`
/// when that domain resolves, else `user/<uid>`.
///
/// `pub(crate)` for the watchdog, which needs the same domain the liveness
/// probe used so its report and its `launchctl print` name the same service
/// (#4536 — resolving it twice is how the two came to disagree).
pub(crate) fn launchd_domain(override_value: Option<&str>) -> String {
    resolve_launchd_domain_detailed(override_value)
        .domain
        .unwrap_or_default()
}

pub(super) fn resolve_launchd_domain_detailed(override_value: Option<&str>) -> DomainResolution {
    if let Some(explicit) = override_value.filter(|s| !s.is_empty()) {
        return DomainResolution {
            domain: Some(explicit.to_string()),
            fallback_check_domain: None,
        };
    }
    let Some(uid) = current_uid() else {
        return DomainResolution {
            domain: None,
            fallback_check_domain: None,
        };
    };
    let gui = format!("gui/{uid}");
    let mut cmd = Command::new("launchctl");
    cmd.args(["print", &gui]);
    // A hung reachability probe reads as "gui/<uid> not reachable" — the same
    // verdict an absent/nonzero `launchctl` gives — so the caller falls back to
    // the SSH-reachable `user/<uid>` domain rather than blocking (#4548). That
    // failure is exactly the ambiguous case #4694 cares about: it may be a
    // genuine absence, or it may be a transient flake — either way `gui` is
    // reported back as the domain worth cross-checking before a caller trusts
    // a negative verdict from `user/<uid>` alone.
    let gui_ok = probe_output(cmd, PROBE_TIMEOUT).is_some_and(|o| o.status.success());
    if gui_ok {
        return DomainResolution {
            domain: Some(gui),
            fallback_check_domain: None,
        };
    }
    DomainResolution {
        domain: Some(format!("user/{uid}")),
        fallback_check_domain: Some(gui),
    }
}

/// Probe whether the launchd job `<domain>/<label>` is loaded —
/// `launchctl print <domain>/<label>` exits 0 only for a bootstrapped job, so
/// a nonzero exit is a real (for *this domain*) "not loaded". A
/// missing/unspawnable `launchctl` yields `None` — unknown, never a false
/// negative. A probe that hangs past [`PROBE_TIMEOUT`] takes that same `None`
/// path (#4548). Shared by [`launchctl_job_provisioned`]'s primary and
/// cross-check probes (#4694) — both are this exact same call against
/// different domains.
fn probe_domain_provisioned(domain: &str, label: &str) -> Option<bool> {
    let mut cmd = Command::new("launchctl");
    cmd.args(["print", &format!("{domain}/{label}")]);
    let output = probe_output(cmd, PROBE_TIMEOUT)?;
    Some(output.status.success())
}

/// Is the watchdog launchd job loaded? A missing/unspawnable `launchctl` (or
/// an undeterminable domain) yields `None` — unknown, never a false negative.
///
/// #4694: a negative (or unknown) primary-domain verdict is not trusted on
/// its own when [`resolve_launchd_domain_detailed`] reports a
/// `fallback_check_domain` — i.e. when domain resolution fell back to
/// `user/<uid>` because its `gui/<uid>` reachability probe failed, which
/// cannot distinguish a genuine absence from a transient flake. In that case
/// this also probes the skipped `gui/<uid>` domain and only reports
/// not-provisioned (`Some(false)`) when BOTH domains agree; any timeout/error
/// on either probe, or a disagreement other than "either domain says loaded",
/// degrades to `None` rather than a confident negative (never folds "unknown"
/// into `Some(false)`). No cross-check occurs when an explicit
/// `LOOM_LAUNCHD_DOMAIN` override was honored (AC6) — that always resolves
/// with `fallback_check_domain: None`.
pub(super) fn launchctl_job_provisioned(
    label: &str,
    domain_override: Option<&str>,
) -> Option<bool> {
    let resolution = resolve_launchd_domain_detailed(domain_override);
    let domain = resolution.domain?;
    let primary = probe_domain_provisioned(&domain, label);
    if primary == Some(true) {
        return primary;
    }
    let Some(check_domain) = resolution.fallback_check_domain.as_deref() else {
        return primary;
    };
    let secondary = probe_domain_provisioned(check_domain, label);
    match (primary, secondary) {
        (_, Some(true)) => Some(true),
        (Some(false), Some(false)) => Some(false),
        _ => None,
    }
}
