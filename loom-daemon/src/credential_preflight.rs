//! Startup forge-credential preflight (#4005).
//!
//! # Problem
//!
//! Every daemon-issued `gh` call (`claim_reconciliation`, `main_health_gate`,
//! `metrics_collector`, the work finder, …) is a plain `Command::new("gh")`
//! with no `env_clear` — so it authenticates however `gh` itself resolves
//! credentials in the daemon's process environment: `GH_TOKEN` /
//! `GITHUB_TOKEN`, falling back to `gh`'s own credential store (the macOS
//! login keychain, or `~/.config/gh/hosts.yml` on Linux). That plumbing
//! already works headlessly *if* a token happens to be exported before
//! `loom-daemon-start.sh` runs — the actual defect is that nothing checks at
//! startup, so a daemon started with neither an exported token nor an
//! unlocked keychain (e.g. over SSH, clean environment) boots clean and then
//! 401s on every forge call for the life of the process, with nothing but
//! silent per-tick noise to show for it.
//!
//! # What this module does
//!
//! [`run`] resolves the credential state **once**, at daemon startup —
//! immediately before the claim-reconciliation startup pass
//! (`main.rs`, the daemon's first `gh` consumer) — and reports the outcome:
//!
//! - `info!` on success, naming which mechanism won and a non-secret
//!   fingerprint (last 4 chars of an env token, or the authenticated login).
//! - `error!` on failure, naming both remedies (export `GH_TOKEN` before
//!   starting the daemon, or unlock the login keychain from a GUI session).
//!
//! The result is also carried in [`crate::types::DaemonStatusReport`] so an
//! operator can see it via `loom-daemon status` without reading logs.
//!
//! # Non-goals
//!
//! - **No new credential store.** This reads the credential `gh` would
//!   already use (env vars it inherits, or its own store) — it never
//!   provisions, writes, or manages a loom-specific PAT file. The existing
//!   plist env-forwarding mechanism (`loom-daemon-start.sh`) already reaches
//!   the daemon and every dispatched sweep child; a second secret-at-rest
//!   surface would add attack surface for no capability gain.
//! - **GitHub only.** The daemon's own forge calls all shell out to `gh`,
//!   which only ever resolves GitHub credentials. `GITEA_TOKEN`/
//!   `FORGE_TOKEN` forwarding exists solely for dispatched sweep children
//!   targeting a Gitea-backed repo — the daemon process itself never calls a
//!   Gitea API, so there is nothing to preflight for it here. See
//!   `.loom/docs/github-authentication.md` § "Headless and SSH-only daemon operation".
//! - **Never blocks or hangs.** Bounded by [`PREFLIGHT_TIMEOUT`] via the
//!   reused [`crate::main_health_gate::run_capture_with_timeout`] helper — an
//!   unlocked-keychain prompt or a hung `gh` is exactly the failure mode this
//!   preflight must survive, not itself trigger.

use std::path::PathBuf;
use std::time::Duration;

use chrono::Utc;

use crate::main_health_gate::run_capture_with_timeout;
use crate::types::CredentialPreflightReport;

/// Bound on the `gh auth status` probe. `gh auth status --json` documents an
/// exit code of `0` regardless of authentication outcome (state is carried in
/// the JSON body), so a non-zero exit or a hang here means the probe itself
/// could not complete — never treated as "not authenticated".
pub const PREFLIGHT_TIMEOUT: Duration = Duration::from_secs(10);

/// Injectable probe seam so the resolution logic is unit-testable without a
/// real `gh` binary — mirrors [`crate::main_health_gate::ForgeCiStatus`].
pub trait GhAuthProbe {
    /// Returns raw `gh auth status --json hosts` stdout, or a non-secret
    /// error string when the probe itself could not complete.
    fn probe(&self) -> Result<String, String>;
}

/// The concrete probe: `gh auth status --json hosts`, bounded by
/// [`PREFLIGHT_TIMEOUT`] via the reused bounded-subprocess helper.
pub struct RealGhAuthProbe {
    /// The `gh` binary to invoke — plain `"gh"` in production, overridable in
    /// tests to point at a stub script on `PATH`.
    pub gh_bin: String,
    /// Working directory for the subprocess. `gh auth status` is host-level
    /// (not repo-scoped), so any existing directory works; the daemon passes
    /// its primary workspace root for consistency with its other `gh` calls.
    pub cwd: PathBuf,
}

impl GhAuthProbe for RealGhAuthProbe {
    fn probe(&self) -> Result<String, String> {
        run_capture_with_timeout(
            &self.gh_bin,
            &["auth", "status", "--json", "hosts"],
            &self.cwd,
            PREFLIGHT_TIMEOUT,
        )
    }
}

/// Non-secret fingerprint for an env-sourced token: the last 4 characters.
/// Tokens shorter than 4 characters (should not occur in practice) fingerprint
/// as `"***"` rather than echoing the whole value.
fn env_fingerprint(value: &str) -> String {
    let tail: String = value
        .chars()
        .rev()
        .take(4)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    if tail.chars().count() < 4 {
        "***".to_string()
    } else {
        tail
    }
}

