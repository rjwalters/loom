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
//! # GitHub App identity (#4430) — a deliberate revision of the original
//! "no new credential store" non-goal
//!
//! The original #4005 non-goal below stated this module would never
//! provision, write, or manage a loom-specific credential file, reasoning
//! that a second secret-at-rest surface added attack surface for no
//! capability gain over the existing `GH_TOKEN`/keychain forwarding. #4430
//! revisits that trade-off deliberately: every fleet host authenticating as
//! the same personal account shares one 5,000/hr rate-limit bucket with the
//! operator's own interactive use, and long-lived PATs sitting on cloud disks
//! drift per-host. A GitHub App installation token is the opposite shape —
//! **short-lived** (1h, minted on-host from an app private key the operator
//! provisions once) and **per-installation rate-limited** — so the
//! calculus changes: a small, deliberately-scoped token *cache* (never the
//! private key itself, which stays wherever the operator put it) is worth
//! the added surface.
//!
//! [`run_with_github_app`] is the new entry point that supersedes [`run`] at
//! the call site in `main.rs`: it attempts the `"github-app"` mechanism
//! first (via the shell helper below) and falls through to the byte-identical
//! [`run`] ambient-`gh`-auth path whenever the app mechanism is unconfigured
//! *or* fails for any reason (expired/revoked/unreadable key, network
//! hiccup, GitHub API error) — see [`GithubAppOutcome`]. [`run`] itself is
//! unchanged and remains the whole story for every host that has never heard
//! of this feature (#4430 AC: "with them absent, behavior is unchanged").
//!
//! Minting itself is **shell, not Rust**: `loom-daemon` has no HTTP client or
//! JWT/RSA crate (see `Cargo.toml`), so JWT signing (`openssl dgst -sha256
//! -sign`) and the installation-token HTTP calls (`curl`) live in
//! `defaults/scripts/lib/github-app-token.sh`, invoked here via `Command`
//! exactly like every other forge call in this codebase. See that file's own
//! header comment for the minting/caching/refresh algorithm.
//!
//! # Non-goals
//!
//! - **No secret ever crosses into a `CredentialPreflightReport` or a log
//!   line.** The minted token is threaded back to the caller *only* via
//!   [`GithubAppPreflight::minted_gh_token`] (for `main.rs` to
//!   `std::env::set_var("GH_TOKEN", …)`) — never through [`log`], never
//!   through [`crate::types::DaemonStatusReport`]. The report/status surface
//!   carries only the non-secret fingerprint `app <id> installation <id>`.
//! - **GitHub only.** The daemon's own forge calls all shell out to `gh`,
//!   which only ever resolves GitHub credentials (whether that's an ambient
//!   credential or a `GH_TOKEN` this process minted itself). `GITEA_TOKEN`/
//!   `FORGE_TOKEN` forwarding exists solely for dispatched sweep children
//!   targeting a Gitea-backed repo — the daemon process itself never calls a
//!   Gitea API, so there is nothing to preflight for it here. See
//!   `.loom/docs/github-authentication.md` § "Headless and SSH-only daemon operation".
//! - **Never blocks or hangs.** Bounded by [`PREFLIGHT_TIMEOUT`] /
//!   [`GITHUB_APP_MINT_TIMEOUT`] via the reused
//!   [`crate::main_health_gate::run_capture_with_timeout`] helper — an
//!   unlocked-keychain prompt, a hung `gh`, or a hung mint script is exactly
//!   the failure mode this preflight must survive, not itself trigger.
//! - **No `--app-id`/`--app-key-file` fleet-provisioning flags in this PR.**
//!   `loom-daemon/src/fleet/add_worker.rs` keeps its existing `--pat-file`
//!   path unchanged; that flag is explicit follow-up work once this core
//!   lands (#4430 scope note).

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use chrono::Utc;
use serde_json::Value;

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

// ============================================================================
// GitHub App identity (#4430)
// ============================================================================

/// Bound on the `github-app-token.sh get-token` subprocess: a local JWT sign
/// (fast) plus up to two GitHub API round-trips (installation resolution +
/// token mint). Generous headroom for a slow network without risking a hung
/// preflight or refresh tick.
pub const GITHUB_APP_MINT_TIMEOUT: Duration = Duration::from_secs(20);

