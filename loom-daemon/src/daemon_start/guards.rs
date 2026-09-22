//! The two #6568 defences: the agent-session refusal and the scratch-workdir
//! advisory.
//!
//! Incident 2026-08-17: a daemon-dispatched sweep exercising the start path out
//! of a `/tmp` checkout replaced the REAL production LaunchAgent on both
//! operator Macs with its own per-invocation environment, and it ran that way
//! for two days. Two independent defences, because either alone still
//! reproduces part of it:
//!
//! * the **strip** — [`super::envh::is_session_scoped_env_key`], applied at
//!   every render *and* at the carry-forward merge, so an already-poisoned
//!   installed file cannot re-inject them either;
//! * the **refusal** — [`guard_session_context_start`], which refuses a real
//!   start that would write the DEFAULT production identity from a shell
//!   carrying agent-session context, without requiring the caller to remember
//!   `LOOM_LAUNCHD_LABEL`.

use std::path::Path;

use super::envh::env_text;
use super::out;
use super::platform;

/// `session_context_keys()` — the agent-session keys THIS shell exports, space
/// separated, sorted and deduplicated.
///
/// The detection set is narrower than the strip set: `LOOM_RUNTIME` is stripped
/// but is deliberately NOT a detection signal, because it is a plausible thing
/// for an operator to export as a personal default and a refusal keyed on it
/// would block legitimate `loom start` runs.
#[must_use]
pub fn session_context_keys() -> String {
    session_context_keys_from(&env_text())
}

/// [`session_context_keys`] over injected `env` text.
#[must_use]
pub fn session_context_keys_from(text: &str) -> String {
    let mut keys: Vec<String> = text
        .split('\n')
        .filter(|l| !l.is_empty())
        .filter(|l| detection_line(l))
        .filter_map(|l| l.find('=').map(|i| l[..i].to_string()))
        .collect();
    // `sort -u`. Byte order rather than the locale's collation: every key this
    // can produce is ASCII upper-case and underscore, an alphabet in which the
    // two agree.
    keys.sort_unstable();
    keys.dedup();
    keys.join(" ")
}

/// `^(LOOM_SWEEP_[A-Za-z0-9_]*|LOOM_TERMINAL_ID|LOOM_ROLE)=`.
fn detection_line(line: &str) -> bool {
    if let Some(rest) = line.strip_prefix("LOOM_SWEEP_") {
        let stop = rest
            .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
            .unwrap_or(rest.len());
        return rest[stop..].starts_with('=');
    }
    line.starts_with("LOOM_TERMINAL_ID=") || line.starts_with("LOOM_ROLE=")
}

/// What [`guard_session_context_start`] decided.
pub enum SessionGuard {
    /// Nothing to report.
    Clear,
    /// Warned but proceeding (an inspection mode, or an explicit
    /// acknowledgement).
    Warned,
    /// A real start that must exit 1.
    Refuse,
}