/// Pure reducer: given the daemon's own `GH_TOKEN`/`GITHUB_TOKEN` env lookups
/// and the raw probe outcome, decide the report. Split out from I/O so it is
/// unit-testable without a real `gh` binary or process environment — mirrors
/// `main_health_gate::parse_gh_run_list`.
fn resolve(
    gh_token_env: Option<&str>,
    github_token_env: Option<&str>,
    probe: Result<String, String>,
) -> CredentialPreflightReport {
    let checked_at = Utc::now();
    let stdout = match probe {
        Ok(stdout) => stdout,
        Err(e) => {
            return CredentialPreflightReport {
                ok: false,
                mechanism: "unknown".to_string(),
                fingerprint: None,
                message: format!(
                    "could not determine gh authentication state ({e}) — forge calls may fail \
                     silently. Export GH_TOKEN before starting the daemon (see \
                     loom-daemon-start.sh), or unlock the login keychain from a GUI session."
                ),
                checked_at,
            };
        }
    };

    let parsed: serde_json::Value = match serde_json::from_str(&stdout) {
        Ok(v) => v,
        Err(e) => {
            return CredentialPreflightReport {
                ok: false,
                mechanism: "unknown".to_string(),
                fingerprint: None,
                message: format!("could not parse `gh auth status --json hosts` output ({e})"),
                checked_at,
            };
        }
    };

    // The active entry per host is the one `gh` will actually use — `gh auth
    // status --json hosts` returns every known account per host, with
    // exactly one `active: true` entry per host that has one.
    let active = parsed
        .get("hosts")
        .and_then(serde_json::Value::as_object)
        .into_iter()
        .flat_map(|hosts| hosts.values())
        .filter_map(serde_json::Value::as_array)
        .flatten()
        .find(|entry| entry.get("active").and_then(serde_json::Value::as_bool) == Some(true));

    let Some(active) = active else {
        return CredentialPreflightReport {
            ok: false,
            mechanism: "none".to_string(),
            fingerprint: None,
            message: "no usable gh credential found — not logged into any GitHub host. Export \
                      GH_TOKEN before starting the daemon (see loom-daemon-start.sh), or run \
                      `gh auth login` / unlock the login keychain from a GUI session."
                .to_string(),
            checked_at,
        };
    };

    let token_source = active
        .get("tokenSource")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("unknown");
    let state = active
        .get("state")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("unknown");
    let login = active
        .get("login")
        .and_then(serde_json::Value::as_str)
        .filter(|s| !s.is_empty());

    if state != "success" {
        return CredentialPreflightReport {
            ok: false,
            mechanism: token_source.to_string(),
            fingerprint: None,
            message: format!(
                "gh reports the active credential ({token_source}) as unusable — forge calls \
                 will 401. Export a valid GH_TOKEN before starting the daemon, or unlock the \
                 login keychain from a GUI session."
            ),
            checked_at,
        };
    }

    let fingerprint = match token_source {
        "GH_TOKEN" => gh_token_env.map(env_fingerprint),
        "GITHUB_TOKEN" => github_token_env.map(env_fingerprint),
        _ => login.map(str::to_string),
    };

    CredentialPreflightReport {
        ok: true,
        mechanism: token_source.to_string(),
        fingerprint,
        message: match login {
            Some(l) => format!("authenticated via {token_source} (account {l})"),
            None => format!("authenticated via {token_source}"),
        },
        checked_at,
    }
}