/// Default cadence for the background refresh tick that keeps the daemon's
/// own `GH_TOKEN` process env current across a token's ~1h lifetime.
/// Comfortably inside the 10-minute re-mint window the shell helper enforces,
/// so a tick never arrives after the window has already closed.
pub const GITHUB_APP_REFRESH_INTERVAL: Duration = Duration::from_secs(300);

/// Outcome of one GitHub App mint attempt (`github-app-token.sh get-token`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GithubAppOutcome {
    /// No app id / private-key path configured anywhere (env or config) —
    /// the load-bearing default. Every caller treats this identically to "no
    /// GitHub App feature exists on this host": fall through to the ambient
    /// `gh` auth path, never a failure.
    NotConfigured,
    /// A token was minted (or reused from the shell helper's on-disk cache).
    /// `token` must NEVER be logged, printed, or serialized — only
    /// `installation_id`/`app_id`/`expires_at` are safe to surface.
    Minted {
        token: String,
        installation_id: String,
        app_id: String,
        expires_at: String,
    },
    /// Configured, but minting failed this attempt (unreadable/revoked key,
    /// network failure, GitHub API error, malformed response, …). `reason`
    /// is human-readable and already scrubbed of secret material by the
    /// shell helper. Callers fall back to ambient auth rather than hard-fail.
    Error(String),
}

/// Injectable seam for minting an installation token, mirroring
/// [`GhAuthProbe`]. The real implementation shells out to
/// `github-app-token.sh get-token <owner/repo>` (#4430) — no new Rust
/// HTTP/JWT dependency, per the issue's shell-first mandate.
pub trait GithubAppMinter {
    /// Attempt to resolve a usable installation token for `owner_repo`
    /// (`"owner/repo"`, the installation-selection key — see module docs).
    fn mint(&self, owner_repo: &str) -> GithubAppOutcome;
}

/// The concrete minter: `bash <script_path> get-token <owner_repo>`, bounded
/// by [`GITHUB_APP_MINT_TIMEOUT`].
pub struct RealGithubAppMinter {
    /// Path to `github-app-token.sh` (see [`resolve_github_app_script`]).
    pub script_path: PathBuf,
    /// Working directory for the subprocess (any existing directory works —
    /// the script resolves its own config root independently).
    pub cwd: PathBuf,
}

impl GithubAppMinter for RealGithubAppMinter {
    fn mint(&self, owner_repo: &str) -> GithubAppOutcome {
        let script_path_str = self.script_path.to_string_lossy().to_string();
        let stdout = match run_capture_with_timeout(
            "bash",
            &[script_path_str.as_str(), "get-token", owner_repo],
            &self.cwd,
            GITHUB_APP_MINT_TIMEOUT,
        ) {
            Ok(s) => s,
            Err(e) => {
                return GithubAppOutcome::Error(format!(
                    "could not run github-app-token.sh get-token: {e}"
                ))
            }
        };
        parse_github_app_response(&stdout)
    }
}

/// Parse `github-app-token.sh`'s single-line JSON envelope
/// (`{"status": "ok"|"not_configured"|"error", …}`) into a
/// [`GithubAppOutcome`]. Split out from I/O so it is unit-testable without a
/// real subprocess — mirrors [`resolve`].
fn parse_github_app_response(stdout: &str) -> GithubAppOutcome {
    let parsed: Value = match serde_json::from_str(stdout.trim()) {
        Ok(v) => v,
        Err(e) => {
            return GithubAppOutcome::Error(format!(
                "could not parse github-app-token.sh output ({e})"
            ))
        }
    };
    let status = parsed.get("status").and_then(Value::as_str).unwrap_or("");
    match status {
        "not_configured" => GithubAppOutcome::NotConfigured,
        "ok" => {
            let get = |k: &str| {
                parsed
                    .get(k)
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string()
            };
            let token = get("token");
            if token.is_empty() {
                return GithubAppOutcome::Error(
                    "github-app-token.sh reported ok but returned no token".to_string(),
                );
            }
            GithubAppOutcome::Minted {
                token,
                installation_id: get("installation_id"),
                app_id: get("app_id"),
                expires_at: get("expires_at"),
            }
        }
        _ => {
            let message = parsed
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("github-app-token.sh reported an unrecognized status")
                .to_string();
            GithubAppOutcome::Error(message)
        }
    }
}