/// `guard_session_context_start()`.
///
/// Modelled exactly on `warn_autonomy_downgrade`: a real start refuses,
/// `--print-plist` / `--print-unit` warn only. Refusing a read-only preview
/// would make it impossible to see what a real start would render.
///
/// Scope is the two tiers that write a durable supervisor definition under a
/// well-known identity. The nohup fallback renders nothing, so it has no
/// durable config to poison and is left byte-for-byte unchanged.
#[must_use]
pub fn guard_session_context_start(
    print_plist: bool,
    print_unit: bool,
    use_launchd: bool,
    is_linux_systemd: bool,
    argv0: &str,
) -> SessionGuard {
    let session_keys = session_context_keys();
    if session_keys.is_empty() {
        return SessionGuard::Clear;
    }

    // Which mechanism would this invocation write? The inspection modes decide
    // from argv alone; a real start uses whatever platform detection picked.
    let mech = if print_plist {
        "launchd"
    } else if print_unit {
        "systemd"
    } else if use_launchd {
        "launchd"
    } else if is_linux_systemd {
        "systemd"
    } else {
        return SessionGuard::Clear;
    };

    let (identity_hint, override_hint) = if mech == "launchd" {
        if std::env::var("LOOM_LAUNCHD_LABEL").is_ok_and(|v| !v.is_empty()) {
            return SessionGuard::Clear;
        }
        (
            format!(
                "the production LaunchAgent label {} in the real launchd domain",
                platform::launchd_label()
            ),
            "LOOM_LAUNCHD_LABEL=com.example.loom-daemon-test".to_string(),
        )
    } else {
        if std::env::var("LOOM_SYSTEMD_UNIT").is_ok_and(|v| !v.is_empty()) {
            return SessionGuard::Clear;
        }
        (
            format!(
                "the production systemd --user unit {}",
                std::env::var("LOOM_SYSTEMD_UNIT")
                    .unwrap_or_else(|_| "loom-daemon.service".to_string())
            ),
            "LOOM_SYSTEMD_UNIT=loom-daemon-test.service".to_string(),
        )
    };

    out::warn("");
    out::warn(&format!(
        "WARNING: agent-session context detected -- this shell exports: {session_keys}"
    ));
    out::warn(&format!("  A start from here would write {identity_hint}"));
    out::warn("  from a per-invocation agent environment (workspace, log paths and autonomy");
    out::warn("  knobs scoped to ONE sweep). That is incident 2026-08-17: both operator Macs'");
    out::warn("  production daemons ran for two days under a sweep's test configuration.");

    if platform::env_says_on("LOOM_ALLOW_SESSION_DAEMON_START") {
        out::warn("  Proceeding anyway: LOOM_ALLOW_SESSION_DAEMON_START is set (explicit operator acknowledgement).");
        return SessionGuard::Warned;
    }

    if print_plist || print_unit {
        out::warn(
            "  This is a read-only preview, so it is NOT refused -- but a REAL start with this",
        );
        out::warn("  environment would be. See the remediation below.");
        out::warn(&format!("  Remediation: scope the identity ({override_hint}),"));
        out::warn("  drop the session vars (env -u LOOM_ROLE -u LOOM_TERMINAL_ID -u LOOM_SWEEP_CLAIM_OWNED ...),");
        out::warn("  or set LOOM_ALLOW_SESSION_DAEMON_START=1 to acknowledge a deliberate production start.");
        return SessionGuard::Warned;
    }

    out::err("");
    out::err("ERROR: refusing to start -- this would overwrite the REAL daemon configuration with");
    out::err("an agent session's environment (see the WARNING above). Choose one:");
    out::err("  * Exercising/testing the start path? Scope the supervisor identity:");
    out::err(&format!("      {override_hint}"));
    out::err("  * Genuinely starting the production daemon from inside an agent session?");
    out::err(&format!("      LOOM_ALLOW_SESSION_DAEMON_START=1 {argv0} ..."));
    out::err("  * Or run it from a clean shell:");
    out::err(&format!(
        "      env -u LOOM_ROLE -u LOOM_TERMINAL_ID -u LOOM_SWEEP_CLAIM_OWNED {argv0} ..."
    ));
    out::err("(#6568 -- the session-scoped keys themselves are stripped from every rendered");
    out::err("plist/unit regardless; this refusal additionally protects the production identity.)");
    SessionGuard::Refuse
}

/// `is_scratch_style_path()`.
///
/// `$TMPDIR` is compared with its trailing slash stripped, then the fixed
/// prefixes, then the two suffix shapes. A `*-checkout` match is a *glob*, so
/// it also matches a bare `-checkout` with nothing before it.
#[must_use]
pub fn is_scratch_style_path(p: &str) -> bool {
    if p.is_empty() {
        return false;
    }
    let tmpdir = std::env::var("TMPDIR").unwrap_or_default();
    let tmpdir = tmpdir.strip_suffix('/').unwrap_or(&tmpdir).to_string();
    if !tmpdir.is_empty() && p.starts_with(&format!("{tmpdir}/")) {
        return true;
    }
    for prefix in [
        "/tmp/",
        "/private/tmp/",
        "/var/tmp/",
        "/var/folders/",
        "/private/var/folders/",
    ] {
        if p.starts_with(prefix) {
            return true;
        }
    }
    if p.ends_with("-checkout") || p.contains("-checkout/") {
        return true;
    }
    p.contains("/.loom/worktrees/")
}