/// Run the startup preflight and log the outcome (#4005 AC1/AC2): `info!` on
/// success naming the mechanism + fingerprint, `error!` on failure naming
/// both remedies. Never panics; bounded by [`PREFLIGHT_TIMEOUT`] (via
/// `probe`), so it cannot hang daemon startup.
#[must_use]
pub fn run(probe: &dyn GhAuthProbe) -> CredentialPreflightReport {
    let gh_token = std::env::var("GH_TOKEN").ok().filter(|s| !s.is_empty());
    let github_token = std::env::var("GITHUB_TOKEN").ok().filter(|s| !s.is_empty());
    let report = resolve(gh_token.as_deref(), github_token.as_deref(), probe.probe());
    if report.ok {
        log::info!(
            "credential_preflight: forge credential resolved via {} ({}) — #4005",
            report.mechanism,
            report.fingerprint.as_deref().unwrap_or("no fingerprint")
        );
    } else {
        log::error!("credential_preflight: {} — #4005", report.message);
    }
    report
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FixedProbe(Result<String, String>);
    impl GhAuthProbe for FixedProbe {
        fn probe(&self) -> Result<String, String> {
            self.0.clone()
        }
    }

    fn json_active(token_source: &str, state: &str, login: &str) -> String {
        format!(
            r#"{{"hosts":{{"github.com":[{{"state":"{state}","active":true,"host":"github.com","login":"{login}","tokenSource":"{token_source}"}}]}}}}"#
        )
    }

    #[test]
    fn env_gh_token_present_and_valid_reports_ok_with_env_mechanism() {
        let stdout = json_active("GH_TOKEN", "success", "");
        let report = resolve(Some("ghp_abcd1234wxyz"), None, Ok(stdout));
        assert!(report.ok);
        assert_eq!(report.mechanism, "GH_TOKEN");
        assert_eq!(report.fingerprint.as_deref(), Some("wxyz"));
        assert!(!report.message.contains("ghp_abcd1234wxyz"));
    }

    #[test]
    fn env_github_token_present_and_valid_reports_ok_with_env_mechanism() {
        let stdout = json_active("GITHUB_TOKEN", "success", "");
        let report = resolve(None, Some("ghp_zzzz9999abcd"), Ok(stdout));
        assert!(report.ok);
        assert_eq!(report.mechanism, "GITHUB_TOKEN");
        assert_eq!(report.fingerprint.as_deref(), Some("abcd"));
    }

    #[test]
    fn no_env_token_credential_store_success_reports_ok_with_login_fingerprint() {
        let stdout = json_active("keyring", "success", "octocat");
        let report = resolve(None, None, Ok(stdout));
        assert!(report.ok);
        assert_eq!(report.mechanism, "keyring");
        assert_eq!(report.fingerprint.as_deref(), Some("octocat"));
    }

    #[test]
    fn no_active_host_reports_degraded_none_mechanism() {
        let report = resolve(None, None, Ok(r#"{"hosts":{}}"#.to_string()));
        assert!(!report.ok);
        assert_eq!(report.mechanism, "none");
        assert!(report.message.contains("gh auth login") || report.message.contains("GH_TOKEN"));
    }

    #[test]
    fn active_host_with_error_state_reports_degraded() {
        let stdout = json_active("GH_TOKEN", "error", "");
        let report = resolve(Some("ghp_invalidinvalid"), None, Ok(stdout));
        assert!(!report.ok);
        assert_eq!(report.mechanism, "GH_TOKEN");
        assert!(report.fingerprint.is_none());
        assert!(!report.message.contains("ghp_invalidinvalid"));
    }

    #[test]
    fn probe_error_reports_unknown_mechanism_never_panics() {
        let report =
            resolve(None, None, Err("could not spawn `gh`: No such file or directory".to_string()));
        assert!(!report.ok);
        assert_eq!(report.mechanism, "unknown");
    }

    #[test]
    fn unparseable_probe_output_reports_unknown_mechanism_never_panics() {
        let report = resolve(None, None, Ok("not json at all".to_string()));
        assert!(!report.ok);
        assert_eq!(report.mechanism, "unknown");
    }

    #[test]
    fn env_fingerprint_never_leaks_full_token() {
        let token = "ghp_abcdefghijklmnopqrstuvwxyz0123456789";
        let fp = env_fingerprint(token);
        assert_eq!(fp.len(), 4);
        assert_ne!(fp, token);
        assert!(!fp.contains("ghp_"));
    }

    #[test]
    fn env_fingerprint_short_value_never_panics() {
        assert_eq!(env_fingerprint(""), "***");
        assert_eq!(env_fingerprint("ab"), "***");
    }

    #[test]
    fn run_with_missing_gh_binary_never_panics_and_reports_degraded() {
        // A `FixedProbe` standing in for "gh missing from PATH" — exercises
        // the same code path `RealGhAuthProbe` would hit via
        // `run_capture_with_timeout`'s spawn-failure branch, without
        // depending on the test host's actual PATH contents.
        let probe = FixedProbe(Err(
            "could not spawn `gh`: No such file or directory (os error 2)".to_string()
        ));
        let report = run(&probe);
        assert!(!report.ok);
        assert_eq!(report.mechanism, "unknown");
    }

    #[test]
    fn run_never_includes_a_token_looking_value_in_message() {
        let stdout = json_active("GH_TOKEN", "success", "");
        let probe = FixedProbe(Ok(stdout));
        // Test-only env mutation of a var no other test in this module reads
        // or writes; restored immediately after.
        std::env::set_var("GH_TOKEN", "ghp_supersecrettoken1234567890");
        let report = run(&probe);
        std::env::remove_var("GH_TOKEN");
        assert!(report.ok);
        assert!(!report.message.contains("ghp_supersecrettoken1234567890"));
        assert!(report.fingerprint.as_deref() != Some("ghp_supersecrettoken1234567890"));
    }

    #[test]
    fn real_probe_with_missing_gh_binary_never_panics() {
        let probe = RealGhAuthProbe {
            gh_bin: "loom-test-nonexistent-gh-binary-4005".to_string(),
            cwd: std::env::temp_dir(),
        };
        assert!(probe.probe().is_err());
    }
}