/// Resolve the `github-app-token.sh` path: prefer the installed
/// `.loom/scripts/lib/` copy, else the in-repo `defaults/scripts/lib/`
/// source — mirrors `auto_update::ScriptAutoUpdateProbe::resolve_script`.
/// `None` when neither exists (a stale install predating #4430, or a
/// workspace root that isn't a Loom-managed checkout at all) — every caller
/// treats a `None` exactly like [`GithubAppOutcome::NotConfigured`].
#[must_use]
pub fn resolve_github_app_script(root: &Path) -> Option<PathBuf> {
    let installed = root.join(".loom/scripts/lib/github-app-token.sh");
    if installed.exists() {
        return Some(installed);
    }
    let source = root.join("defaults/scripts/lib/github-app-token.sh");
    if source.exists() {
        return Some(source);
    }
    None
}

/// Resolve the `owner/repo` name-with-owner for `cwd`'s `origin` remote via a
/// plain `git remote get-url` + URL parse — deliberately **not** a `gh` call,
/// so this never depends on the very credential this module is trying to
/// establish. Installation selection is derivable per repo (#4430: "GET
/// /repos/{owner}/{repo}/installation resolves the installation for any repo
/// the app can see"), so this is the key the shell helper caches against.
#[must_use]
pub fn nwo_from_git_remote(cwd: &Path) -> Option<String> {
    let output = Command::new("git")
        .args(["remote", "get-url", "origin"])
        .current_dir(cwd)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let url = String::from_utf8(output.stdout).ok()?.trim().to_string();
    if url.is_empty() {
        return None;
    }
    let stripped = url.strip_suffix(".git").unwrap_or(url.as_str());

    let path = if let Some(rest) = stripped.strip_prefix("git@") {
        // SSH: git@host:owner/repo
        let (_host, p) = rest.split_once(':')?;
        p
    } else if let Some(rest) = stripped.strip_prefix("https://") {
        // HTTPS: https://host/owner/repo
        let (_host, p) = rest.split_once('/')?;
        p
    } else if let Some(rest) = stripped.strip_prefix("http://") {
        let (_host, p) = rest.split_once('/')?;
        p
    } else {
        return None;
    };

    let trimmed = path.trim_matches('/');
    let (owner, repo) = trimmed.split_once('/')?;
    if owner.is_empty() || repo.is_empty() {
        return None;
    }
    Some(format!("{owner}/{repo}"))
}

/// The full startup preflight result when the GitHub App mechanism is in
/// play: the report to surface (log line, `loom-daemon status`,
/// [`crate::types::DaemonStatusReport`]) plus the minted token — if any —
/// for the caller to export as the daemon process's own `GH_TOKEN`.
///
/// `minted_gh_token` is the ONLY channel a token value travels through; it is
/// deliberately excluded from [`Self::report`] (and therefore from every log
/// line and status surface) by construction.
pub struct GithubAppPreflight {
    pub report: CredentialPreflightReport,
    pub minted_gh_token: Option<String>,
}