/// `warn_scratch_workdir_drift()` — advisory only; it never blocks a start.
///
/// A scratch-rooted daemon is exactly what this repo's hermetic suites
/// deliberately run, so a refusal here would be wrong. The point is that an
/// operator reading start output sees it immediately rather than two days later.
pub fn warn_scratch_workdir_drift(repo_root: &Path) {
    let mut offenders: Vec<String> = Vec::new();
    let repo_root_s = repo_root.display().to_string();
    if is_scratch_style_path(&repo_root_s) {
        offenders.push(format!("WorkingDirectory={repo_root_s}"));
    }
    let workspace = std::env::var("LOOM_WORKSPACE").unwrap_or_default();
    if !workspace.is_empty() && workspace != repo_root_s && is_scratch_style_path(&workspace) {
        offenders.push(format!("LOOM_WORKSPACE={workspace}"));
    }
    if offenders.is_empty() {
        return;
    }
    out::warn("");
    out::warn("WARNING: this daemon would run out of a SCRATCH / temporary directory (#6568):");
    for o in &offenders {
        out::warn(&format!("  - {o}"));
    }
    out::warn(
        "  A $TMPDIR / *-checkout / worktree root is not durable daemon config: its contents",
    );
    out::warn("  (and any watchdog, socket or pid path under it) vanish on reboot or cleanup, and");
    out::warn(
        "  the daemon keeps reporting healthy the whole time. If this is a test fixture this",
    );
    out::warn(
        "  is expected; if this is a real host, the daemon is misconfigured -- restart it from",
    );
    out::warn("  the machine checkout (see .loom/docs/troubleshooting.md).");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loom_runtime_is_stripped_but_never_detected() {
        // The asymmetry is deliberate and is the difference between blocking a
        // poisoning and blocking an operator.
        assert!(super::super::envh::is_session_scoped_env_key("LOOM_RUNTIME"));
        assert_eq!(session_context_keys_from("LOOM_RUNTIME=claude\n"), "");
    }

    #[test]
    fn detection_keys_are_sorted_and_deduplicated() {
        let text =
            "LOOM_ROLE=a\nLOOM_SWEEP_NICED=1\nLOOM_TERMINAL_ID=t\nLOOM_SWEEP_CLAIM_OWNED=9\n";
        assert_eq!(
            session_context_keys_from(text),
            "LOOM_ROLE LOOM_SWEEP_CLAIM_OWNED LOOM_SWEEP_NICED LOOM_TERMINAL_ID"
        );
    }

    #[test]
    fn a_bare_loom_sweep_prefix_still_counts() {
        assert_eq!(session_context_keys_from("LOOM_SWEEP_=1\n"), "LOOM_SWEEP_");
        assert_eq!(session_context_keys_from("LOOM_SWEEPX=1\n"), "");
    }

    #[test]
    fn scratch_detection_covers_the_shapes_the_incident_produced() {
        assert!(is_scratch_style_path("/tmp/pr6416-checkout"));
        assert!(is_scratch_style_path("/private/var/folders/x/y"));
        assert!(is_scratch_style_path("/home/u/repo/.loom/worktrees/issue-1"));
        assert!(is_scratch_style_path("/opt/thing-checkout"));
        assert!(is_scratch_style_path("/opt/thing-checkout/sub"));
        assert!(!is_scratch_style_path("/home/u/repo"));
        assert!(!is_scratch_style_path(""));
    }
}