/// Runs the full #4430 preflight: attempt a GitHub App mint first (when
/// `owner_repo` resolved at all); fall through to the byte-identical
/// pre-#4430 [`run`] (ambient `gh` auth) whenever the app mechanism is
/// unconfigured, and STILL fall through — with an additional log line naming
/// the failure — when it is configured but minting fails for any reason
/// (#4430 AC: "daemon falls back to ambient auth rather than hard-failing").
///
/// This is the call site `main.rs` uses in place of [`run`] directly; `run`
/// itself is unchanged, so a host with no `owner_repo` resolved (e.g. the
/// daemon's workspace root isn't a git checkout) or no app configured gets
/// exactly the pre-#4430 log lines and report shape.
#[must_use]
pub fn run_with_github_app(
    gh_probe: &dyn GhAuthProbe,
    app_minter: &dyn GithubAppMinter,
    owner_repo: Option<&str>,
) -> GithubAppPreflight {
    let Some(owner_repo) = owner_repo else {
        return GithubAppPreflight {
            report: run(gh_probe),
            minted_gh_token: None,
        };
    };

    match app_minter.mint(owner_repo) {
        GithubAppOutcome::NotConfigured => GithubAppPreflight {
            report: run(gh_probe),
            minted_gh_token: None,
        },
        GithubAppOutcome::Minted {
            token,
            installation_id,
            app_id,
            ..
        } => {
            let fingerprint = format!("app {app_id} installation {installation_id}");
            log::info!(
                "credential_preflight: forge credential resolved via github-app ({fingerprint}) — #4430"
            );
            GithubAppPreflight {
                report: CredentialPreflightReport {
                    ok: true,
                    mechanism: "github-app".to_string(),
                    fingerprint: Some(fingerprint),
                    message: format!(
                        "authenticated via github-app (app {app_id}, installation {installation_id})"
                    ),
                    checked_at: Utc::now(),
                },
                minted_gh_token: Some(token),
            }
        }
        GithubAppOutcome::Error(reason) => {
            log::error!(
                "credential_preflight: github-app mint failed ({reason}); falling back to ambient \
                 gh auth — #4430"
            );
            let mut fallback = run(gh_probe);
            fallback.message = format!("github-app unavailable ({reason}); {}", fallback.message);
            GithubAppPreflight {
                report: fallback,
                minted_gh_token: None,
            }
        }
    }
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

    // ------------------------------------------------------------------
    // GitHub App identity (#4430)
    // ------------------------------------------------------------------

    struct FixedMinter(GithubAppOutcome);
    impl GithubAppMinter for FixedMinter {
        fn mint(&self, _owner_repo: &str) -> GithubAppOutcome {
            self.0.clone()
        }
    }

    #[test]
    fn parse_github_app_response_not_configured() {
        let outcome = parse_github_app_response(
            r#"{"status":"not_configured","message":"github app not configured"}"#,
        );
        assert_eq!(outcome, GithubAppOutcome::NotConfigured);
    }

    #[test]
    fn parse_github_app_response_ok_minted() {
        let outcome = parse_github_app_response(
            r#"{"status":"ok","token":"ghs_abc123","installation_id":"999","app_id":"42","expires_at":"2099-01-01T00:00:00Z"}"#,
        );
        assert_eq!(
            outcome,
            GithubAppOutcome::Minted {
                token: "ghs_abc123".to_string(),
                installation_id: "999".to_string(),
                app_id: "42".to_string(),
                expires_at: "2099-01-01T00:00:00Z".to_string(),
            }
        );
    }

    #[test]
    fn parse_github_app_response_ok_without_token_is_an_error_never_a_panic() {
        let outcome = parse_github_app_response(r#"{"status":"ok"}"#);
        assert!(matches!(outcome, GithubAppOutcome::Error(_)));
    }

    #[test]
    fn parse_github_app_response_error_status_carries_message() {
        let outcome = parse_github_app_response(
            r#"{"status":"error","message":"could not resolve installation"}"#,
        );
        assert_eq!(outcome, GithubAppOutcome::Error("could not resolve installation".to_string()));
    }

    #[test]
    fn parse_github_app_response_malformed_json_never_panics() {
        let outcome = parse_github_app_response("not json at all");
        assert!(matches!(outcome, GithubAppOutcome::Error(_)));
    }

    #[test]
    fn run_with_github_app_no_owner_repo_falls_back_to_ambient_run() {
        let stdout = json_active("GH_TOKEN", "success", "");
        let probe = FixedProbe(Ok(stdout));
        let minter = FixedMinter(GithubAppOutcome::NotConfigured);
        let result = run_with_github_app(&probe, &minter, None);
        assert_eq!(result.report.mechanism, "GH_TOKEN");
        assert!(result.minted_gh_token.is_none());
    }

    #[test]
    fn run_with_github_app_not_configured_falls_back_to_ambient_run_byte_identical() {
        let stdout = json_active("keyring", "success", "octocat");
        let probe = FixedProbe(Ok(stdout));
        let minter = FixedMinter(GithubAppOutcome::NotConfigured);
        let result = run_with_github_app(&probe, &minter, Some("owner/repo"));
        // Identical to calling `run(&probe)` directly (modulo `checked_at`,
        // which is a fresh `Utc::now()` timestamp on each call).
        let direct = run(&probe);
        assert_eq!(result.report.ok, direct.ok);
        assert_eq!(result.report.mechanism, direct.mechanism);
        assert_eq!(result.report.fingerprint, direct.fingerprint);
        assert_eq!(result.report.message, direct.message);
        assert!(result.minted_gh_token.is_none());
    }

    #[test]
    fn run_with_github_app_minted_reports_github_app_mechanism_and_returns_token() {
        let stdout = json_active("keyring", "success", "octocat");
        let probe = FixedProbe(Ok(stdout));
        let minter = FixedMinter(GithubAppOutcome::Minted {
            token: "ghs_supersecrettoken".to_string(),
            installation_id: "999".to_string(),
            app_id: "42".to_string(),
            expires_at: "2099-01-01T00:00:00Z".to_string(),
        });
        let result = run_with_github_app(&probe, &minter, Some("owner/repo"));
        assert!(result.report.ok);
        assert_eq!(result.report.mechanism, "github-app");
        assert_eq!(result.report.fingerprint.as_deref(), Some("app 42 installation 999"));
        assert!(!result.report.message.contains("ghs_supersecrettoken"));
        assert_eq!(result.minted_gh_token.as_deref(), Some("ghs_supersecrettoken"));
    }

    #[test]
    fn run_with_github_app_error_falls_back_to_ambient_auth_never_a_hard_failure() {
        let stdout = json_active("GH_TOKEN", "success", "");
        let probe = FixedProbe(Ok(stdout));
        let minter =
            FixedMinter(GithubAppOutcome::Error("github app private key not readable".to_string()));
        let result = run_with_github_app(&probe, &minter, Some("owner/repo"));
        // Falls back to the ambient mechanism (still resolvable here), not a
        // hard failure -- mirrors the #4430 AC on expired/revoked/unreadable
        // keys.
        assert!(result.report.ok);
        assert_eq!(result.report.mechanism, "GH_TOKEN");
        assert!(result.report.message.contains("github-app unavailable"));
        assert!(result.minted_gh_token.is_none());
    }

    #[test]
    fn nwo_from_git_remote_parses_ssh_and_https_urls() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path();

        let init = Command::new("git")
            .args(["init", "-q"])
            .current_dir(path)
            .status()
            .unwrap();
        assert!(init.success());

        let add = Command::new("git")
            .args([
                "remote",
                "add",
                "origin",
                "git@github.com:rjwalters/loom.git",
            ])
            .current_dir(path)
            .status()
            .unwrap();
        assert!(add.success());

        assert_eq!(nwo_from_git_remote(path), Some("rjwalters/loom".to_string()));

        let set_url = Command::new("git")
            .args([
                "remote",
                "set-url",
                "origin",
                "https://github.com/2AMLogic/klayout-tools.git",
            ])
            .current_dir(path)
            .status()
            .unwrap();
        assert!(set_url.success());

        assert_eq!(nwo_from_git_remote(path), Some("2AMLogic/klayout-tools".to_string()));
    }

    #[test]
    fn nwo_from_git_remote_no_remote_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        let init = Command::new("git")
            .args(["init", "-q"])
            .current_dir(dir.path())
            .status()
            .unwrap();
        assert!(init.success());
        assert_eq!(nwo_from_git_remote(dir.path()), None);
    }

    #[test]
    fn resolve_github_app_script_prefers_installed_over_source() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join(".loom/scripts/lib")).unwrap();
        std::fs::create_dir_all(root.join("defaults/scripts/lib")).unwrap();
        std::fs::write(
            root.join("defaults/scripts/lib/github-app-token.sh"),
            "#!/usr/bin/env bash\n",
        )
        .unwrap();
        // Only the source copy exists -> that one wins.
        assert_eq!(
            resolve_github_app_script(root),
            Some(root.join("defaults/scripts/lib/github-app-token.sh"))
        );

        std::fs::write(root.join(".loom/scripts/lib/github-app-token.sh"), "#!/usr/bin/env bash\n")
            .unwrap();
        // Both exist -> the installed copy wins.
        assert_eq!(
            resolve_github_app_script(root),
            Some(root.join(".loom/scripts/lib/github-app-token.sh"))
        );
    }

    #[test]
    fn resolve_github_app_script_neither_present_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(resolve_github_app_script(dir.path()), None);
    }
}
