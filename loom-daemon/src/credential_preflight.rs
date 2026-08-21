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
//!   [`GithubAppPreflight::minted_gh_token`] (for `main.rs` to publish via
//!   [`publish_github_app_token`] — see "File-based token delivery (#4458)"
//!   below) — never through [`log`], never through
//!   [`crate::types::DaemonStatusReport`]. The report/status surface
//!   carries only the non-secret fingerprint `app <id> installation <id>`.
//! - **GitHub only.** The daemon's own forge calls all shell out to `gh`,
//!   which only ever resolves GitHub credentials (whether that's an ambient
//!   credential or a `GH_TOKEN` this process minted itself). `GITEA_TOKEN`/
//!   `FORGE_TOKEN` forwarding exists solely for dispatched sweep children
//!   targeting a Gitea-backed repo — the daemon process itself never calls a
//!   Gitea API, so there is nothing to preflight for it here. See
//!   `.loom/docs/github-authentication.md` § "Headless and SSH-only daemon operation".
//! - **Never blocks or hangs.** Bounded by [`PREFLIGHT_TIMEOUT`] /
//!   [`resolve_github_app_mint_timeout`] via the reused
//!   [`crate::main_health_gate::run_capture_with_timeout`] helper — an
//!   unlocked-keychain prompt, a hung `gh`, or a hung mint script is exactly
//!   the failure mode this preflight must survive, not itself trigger.
//! - **No `--app-id`/`--app-key-file` fleet-provisioning flags in this PR.**
//!   `loom-daemon/src/fleet/add_worker.rs` keeps its existing `--pat-file`
//!   path unchanged; that flag is explicit follow-up work once this core
//!   lands (#4430 scope note).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

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

/// Default bound on the `github-app-token.sh get-token` subprocess: a local JWT
/// sign (fast) plus up to two GitHub API round-trips (installation resolution +
/// token mint).
///
/// **Raised from a fixed 20s to 90s in #5630.** The 20s budget was sized for
/// the happy path (the helper returns in ~30ms when invoked by hand) and held
/// on an idle host — but on a saturated fleet host (`observed_idle=0%`, 16
/// concurrent agents) the *fork/exec* of `bash`, the `openssl` JWT sign, and
/// two `curl` round-trips all queue behind every other runnable process, and
/// the whole thing routinely blew past 20s. The daemon then declared the mint
/// a failure, left `GH_TOKEN` stale, and every downstream `gh` call across
/// every managed repo started failing at once — a credential-timing artifact
/// that read as "22 of 22 repos have a broken main".
///
/// The bound is still a bound (nothing hangs forever) — it is just no longer
/// tighter than the worst-case scheduling latency it has to survive. Override
/// per host via [`GITHUB_APP_MINT_TIMEOUT_ENV`] or
/// `forge.githubApp.mintTimeoutSeconds` — the same config namespace
/// `github-app-token.sh` itself reads `appId` / `privateKeyPath` from, so an
/// operator configuring the App finds every knob in one place. See
/// [`resolve_github_app_mint_timeout`].
pub const DEFAULT_GITHUB_APP_MINT_TIMEOUT: Duration = Duration::from_secs(90);

/// Env override for the mint subprocess bound, in **seconds** (#5630).
/// Precedence **env > config > [`DEFAULT_GITHUB_APP_MINT_TIMEOUT`]**, matching
/// every other daemon knob. A zero / unparseable value falls through.
pub const GITHUB_APP_MINT_TIMEOUT_ENV: &str = "LOOM_GITHUB_APP_MINT_TIMEOUT_SECS";

/// How many times to invoke `github-app-token.sh get-token` before declaring
/// the mint failed (#5630). Only *transport-level* failures (timeout, spawn
/// error, non-zero exit — i.e. the subprocess never produced a parseable
/// answer) are retried: a parsed `{"status":"error"}` envelope is a
/// deterministic answer from the helper (bad key, app not installed) and
/// re-running it would only double the latency for the same result.
///
/// Two attempts, not more: the observed failure is a *scheduling* hiccup on a
/// momentarily-saturated host, which a single re-try a moment later clears.
pub const GITHUB_APP_MINT_ATTEMPTS: u32 = 2;

/// Pause between mint attempts (#5630) — long enough to let a transient
/// fork/exec storm drain, short enough that the whole retry budget stays well
/// inside one [`GITHUB_APP_REFRESH_INTERVAL`] tick.
pub const GITHUB_APP_MINT_RETRY_DELAY: Duration = Duration::from_secs(2);

/// Resolve the mint subprocess bound for `repo_root` with precedence
/// **env > config (`forge.githubApp.mintTimeoutSeconds`) >
/// [`DEFAULT_GITHUB_APP_MINT_TIMEOUT`]** (#5630).
#[must_use]
pub fn resolve_github_app_mint_timeout(repo_root: &Path) -> Duration {
    env_github_app_mint_timeout_secs()
        .or_else(|| config_github_app_mint_timeout_secs(repo_root))
        .map_or(DEFAULT_GITHUB_APP_MINT_TIMEOUT, Duration::from_secs)
}

/// [`GITHUB_APP_MINT_TIMEOUT_ENV`] as seconds, filtered to `> 0`.
fn env_github_app_mint_timeout_secs() -> Option<u64> {
    std::env::var(GITHUB_APP_MINT_TIMEOUT_ENV)
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|&s| s > 0)
}

/// `forge.githubApp.mintTimeoutSeconds` from config, filtered to `> 0`.
fn config_github_app_mint_timeout_secs(repo_root: &Path) -> Option<u64> {
    let effective = crate::config_resolver::resolve_effective_config(repo_root);
    crate::config_resolver::get_path(&effective, "forge.githubApp")?
        .get("mintTimeoutSeconds")
        .and_then(Value::as_u64)
        .filter(|&s| s > 0)
}

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

    /// As [`Self::mint`] but bypasses the shell helper's on-disk token cache
    /// (`get-token --force`), always minting a fresh installation token
    /// (#6171). An installation token is scoped to the repositories the
    /// installation could reach at mint time — a cached token can therefore
    /// be stale in *scope*, not just in *expiry*, whenever the managed-repo
    /// set has grown since the last mint (e.g. a `workspace add` against an
    /// owner this daemon already manages). Defaults to [`Self::mint`] so
    /// existing test doubles need no changes; [`RealGithubAppMinter`]
    /// overrides it to pass `--force` through to the shell helper.
    fn mint_forced(&self, owner_repo: &str) -> GithubAppOutcome {
        self.mint(owner_repo)
    }
}

/// The concrete minter: `bash <script_path> get-token [--force] <owner_repo>`,
/// bounded by [`resolve_github_app_mint_timeout`] and retried once on a
/// transport-level failure (#5630).
pub struct RealGithubAppMinter {
    /// Path to `github-app-token.sh` (see [`resolve_github_app_script`]).
    pub script_path: PathBuf,
    /// Working directory for the subprocess (any existing directory works —
    /// the script resolves its own config root independently). Also the root
    /// the mint-timeout knob is resolved against.
    pub cwd: PathBuf,
}

impl RealGithubAppMinter {
    /// Shared implementation behind [`GithubAppMinter::mint`] /
    /// [`GithubAppMinter::mint_forced`] (#6171) — the only difference between
    /// the two is whether `--force` is passed to the shell helper.
    fn mint_with_force(&self, owner_repo: &str, force: bool) -> GithubAppOutcome {
        let script_path_str = self.script_path.to_string_lossy().to_string();
        let timeout = resolve_github_app_mint_timeout(&self.cwd);
        let mut args: Vec<&str> = vec![script_path_str.as_str(), "get-token"];
        if force {
            args.push("--force");
        }
        args.push(owner_repo);
        mint_with_retry(GITHUB_APP_MINT_ATTEMPTS, GITHUB_APP_MINT_RETRY_DELAY, |_attempt| {
            run_capture_with_timeout("bash", &args, &self.cwd, timeout)
        })
    }
}

impl GithubAppMinter for RealGithubAppMinter {
    fn mint(&self, owner_repo: &str) -> GithubAppOutcome {
        self.mint_with_force(owner_repo, false)
    }

    fn mint_forced(&self, owner_repo: &str) -> GithubAppOutcome {
        self.mint_with_force(owner_repo, true)
    }
}

/// Run `mint_once` up to `attempts` times, sleeping `retry_delay` between
/// attempts, and parse the first successful invocation's stdout (#5630).
///
/// Split out from [`RealGithubAppMinter`] so the retry policy is unit-testable
/// without a real subprocess — mirrors [`parse_github_app_response`]'s
/// I/O-free split.
///
/// Only a transport-level `Err` (timeout, spawn failure, non-zero exit) is
/// retried: an `Ok(stdout)` is a real answer from the helper, including a
/// `{"status":"error"}` envelope, and re-running it would only spend another
/// timeout budget to get the identical deterministic result. The returned
/// error names the attempt count so the log line distinguishes "one slow
/// tick" from "this host cannot mint at all".
pub fn mint_with_retry<F>(
    attempts: u32,
    retry_delay: Duration,
    mut mint_once: F,
) -> GithubAppOutcome
where
    F: FnMut(u32) -> std::result::Result<String, String>,
{
    let attempts = attempts.max(1);
    let mut last_error = String::new();
    for attempt in 1..=attempts {
        match mint_once(attempt) {
            Ok(stdout) => return parse_github_app_response(&stdout),
            Err(e) => {
                last_error = e;
                if attempt < attempts {
                    log::debug!(
                        "credential_preflight: github-app mint attempt {attempt}/{attempts} \
                         failed ({last_error}); retrying in {}s — #5630",
                        retry_delay.as_secs()
                    );
                    std::thread::sleep(retry_delay);
                }
            }
        }
    }
    GithubAppOutcome::Error(format!(
        "could not run github-app-token.sh get-token after {attempts} attempt(s): {last_error}"
    ))
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
            let installation_id = get("installation_id");
            if installation_id.is_empty() {
                // #5401: a successful mint with a blank `installation_id` is a
                // defect in the shell helper (historically a bash
                // command-substitution subshell-scoping bug that silently
                // dropped the field), not a legitimate outcome — surface it as
                // an `Error` rather than a `Minted` whose fingerprint renders
                // as "installation " with nothing after it, which reads as
                // healthy on `loom-daemon status` while hiding the exact
                // detail (which installation minted the token) that would
                // reveal a wrong-owner mismatch.
                return GithubAppOutcome::Error(
                    "github-app-token.sh reported ok with a token but an empty installation_id \
                     (defect, not a valid mint)"
                        .to_string(),
                );
            }
            GithubAppOutcome::Minted {
                token,
                installation_id,
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

// ============================================================================
// Forge-credential freshness (#5630)
//
// The daemon's own `GH_TOKEN` / `GH_CONFIG_DIR` credentials are refreshed by
// background ticks (see `daemon_service`): one for the primary credential
// (#4430) and one per cross-owner credential (#5401). When a tick fails, the
// credential on disk keeps aging and EVERY forge call the daemon makes — in
// every managed repo, all at once — starts answering wrong. The main-health
// gate reads those wrong answers as "main is red" and halts dispatch host-wide;
// the observed signature was 22 of 22 repos flipping to halted within one tick
// of a refresh timeout, then flipping back when the next tick succeeded.
//
// 22-at-once is a *credential* signature, not 22 broken mains. This tracker is
// the missing input that lets the gate tell those apart: it records the current
// consecutive-failure streak of the refresh tick so a consumer can ask "were my
// forge answers trustworthy just now?" before treating a failure as evidence
// about `main`.
//
// The window is deliberately **bounded** (`FORGE_CREDENTIAL_STALE_GRACE`): a
// brief hiccup suppresses new halt verdicts, but a credential that has been
// broken for half an hour is a genuine operator problem, and at that point the
// pre-#5630 fail-safe (evaluate normally; a failure halts) must reassert rather
// than freeze every repo's verdict forever.
// ============================================================================

/// How long after the **first** failure of a consecutive refresh-failure streak
/// the daemon keeps treating its forge answers as untrustworthy (#5630).
///
/// Sized well above the ~5-minute [`GITHUB_APP_REFRESH_INTERVAL`] so a handful
/// of consecutive saturation-induced timeouts stay inside the window, and well
/// below "forever" so a genuinely broken credential stops masking real signal.
pub const DEFAULT_FORGE_CREDENTIAL_STALE_GRACE: Duration = Duration::from_secs(1800);

/// Env override for [`DEFAULT_FORGE_CREDENTIAL_STALE_GRACE`], in **seconds**.
/// A zero / unparseable value falls through to the default. Set it to a small
/// value to effectively restore pre-#5630 behavior.
pub const FORGE_CREDENTIAL_STALE_GRACE_ENV: &str = "LOOM_FORGE_CREDENTIAL_STALE_GRACE_SECS";

/// Source label for the primary (`#4430`) credential-refresh tick — the one
/// that maintains the daemon's own `GH_TOKEN` / `GH_CONFIG_DIR`.
pub const CREDENTIAL_SOURCE_PRIMARY: &str = "github-app refresh tick";

/// Source label for a cross-owner (`#5401`) credential-refresh tick, one per
/// distinct owner whose repos get their own `GH_CONFIG_DIR`.
#[must_use]
pub fn credential_source_for_owner(owner_repo: &str) -> String {
    format!("per-owner github-app refresh ({owner_repo})")
}

/// One refresh source's current consecutive-failure streak. Absent from
/// [`CredentialStreaks`] ⇒ that source's most recent attempt succeeded (the
/// steady state).
#[derive(Debug, Clone)]
struct CredentialFailureStreak {
    /// When the *first* failure of this streak was recorded — the anchor the
    /// grace window is measured from, so a streak cannot renew its own window
    /// indefinitely by failing again.
    first_failure_at: Instant,
    /// How many consecutive failures have been recorded in this streak.
    failures: u32,
    /// The most recent failure's (already secret-scrubbed) reason, for the log
    /// line that explains why a gate tick was held.
    last_reason: String,
}

/// Per-source refresh-failure streaks — the whole tracker as a **plain value**.
///
/// Keyed by source rather than a single slot because the daemon runs several
/// independent refresh loops (primary + one per cross-owner credential). With
/// one shared slot, a healthy source's success would silently clear a
/// genuinely-failing source's streak on the next tick — the hold would
/// evaporate exactly when it is most needed. An empty map means every source is
/// healthy.
///
/// Deliberately a value type with no ambient state of its own: the production
/// singleton is one `static` built out of it ([`forge_credential_streaks`]),
/// and every unit test drives a **local** instance instead. That matters
/// because `main_health_gate`'s production [`crate::main_health_gate::
/// GlobalCredentialFreshness`] reads the singleton — if these tests mutated it,
/// they would non-deterministically flip unrelated gate tests running
/// concurrently in the same process into `ForgeCredentialStale` (the #4385
/// hazard, with a bite).
#[derive(Debug, Default)]
pub struct CredentialStreaks {
    by_source: HashMap<String, CredentialFailureStreak>,
}

impl CredentialStreaks {
    /// Record that `source`'s refresh attempt **failed** at `now`. Starts that
    /// source's streak (anchoring its grace window) or extends the existing one.
    pub fn record_failure_at(&mut self, source: &str, reason: &str, now: Instant) {
        self.by_source
            .entry(source.to_string())
            .and_modify(|streak| {
                streak.failures = streak.failures.saturating_add(1);
                streak.last_reason = reason.to_string();
            })
            .or_insert_with(|| CredentialFailureStreak {
                first_failure_at: now,
                failures: 1,
                last_reason: reason.to_string(),
            });
    }

    /// Record that `source`'s refresh attempt **succeeded**, ending that
    /// source's streak. Idempotent; other sources' streaks are untouched.
    pub fn record_success(&mut self, source: &str) {
        self.by_source.remove(source);
    }

    /// Whether **any** source is inside its `grace` window as of `now`.
    ///
    /// Deliberately "any", not "all": the gate consumes one host-wide answer,
    /// and a single failing credential is enough to make some managed repos'
    /// forge calls start lying. Holding a verdict is cheap; a false host-wide
    /// halt is not.
    #[must_use]
    pub fn is_stale_at(&self, now: Instant, grace: Duration) -> bool {
        self.by_source
            .values()
            .any(|s| credential_stale_hold(Some(s.first_failure_at), now, grace))
    }

    /// A short, non-secret summary of the current stale window, or `None` when
    /// every source is healthy (or every streak's grace window has expired).
    ///
    /// When several sources are failing at once the **oldest** active streak is
    /// described (its grace window expires first, so it is the one an operator
    /// most needs to see), with a count of the others.
    #[must_use]
    pub fn summary_at(&self, now: Instant, grace: Duration) -> Option<String> {
        let mut active: Vec<(&String, &CredentialFailureStreak)> = self
            .by_source
            .iter()
            .filter(|(_, s)| credential_stale_hold(Some(s.first_failure_at), now, grace))
            .collect();
        // Oldest first; tie-break on the source name so the message is stable.
        active.sort_by(|a, b| {
            a.1.first_failure_at
                .cmp(&b.1.first_failure_at)
                .then(a.0.cmp(b.0))
        });
        let (source, streak) = active.first()?;
        let others = match active.len() {
            1 => String::new(),
            n => format!(" (+{} other failing credential source(s))", n - 1),
        };
        Some(format!(
            "{source}: {} consecutive failure(s) over the last {}s (grace {}s){others}; last: {}",
            streak.failures,
            now.saturating_duration_since(streak.first_failure_at)
                .as_secs(),
            grace.as_secs(),
            streak.last_reason
        ))
    }
}

/// The one production [`CredentialStreaks`], shared by the refresh loops that
/// write it and the main-health gate that reads it.
static FORGE_CREDENTIAL_STREAKS: OnceLock<Mutex<CredentialStreaks>> = OnceLock::new();

fn forge_credential_streaks() -> &'static Mutex<CredentialStreaks> {
    FORGE_CREDENTIAL_STREAKS.get_or_init(|| Mutex::new(CredentialStreaks::default()))
}

/// Empty the process-global streak tracker — **tests only** (#6663).
///
/// The singleton outlives every individual test in the lib target, so a test
/// that exercises a production path which records a failure (the only one is
/// [`force_refresh_owner_credential_with`]'s `Error` arm) leaves the tracker
/// "stale" for the next 30 minutes of grace, i.e. for the rest of the test
/// binary's life. Any later test that reads the singleton then sees
/// `ForgeCredentialStale` instead of the verdict it asserts, which is exactly
/// the ordering-dependent RED that #6663 reports.
///
/// The rule for using this: a test that deliberately drives the **global**
/// tracker must (a) be `#[serial_test::serial]` on the default key, and (b)
/// reset both before and after, so it neither inherits nor exports poison.
/// Tests that merely need an answer about credential freshness should inject
/// one instead (`CommandGateRunner::with_credential_freshness`,
/// `run_gate_tick_with_fns`) and never touch this.
#[cfg(test)]
pub(crate) fn reset_forge_credential_streaks() {
    *forge_credential_streaks()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = CredentialStreaks::default();
}

/// Resolve the stale-credential grace window: **env >
/// [`DEFAULT_FORGE_CREDENTIAL_STALE_GRACE`]**. Deliberately env-only (no
/// per-repo config key): the credential is daemon-global, so a per-repo knob
/// would be ambiguous about which repo's value wins.
#[must_use]
pub fn resolve_forge_credential_stale_grace() -> Duration {
    std::env::var(FORGE_CREDENTIAL_STALE_GRACE_ENV)
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|&s| s > 0)
        .map_or(DEFAULT_FORGE_CREDENTIAL_STALE_GRACE, Duration::from_secs)
}

/// The pure decision behind [`forge_credential_stale`] (#5630): given when a
/// failure streak started (`None` ⇒ no streak), the current instant, and the
/// grace window, should the daemon treat its forge answers as untrustworthy?
///
/// `true` only inside the window measured from the streak's **first** failure —
/// so the hold is bounded even while failures keep arriving.
#[must_use]
pub fn credential_stale_hold(
    first_failure_at: Option<Instant>,
    now: Instant,
    grace: Duration,
) -> bool {
    first_failure_at.is_some_and(|t| now.saturating_duration_since(t) < grace)
}

/// Record that `source`'s forge-credential refresh attempt **failed** on the
/// process-global tracker. Thin wrapper over
/// [`CredentialStreaks::record_failure_at`] — the logic lives there so tests
/// never have to touch the singleton.
pub fn record_forge_credential_failure(source: &str, reason: &str) {
    forge_credential_streaks()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .record_failure_at(source, reason, Instant::now());
}

/// Record that `source`'s forge-credential refresh attempt **succeeded** on the
/// process-global tracker, ending that source's streak. Idempotent and cheap —
/// safe to call on every successful tick.
pub fn record_forge_credential_success(source: &str) {
    forge_credential_streaks()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .record_success(source);
}

/// Whether **any** of the daemon's forge credentials is currently within a
/// bounded stale window (#5630) — i.e. some refresh tick is failing recently
/// enough that forge answers (and anything that shells out to `gh`/`git`
/// against the forge) should not be taken as evidence about a repo's `main`.
#[must_use]
pub fn forge_credential_stale() -> bool {
    let grace = resolve_forge_credential_stale_grace();
    forge_credential_streaks()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .is_stale_at(Instant::now(), grace)
}

/// A short, non-secret human-readable summary of the current stale window, or
/// `None` when every credential is healthy (or every streak's grace window has
/// expired). Used verbatim in the gate's held-tick log line.
#[must_use]
pub fn forge_credential_stale_summary() -> Option<String> {
    let grace = resolve_forge_credential_stale_grace();
    forge_credential_streaks()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .summary_at(Instant::now(), grace)
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

/// The owner segment of a `"owner/repo"` string (everything before the first
/// `/`). Falls back to the whole input for a malformed value rather than
/// panicking — this is only ever fed strings [`nwo_from_git_remote`] already
/// validated to contain a `/`, but stays total for any future caller.
fn owner_of(owner_repo: &str) -> &str {
    owner_repo.split('/').next().unwrap_or(owner_repo)
}

/// #5401: the GitHub App mechanism mints exactly ONE installation token per
/// daemon process, keyed on `root_owner_repo` (the workspace root's own
/// `owner/repo`, from [`nwo_from_git_remote`]). An installation token is
/// scoped to its own installation's repositories, so any OTHER managed repo
/// whose owner differs from `root_owner_repo`'s is unreachable for private
/// data under that token — no single-token configuration can serve a fleet
/// spanning multiple owners. This is the pure filter behind the startup
/// warning in `daemon_service.rs`: given the root's `owner/repo` and every
/// other managed repo's resolved `owner/repo`, return the subset whose owner
/// differs.
#[must_use]
pub fn detect_cross_owner_repos(
    root_owner_repo: &str,
    managed_owner_repos: &[String],
) -> Vec<String> {
    let root_owner = owner_of(root_owner_repo);
    managed_owner_repos
        .iter()
        .filter(|nwo| owner_of(nwo) != root_owner)
        .cloned()
        .collect()
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

// ============================================================================
// File-based token delivery (#4458)
// ============================================================================
//
// The startup preflight and the refresh tick above both used to publish the
// minted installation token by calling `std::env::set_var("GH_TOKEN", …)`
// directly in `main.rs`. That is sound *once* at boot (nothing else is
// spawning `gh`/`git` children yet), but the refresh tick repeats it for the
// life of the process from a background tokio task — racing the `environ`
// reads every concurrent `Command::spawn` in this multithreaded runtime
// performs (~76 `Command::new("gh"|"git")` call sites across 16 files, with
// no central spawn choke point to inject through instead — see #4458's
// issue body for the full census). Concurrent `setenv`/`getenv` is undefined
// behavior on POSIX, which is exactly why `set_var` is `unsafe` as of Rust
// edition 2024.
//
// The fix below replaces the *recurring* write with a **daemon-owned
// `GH_CONFIG_DIR`** whose `hosts.yml` is rewritten atomically (write a temp
// file, then `rename` into place — atomic on the same filesystem). `gh`
// re-reads its config from disk on every invocation, so every one of those
// ~76 call sites picks up a fresh token automatically, with zero call-site
// changes and zero recurring `std::env::set_var` calls: the tick becomes a
// pure file operation. The one remaining `std::env::set_var` — pointing
// `GH_CONFIG_DIR` at this directory — fires at most once per process
// lifetime (see `main.rs`), before the tick task or any other `gh`-spawning
// task exists to race it.
//
// This was verified empirically against the `gh` CLI actually pinned in this
// environment (2.96.0): a `GH_CONFIG_DIR` whose `hosts.yml` has no sibling
// `config.yml` triggers `gh`'s one-time "multi-account migration", which
// calls `GET /user` to resolve a login name — a call a GitHub App
// installation token (not a user-authenticated credential) cannot make, so
// *every* `gh` invocation hard-fails with "failed to migrate config" instead
// of a normal 401. Writing a `config.yml` with `version: 1` up front (done by
// [`publish_github_app_token`] on first use) skips that migration path
// entirely — this is the reason a bare `hosts.yml` is not sufficient.
//
// Trade-off accepted: `GH_CONFIG_DIR` is host-global for the daemon process,
// so it also shadows the operator's own `~/.config/gh` (aliases, `gh` prefs)
// for every child the daemon spawns while a GitHub App is configured. That
// is judged acceptable here — the daemon's own `gh` calls are all
// non-interactive, alias-free API/CLI invocations — over the alternative
// (injecting `.env("GH_TOKEN", …)` from a shared store at each of the ~76
// call sites), which would preserve the operator's ambient config but cost a
// multi-file refactor for a residual, currently-dormant hazard (see #4458).

/// Directory (under the workspace root's own `.loom/`) the daemon owns for
/// GitHub-App-token delivery via `GH_CONFIG_DIR` (#4458). Mirrors the
/// existing `.loom/tokens/` convention (`tokens.rs`) — host-local, never
/// committed (see `.gitignore`).
#[must_use]
pub fn github_app_gh_config_dir(workspace_root: &Path) -> PathBuf {
    workspace_root.join(".loom").join("gh-config")
}

/// Static `config.yml` companion `hosts.yml` needs so `gh` skips its
/// one-time multi-account migration (see module doc above for why that
/// migration is fatal for a GitHub-App-only credential).
const GH_CONFIG_YAML: &str = "version: 1\ngit_protocol: https\n";

/// Build the `hosts.yml` content `gh` expects for `token` on `github.com`.
/// Split from the file I/O so it is unit-testable without touching disk
/// (mirrors [`parse_github_app_response`]). `user` is set to
/// `x-access-token` — the conventional placeholder for a GitHub App
/// installation token (not a real user account); `gh` does not validate it
/// against the API, it only uses `oauth_token` for authentication.
fn gh_hosts_yaml(token: &str) -> String {
    format!("github.com:\n    oauth_token: {token}\n    user: x-access-token\n    git_protocol: https\n")
}

/// Set `mode` on `path`, a no-op on non-unix targets (the daemon is
/// unix-only in practice, but this keeps the crate cross-platform-buildable).
fn set_private_mode(path: &Path, mode: u32) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))?;
    }
    #[cfg(not(unix))]
    {
        let _ = (path, mode);
    }
    Ok(())
}

/// Atomically publish `token` into `config_dir/hosts.yml` (write-then-rename,
/// same filesystem — no partial-file window), creating `config_dir` and its
/// static `config.yml` companion on first use. This is a **pure file
/// operation**: it never touches `std::env`, so it cannot race the
/// `environ` reads of any concurrently spawned `gh`/`git` child (#4458) —
/// the refresh tick in `main.rs` calls this in place of the old
/// `std::env::set_var("GH_TOKEN", …)`.
pub fn publish_github_app_token(config_dir: &Path, token: &str) -> std::io::Result<()> {
    std::fs::create_dir_all(config_dir)?;
    set_private_mode(config_dir, 0o700)?;

    let config_path = config_dir.join("config.yml");
    if !config_path.exists() {
        std::fs::write(&config_path, GH_CONFIG_YAML)?;
        set_private_mode(&config_path, 0o600)?;
    }

    let hosts_path = config_dir.join("hosts.yml");
    let tmp_path = config_dir.join("hosts.yml.tmp");
    std::fs::write(&tmp_path, gh_hosts_yaml(token))?;
    set_private_mode(&tmp_path, 0o600)?;
    std::fs::rename(&tmp_path, &hosts_path)?;
    Ok(())
}

// ============================================================================
// Per-owner credential delivery (#5401)
// ============================================================================
//
// The #4458 delivery above publishes exactly ONE installation token, into the
// process-global `GH_CONFIG_DIR` (`github_app_gh_config_dir`), keyed on the
// daemon's *workspace-root* owner. Every `gh`/`git` child inherits it — so a
// managed repo owned by a DIFFERENT GitHub account/org (the fleet in #5401
// spans `rjwalters/*` and `2AMLogic/*`) authenticates with a token scoped to
// the wrong installation: public reads succeed anonymously, private reads and
// all writes silently 404. An installation token cannot be widened to a
// second owner, so the fix is a SECOND (third, …) credential — one per
// distinct owner among the managed repos — each delivered through its own
// per-owner `GH_CONFIG_DIR`, and selected per child `Command` by the target
// repo's local checkout root.
//
// Selection is per-*child* (`Command::env`), never `std::env`: the daemon's
// per-repo `gh` call sites already `current_dir(root)` the managed checkout,
// so `apply_gh_config_for_root` maps that `root` to its owner's config dir and
// sets `GH_CONFIG_DIR` on that child only — which cannot race the `environ`
// reads of any concurrently spawned child the way a recurring
// `std::env::set_var` would (the exact hazard #4458 eliminated for the
// single-owner path).
//
// A single-owner fleet (the pre-#5401 common case) registers nothing here, so
// `apply_gh_config_for_root` is a total no-op — behavior is byte-identical.

/// The owner segment of an `"owner/repo"` string. Public wrapper over the
/// module-private [`owner_of`] so `daemon_service.rs` can group managed repos
/// by owner without re-implementing the split.
#[must_use]
pub fn owner_of_nwo(owner_repo: &str) -> &str {
    owner_of(owner_repo)
}

/// The per-owner `GH_CONFIG_DIR` for `owner`, under the workspace root's own
/// `.loom/` (mirrors [`github_app_gh_config_dir`], the workspace-root owner's
/// dir). Distinct subtree (`gh-config-by-owner/<owner>`) so a per-owner token
/// never clobbers the process-global one. Host-local, never committed.
#[must_use]
pub fn github_app_gh_config_dir_for_owner(workspace_root: &Path, owner: &str) -> PathBuf {
    workspace_root
        .join(".loom")
        .join("gh-config-by-owner")
        .join(owner)
}

/// Process-global map: a managed-repo checkout root -> the `GH_CONFIG_DIR`
/// carrying a token scoped to that repo's owner. Populated at startup /
/// refresh (`daemon_service.rs`) for owners OTHER than the workspace-root
/// owner; the root-owner's repos are deliberately absent so they fall through
/// to the process-global `GH_CONFIG_DIR`. Empty on a single-owner fleet.
fn owner_root_config_registry() -> &'static Mutex<HashMap<PathBuf, PathBuf>> {
    static REGISTRY: OnceLock<Mutex<HashMap<PathBuf, PathBuf>>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Best-effort canonicalization so a `root` registered at startup and the same
/// `root` handed to a per-repo `gh` call site key identically regardless of
/// symlinks / `.` / `..`. Falls back to the path as-given when it can't be
/// canonicalized (e.g. a not-yet-created path in a test).
fn normalize_registry_key(root: &Path) -> PathBuf {
    std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf())
}

/// Register that `root`'s per-repo `gh`/`git` children should carry
/// `config_dir` as their `GH_CONFIG_DIR` (#5401). Idempotent; the refresh path
/// re-registers the same mapping harmlessly.
pub fn register_root_gh_config_dir(root: &Path, config_dir: &Path) {
    if let Ok(mut map) = owner_root_config_registry().lock() {
        map.insert(normalize_registry_key(root), config_dir.to_path_buf());
    }
}

/// Drop every per-owner registration — the root-keyed (#5401), the
/// owner-slug-keyed (#5431), and the refresh-source (#6171) maps. Used by
/// tests to isolate the process-global registries between cases.
pub fn clear_owner_root_registry() {
    if let Ok(mut map) = owner_root_config_registry().lock() {
        map.clear();
    }
    if let Ok(mut map) = owner_slug_config_registry().lock() {
        map.clear();
    }
    if let Ok(mut map) = owner_refresh_registry().lock() {
        map.clear();
    }
}

/// The `GH_CONFIG_DIR` registered for `root`, or `None` when `root` is not a
/// cross-owner managed repo (the common single-owner case, or the root-owner's
/// own repos) — in which case the caller leaves the child's `GH_CONFIG_DIR`
/// untouched so it inherits the daemon's process-global one.
#[must_use]
pub fn gh_config_dir_for_root(root: &Path) -> Option<PathBuf> {
    let key = normalize_registry_key(root);
    owner_root_config_registry().lock().ok()?.get(&key).cloned()
}

/// Point `cmd`'s child `GH_CONFIG_DIR` at the credential scoped to `root`'s
/// owner, when `root` is a cross-owner managed repo (#5401). A total no-op
/// otherwise, so single-owner fleets and the root-owner's own repos are
/// byte-identical to pre-#5401. Sets the env on the *child* only, never
/// `std::env`, so it cannot race a concurrently spawned child's `environ`
/// reads.
pub fn apply_gh_config_for_root(cmd: &mut Command, root: &Path) {
    if let Some(dir) = gh_config_dir_for_root(root) {
        cmd.env("GH_CONFIG_DIR", dir);
    }
}

/// As [`apply_gh_config_for_root`] but for an **async** call site spawning a
/// [`tokio::process::Command`] (the narration sink's own forge lookups in
/// [`crate::safehouse`], #6596). Identical semantics — registered root ⇒ the
/// owner's `GH_CONFIG_DIR` on the child, everything else a total no-op — just a
/// different `Command` type, since tokio's builder is not `std`'s.
pub fn apply_gh_config_for_root_async(cmd: &mut tokio::process::Command, root: &Path) {
    if let Some(dir) = gh_config_dir_for_root(root) {
        cmd.env("GH_CONFIG_DIR", dir);
    }
}

/// As [`apply_gh_config_for_root`] but for a call site that carries an
/// `Option<&Path>` cwd (the [`crate::forge_listing`] cached-listing path): a
/// `None` cwd (the daemon's own workspace) is left on the process-global
/// `GH_CONFIG_DIR`.
pub fn apply_gh_config_for_cwd(cmd: &mut Command, cwd: Option<&Path>) {
    if let Some(root) = cwd {
        apply_gh_config_for_root(cmd, root);
    }
}

/// Process-global map: a managed-repo owner segment (e.g. `2AMLogic`) -> the
/// `GH_CONFIG_DIR` carrying a token scoped to that owner. Populated alongside
/// [`register_root_gh_config_dir`] at startup / refresh (`daemon_service.rs`)
/// for owners OTHER than the workspace-root owner; the root owner is
/// deliberately absent so its repos fall through to the process-global
/// `GH_CONFIG_DIR`. Empty on a single-owner fleet.
///
/// This complements the root-keyed [`owner_root_config_registry`] for call
/// sites that identify the target repo by an `owner/repo` slug + `--repo` flag
/// rather than by a checkout-root `current_dir` — e.g.
/// `fleet::drain::GhClaimResetter` (fleet-wide claim resets) and
/// `telemetry::visibility` (a `gh api repos/{owner}/{repo}` probe). See #5431.
fn owner_slug_config_registry() -> &'static Mutex<HashMap<String, PathBuf>> {
    static REGISTRY: OnceLock<Mutex<HashMap<String, PathBuf>>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Register that a `gh` call targeting any repo owned by `owner` (via a
/// `--repo <owner>/<repo>` flag or an `owner/repo`-in-path `gh api`, rather
/// than a checkout-root `current_dir`) should carry `config_dir` as its
/// `GH_CONFIG_DIR` (#5431). Idempotent; the refresh path re-registers the same
/// mapping harmlessly.
pub fn register_owner_gh_config_dir(owner: &str, config_dir: &Path) {
    if let Ok(mut map) = owner_slug_config_registry().lock() {
        map.insert(owner.to_string(), config_dir.to_path_buf());
    }
}

/// The `GH_CONFIG_DIR` registered for the owner segment of `owner_repo`, or
/// `None` when that owner is not a registered cross-owner managed owner (the
/// common single-owner case, or the root owner's own repos) — the slug-keyed
/// analogue of [`gh_config_dir_for_root`] (#5431).
#[must_use]
pub fn gh_config_dir_for_owner_slug(owner_repo: &str) -> Option<PathBuf> {
    let owner = owner_of(owner_repo);
    if owner.is_empty() {
        return None;
    }
    owner_slug_config_registry()
        .lock()
        .ok()?
        .get(owner)
        .cloned()
}

/// Point `cmd`'s child `GH_CONFIG_DIR` at the credential scoped to the owner of
/// `owner_repo` (an `owner/repo` slug), when that owner is a registered
/// cross-owner managed owner (#5431). A total no-op otherwise, so single-owner
/// fleets and the root owner's own repos are byte-identical. The slug-keyed
/// analogue of [`apply_gh_config_for_root`], for call sites that pass
/// `--repo <owner/repo>` (or embed it in a `gh api` path) instead of running in
/// a checkout-root `current_dir`. Sets the env on the *child* only, never
/// `std::env`.
pub fn apply_gh_config_for_owner_slug(cmd: &mut Command, owner_repo: &str) {
    if let Some(dir) = gh_config_dir_for_owner_slug(owner_repo) {
        cmd.env("GH_CONFIG_DIR", dir);
    }
}

/// One distinct non-root owner among the managed repos, with a representative
/// `owner/repo` to mint from and every managed checkout root under that owner
/// to register (#5401). Produced by [`plan_cross_owner_credentials`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CrossOwnerCredentialPlan {
    /// The owner segment (e.g. `2AMLogic`).
    pub owner: String,
    /// Any `owner/repo` under `owner` — the mint key. Installation resolution
    /// is per-owner, so any repo of the owner resolves the same installation.
    pub representative_owner_repo: String,
    /// Every managed checkout root under `owner`, to register against the
    /// owner's `GH_CONFIG_DIR`.
    pub roots: Vec<PathBuf>,
}

/// Group `managed` (each a `(checkout_root, "owner/repo")`) by owner, dropping
/// the `root_owner` (whose repos use the process-global credential) and any
/// malformed entry, into one [`CrossOwnerCredentialPlan`] per distinct other
/// owner (#5401). Deterministic: owners in first-seen order, roots in input
/// order — so the startup log and the tests are stable.
#[must_use]
pub fn plan_cross_owner_credentials(
    root_owner: &str,
    managed: &[(PathBuf, String)],
) -> Vec<CrossOwnerCredentialPlan> {
    let mut order: Vec<String> = Vec::new();
    let mut by_owner: HashMap<String, CrossOwnerCredentialPlan> = HashMap::new();
    for (root, owner_repo) in managed {
        // Skip anything that didn't split into a real owner/repo, and the
        // root owner (its repos ride the process-global credential).
        if !owner_repo.contains('/') {
            continue;
        }
        let owner = owner_of(owner_repo);
        if owner.is_empty() || owner == root_owner {
            continue;
        }
        match by_owner.get_mut(owner) {
            Some(plan) => {
                if !plan.roots.contains(root) {
                    plan.roots.push(root.clone());
                }
            }
            None => {
                order.push(owner.to_string());
                by_owner.insert(
                    owner.to_string(),
                    CrossOwnerCredentialPlan {
                        owner: owner.to_string(),
                        representative_owner_repo: owner_repo.clone(),
                        roots: vec![root.clone()],
                    },
                );
            }
        }
    }
    order
        .into_iter()
        .filter_map(|o| by_owner.remove(&o))
        .collect()
}

// ============================================================================
// Hot-apply for a newly registered workspace (#6171)
// ============================================================================
//
// The per-owner credentials established above (startup, `plan_cross_owner_
// credentials`) and kept fresh by `daemon_service.rs`'s periodic refresh tick
// are both scoped to whatever the managed-repo set looked like *at the time
// each plan was built*. A workspace registered against a running daemon
// (`loom-daemon workspace add`) hot-applies the registry file (the next tick
// sees the new workspace), but neither of the above re-derives the credential
// plan — so a repo added to an owner this daemon already manages 404s on
// every scan until a full restart re-runs the startup preflight from
// scratch.
//
// The fix is two-part:
//
// 1. [`force_refresh_owner_credential`] — called by `forge_listing.rs` when a
//    *registered* workspace's listing 404s — force-mints (bypasses the shell
//    helper's own cache, which is what makes a mint's scope stale in the
//    first place) a fresh token for that repo's owner and registers it, so
//    the caller can retry immediately rather than wait for a restart.
// 2. [`register_owner_refresh_source`] / [`owner_refresh_sources`] — a
//    process-global registry the periodic per-owner refresh tick now reads
//    fresh every tick (`daemon_service.rs`) instead of a `Vec` frozen at
//    startup, so a credential established dynamically via (1) is kept fresh
//    going forward exactly like one established at startup.

/// The daemon's own primary workspace root (#6171) — the anchor for
/// `.loom/scripts/lib/github-app-token.sh` and `.loom/gh-config-by-owner/*`
/// regardless of which managed repo's checkout root a forced-refresh retry is
/// triggered from. Set once, at daemon startup
/// ([`register_primary_workspace_root`]), before any forge call that could
/// need [`force_refresh_owner_credential`] — mirrors the other process-global
/// registries in this module ([`owner_root_config_registry`],
/// [`owner_slug_config_registry`]).
static PRIMARY_WORKSPACE_ROOT: OnceLock<PathBuf> = OnceLock::new();

/// Record the daemon's own primary workspace root. `get_or_init` makes
/// repeated calls harmless (there is only ever one call site in production,
/// `daemon_service.rs`) rather than panicking on a second call.
pub fn register_primary_workspace_root(root: &Path) {
    PRIMARY_WORKSPACE_ROOT.get_or_init(|| root.to_path_buf());
}

/// Process-global registry of every per-owner credential the periodic refresh
/// tick should keep fresh (#6171): owner -> (representative `owner/repo` to
/// mint from, its `GH_CONFIG_DIR`). Distinct from
/// [`owner_slug_config_registry`] (which maps owner -> dir for `gh` call-site
/// selection) because the refresh tick additionally needs a mint key per
/// owner; kept as its own map rather than overloading that one so a caller
/// that only wants the dir mapping is unaffected.
///
/// Seeded at startup from every [`CrossOwnerCredentialPlan`]
/// (`daemon_service.rs`) and grown at runtime by
/// [`force_refresh_owner_credential`] whenever a 404 reveals a credential
/// that was never established at startup — a brand-new owner, or an owner
/// whose only known root(s) at startup did not include the one that just
/// 404'd. Reading this registry fresh every tick (rather than a `Vec` closed
/// over at task-spawn time) is what makes the periodic tick pick up entries
/// added after startup without a restart.
fn owner_refresh_registry() -> &'static Mutex<HashMap<String, (String, PathBuf)>> {
    static REGISTRY: OnceLock<Mutex<HashMap<String, (String, PathBuf)>>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Register (or update) the periodic refresh source for `owner_repo`'s owner:
/// mint from `owner_repo`, publish into `config_dir`. Idempotent; a later
/// call for the same owner replaces its mint key / dir (harmless — both
/// still resolve to the same owner's installation).
pub fn register_owner_refresh_source(owner_repo: &str, config_dir: &Path) {
    let owner = owner_of(owner_repo).to_string();
    if owner.is_empty() {
        return;
    }
    if let Ok(mut map) = owner_refresh_registry().lock() {
        map.insert(owner, (owner_repo.to_string(), config_dir.to_path_buf()));
    }
}

/// Snapshot of every registered periodic refresh source, for the tick loop in
/// `daemon_service.rs` to iterate — empty on a single-owner fleet that has
/// never hit the #6171 recovery path, exactly like the pre-#6171 `Vec`.
#[must_use]
pub fn owner_refresh_sources() -> Vec<(String, PathBuf)> {
    owner_refresh_registry()
        .lock()
        .map(|m| m.values().cloned().collect())
        .unwrap_or_default()
}

/// The #6171 decision + registration logic behind [`force_refresh_owner_credential`],
/// split out so it is unit-testable with an injected [`GithubAppMinter`] —
/// mirrors [`run_with_github_app`]'s injectable-minter split. Force-mints a
/// fresh token for `repo_root`'s owner via `minter` and, on success,
/// publishes + registers it (so both the immediate retry and every later call
/// against `repo_root` — and the periodic refresh tick — pick it up).
///
/// Returns `true` iff a fresh credential is now published and the caller
/// should retry its failed `gh` call; `false` when there is nothing this
/// daemon can do (`repo_root`'s remote doesn't resolve to an `owner/repo`,
/// the app isn't configured, or the mint/publish itself failed) — the caller
/// then treats the original failure as real.
#[must_use]
pub fn force_refresh_owner_credential_with(
    workspace_root: &Path,
    repo_root: &Path,
    minter: &dyn GithubAppMinter,
) -> bool {
    let Some(owner_repo) = nwo_from_git_remote(repo_root) else {
        return false;
    };
    let owner = owner_of(&owner_repo).to_string();
    let source = credential_source_for_owner(&owner_repo);
    match minter.mint_forced(&owner_repo) {
        GithubAppOutcome::Minted {
            token,
            installation_id,
            app_id,
            ..
        } => {
            let owner_dir = github_app_gh_config_dir_for_owner(workspace_root, &owner);
            match publish_github_app_token(&owner_dir, &token) {
                Ok(()) => {
                    register_root_gh_config_dir(repo_root, &owner_dir);
                    register_owner_gh_config_dir(&owner, &owner_dir);
                    register_owner_refresh_source(&owner_repo, &owner_dir);
                    record_forge_credential_success(&source);
                    log::info!(
                        "credential_preflight: forced per-owner github-app refresh for {owner} \
                         ({owner_repo}) after a registered-workspace scan failure — retrying (app \
                         {app_id} installation {installation_id}) — #6171"
                    );
                    true
                }
                Err(e) => {
                    log::warn!(
                        "credential_preflight: forced refresh minted a token for {owner} but \
                         could not publish it to {} ({e}) — #6171",
                        owner_dir.display()
                    );
                    false
                }
            }
        }
        GithubAppOutcome::NotConfigured => false,
        GithubAppOutcome::Error(reason) => {
            record_forge_credential_failure(&source, &reason);
            log::warn!(
                "credential_preflight: forced per-owner github-app refresh for {owner_repo} \
                 failed ({reason}) — #6171"
            );
            false
        }
    }
}

/// Production entry point for the #6171 404-recovery path: resolves the real
/// minter (`github-app-token.sh`) from the registered
/// [`PRIMARY_WORKSPACE_ROOT`] and delegates to
/// [`force_refresh_owner_credential_with`]. `false` (a no-op) when no primary
/// root has been registered yet, or no GitHub App is configured on this host
/// — same "no feature exists" posture as every other call site in this
/// module.
#[must_use]
pub fn force_refresh_owner_credential(repo_root: &Path) -> bool {
    let Some(workspace_root) = PRIMARY_WORKSPACE_ROOT.get() else {
        return false;
    };
    let Some(script_path) = resolve_github_app_script(workspace_root) else {
        return false;
    };
    let minter = RealGithubAppMinter {
        script_path,
        cwd: workspace_root.clone(),
    };
    force_refresh_owner_credential_with(workspace_root, repo_root, &minter)
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
    fn parse_github_app_response_ok_with_empty_installation_id_is_an_error() {
        // #5401: a successful mint with a blank installation_id is a defect
        // (the shell helper's subshell-scoping bug that motivated this
        // issue), not a legitimate `Minted` outcome whose fingerprint would
        // silently render as "installation " with nothing after it.
        let outcome = parse_github_app_response(
            r#"{"status":"ok","token":"ghs_abc123","installation_id":"","app_id":"42","expires_at":"2099-01-01T00:00:00Z"}"#,
        );
        assert!(matches!(outcome, GithubAppOutcome::Error(_)));
    }

    #[test]
    fn parse_github_app_response_ok_with_missing_installation_id_field_is_an_error() {
        let outcome = parse_github_app_response(
            r#"{"status":"ok","token":"ghs_abc123","app_id":"42","expires_at":"2099-01-01T00:00:00Z"}"#,
        );
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
    fn detect_cross_owner_repos_flags_only_differing_owners() {
        let managed = vec![
            "rjwalters/anvil".to_string(),
            "2AMLogic/marketing".to_string(),
            "2AMLogic/2am".to_string(),
            "rjwalters/safehouse".to_string(),
        ];
        let flagged = detect_cross_owner_repos("rjwalters/loom", &managed);
        assert_eq!(flagged, vec!["2AMLogic/marketing".to_string(), "2AMLogic/2am".to_string()]);
    }

    #[test]
    fn detect_cross_owner_repos_single_owner_fleet_is_a_no_op() {
        // #5401 edge case: a fleet with managed repos under a single owner
        // (the common case pre-#5401) must see zero flagged repos.
        let managed = vec![
            "rjwalters/anvil".to_string(),
            "rjwalters/safehouse".to_string(),
        ];
        assert!(detect_cross_owner_repos("rjwalters/loom", &managed).is_empty());
    }

    #[test]
    fn detect_cross_owner_repos_empty_managed_list_is_a_no_op() {
        assert!(detect_cross_owner_repos("rjwalters/loom", &[]).is_empty());
    }

    // ------------------------------------------------------------------
    // Per-owner credential delivery (#5401)
    // ------------------------------------------------------------------

    #[test]
    fn owner_of_nwo_is_the_public_owner_split() {
        assert_eq!(owner_of_nwo("2AMLogic/marketing"), "2AMLogic");
        assert_eq!(owner_of_nwo("rjwalters/loom"), "rjwalters");
    }

    #[test]
    fn github_app_gh_config_dir_for_owner_lives_under_dot_loom_by_owner() {
        let root = Path::new("/tmp/ws");
        assert_eq!(
            github_app_gh_config_dir_for_owner(root, "2AMLogic"),
            root.join(".loom")
                .join("gh-config-by-owner")
                .join("2AMLogic")
        );
        // Distinct from the process-global (workspace-root owner) dir, so a
        // per-owner token can never clobber it.
        assert_ne!(
            github_app_gh_config_dir_for_owner(root, "2AMLogic"),
            github_app_gh_config_dir(root)
        );
    }

    #[test]
    fn plan_cross_owner_credentials_groups_distinct_non_root_owners() {
        let managed = vec![
            (PathBuf::from("/ws/loom"), "rjwalters/loom".to_string()),
            (PathBuf::from("/ws/marketing"), "2AMLogic/marketing".to_string()),
            (PathBuf::from("/ws/2am"), "2AMLogic/2am".to_string()),
            (PathBuf::from("/ws/anvil"), "rjwalters/anvil".to_string()),
        ];
        let plans = plan_cross_owner_credentials("rjwalters", &managed);
        // Only the one non-root owner (2AMLogic), collapsing both its repos.
        assert_eq!(plans.len(), 1);
        assert_eq!(plans[0].owner, "2AMLogic");
        // First-seen repo of that owner is the representative mint key.
        assert_eq!(plans[0].representative_owner_repo, "2AMLogic/marketing");
        assert_eq!(plans[0].roots, vec![PathBuf::from("/ws/marketing"), PathBuf::from("/ws/2am")]);
    }

    #[test]
    fn plan_cross_owner_credentials_single_owner_fleet_is_empty() {
        let managed = vec![
            (PathBuf::from("/ws/loom"), "rjwalters/loom".to_string()),
            (PathBuf::from("/ws/anvil"), "rjwalters/anvil".to_string()),
        ];
        assert!(plan_cross_owner_credentials("rjwalters", &managed).is_empty());
    }

    #[test]
    fn plan_cross_owner_credentials_skips_malformed_entries() {
        let managed = vec![
            (PathBuf::from("/ws/loom"), "rjwalters/loom".to_string()),
            (PathBuf::from("/ws/junk"), "no-slash".to_string()),
            (PathBuf::from("/ws/marketing"), "2AMLogic/marketing".to_string()),
        ];
        let plans = plan_cross_owner_credentials("rjwalters", &managed);
        assert_eq!(plans.len(), 1);
        assert_eq!(plans[0].owner, "2AMLogic");
        assert_eq!(plans[0].roots, vec![PathBuf::from("/ws/marketing")]);
    }

    #[test]
    fn plan_cross_owner_credentials_is_deterministic_in_first_seen_order() {
        let managed = vec![
            (PathBuf::from("/ws/beta"), "beta/one".to_string()),
            (PathBuf::from("/ws/alpha"), "alpha/one".to_string()),
            (PathBuf::from("/ws/beta2"), "beta/two".to_string()),
        ];
        let plans = plan_cross_owner_credentials("root", &managed);
        // beta seen before alpha, so beta comes first regardless of alpha's
        // lexical precedence.
        assert_eq!(
            plans.iter().map(|p| p.owner.as_str()).collect::<Vec<_>>(),
            vec!["beta", "alpha"]
        );
    }

    #[test]
    #[serial_test::serial]
    fn apply_gh_config_for_root_sets_env_only_for_registered_roots() {
        clear_owner_root_registry();
        let dir = tempfile::tempdir().unwrap();
        let registered = dir.path().join("marketing");
        let unregistered = dir.path().join("loom");
        std::fs::create_dir_all(&registered).unwrap();
        std::fs::create_dir_all(&unregistered).unwrap();
        let owner_dir = dir.path().join(".loom/gh-config-by-owner/2AMLogic");

        register_root_gh_config_dir(&registered, &owner_dir);

        // A registered (cross-owner) root gets GH_CONFIG_DIR set on the child.
        let mut cmd = Command::new("true");
        apply_gh_config_for_root(&mut cmd, &registered);
        let has_env = cmd.get_envs().any(|(k, v)| {
            k == "GH_CONFIG_DIR" && v == Some(std::ffi::OsStr::new(owner_dir.as_os_str()))
        });
        assert!(has_env, "registered root should carry the owner's GH_CONFIG_DIR");

        // An unregistered (root-owner / single-owner) root is a no-op — the
        // child inherits the process-global GH_CONFIG_DIR untouched.
        let mut cmd2 = Command::new("true");
        apply_gh_config_for_root(&mut cmd2, &unregistered);
        assert!(
            cmd2.get_envs().all(|(k, _)| k != "GH_CONFIG_DIR"),
            "unregistered root must not set GH_CONFIG_DIR on the child"
        );

        clear_owner_root_registry();
    }

    #[test]
    #[serial_test::serial]
    fn apply_gh_config_for_root_async_matches_the_sync_helper() {
        // #6596: the narration sink spawns `gh` through tokio, so it needs the
        // same registered-root ⇒ owner-credential mapping the sync call sites
        // get — and the same no-op for every other root.
        clear_owner_root_registry();
        let dir = tempfile::tempdir().unwrap();
        let registered = dir.path().join("product");
        let unregistered = dir.path().join("loom");
        std::fs::create_dir_all(&registered).unwrap();
        std::fs::create_dir_all(&unregistered).unwrap();
        let owner_dir = dir.path().join(".loom/gh-config-by-owner/2AMLogic");

        register_root_gh_config_dir(&registered, &owner_dir);

        let mut cmd = tokio::process::Command::new("true");
        apply_gh_config_for_root_async(&mut cmd, &registered);
        let has_env = cmd.as_std().get_envs().any(|(k, v)| {
            k == "GH_CONFIG_DIR" && v == Some(std::ffi::OsStr::new(owner_dir.as_os_str()))
        });
        assert!(has_env, "registered root should carry the owner's GH_CONFIG_DIR");

        let mut cmd2 = tokio::process::Command::new("true");
        apply_gh_config_for_root_async(&mut cmd2, &unregistered);
        assert!(
            cmd2.as_std().get_envs().all(|(k, _)| k != "GH_CONFIG_DIR"),
            "unregistered root must not set GH_CONFIG_DIR on the child"
        );

        clear_owner_root_registry();
    }

    #[test]
    #[serial_test::serial]
    fn apply_gh_config_for_cwd_none_is_a_no_op() {
        clear_owner_root_registry();
        let mut cmd = Command::new("true");
        apply_gh_config_for_cwd(&mut cmd, None);
        assert!(cmd.get_envs().all(|(k, _)| k != "GH_CONFIG_DIR"));
        clear_owner_root_registry();
    }

    #[test]
    #[serial_test::serial]
    fn apply_gh_config_for_owner_slug_sets_env_only_for_registered_owners() {
        clear_owner_root_registry();
        let dir = tempfile::tempdir().unwrap();
        let owner_dir = dir.path().join(".loom/gh-config-by-owner/2AMLogic");

        register_owner_gh_config_dir("2AMLogic", &owner_dir);

        // A slug under a registered (cross-owner) owner gets GH_CONFIG_DIR set,
        // regardless of which repo under that owner is named.
        let mut cmd = Command::new("true");
        apply_gh_config_for_owner_slug(&mut cmd, "2AMLogic/klayout-tools");
        let has_env = cmd.get_envs().any(|(k, v)| {
            k == "GH_CONFIG_DIR" && v == Some(std::ffi::OsStr::new(owner_dir.as_os_str()))
        });
        assert!(
            has_env,
            "a slug under a registered owner should carry that owner's GH_CONFIG_DIR"
        );

        // A slug under an unregistered (root-owner / single-owner) owner is a
        // no-op — the child inherits the process-global GH_CONFIG_DIR untouched.
        let mut cmd2 = Command::new("true");
        apply_gh_config_for_owner_slug(&mut cmd2, "rjwalters/loom");
        assert!(
            cmd2.get_envs().all(|(k, _)| k != "GH_CONFIG_DIR"),
            "a slug under an unregistered owner must not set GH_CONFIG_DIR on the child"
        );

        clear_owner_root_registry();
    }

    #[test]
    #[serial_test::serial]
    fn apply_gh_config_for_owner_slug_is_a_no_op_for_a_malformed_or_ownerless_slug() {
        clear_owner_root_registry();
        register_owner_gh_config_dir("2AMLogic", std::path::Path::new("/tmp/whatever"));

        // No `/` at all -> no owner segment -> never matches -> no-op.
        let mut cmd = Command::new("true");
        apply_gh_config_for_owner_slug(&mut cmd, "not-a-slug");
        assert!(cmd.get_envs().all(|(k, _)| k != "GH_CONFIG_DIR"));

        // Empty string -> no-op.
        let mut cmd2 = Command::new("true");
        apply_gh_config_for_owner_slug(&mut cmd2, "");
        assert!(cmd2.get_envs().all(|(k, _)| k != "GH_CONFIG_DIR"));

        clear_owner_root_registry();
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

    #[test]
    fn github_app_gh_config_dir_lives_under_dot_loom() {
        let root = Path::new("/tmp/some-workspace");
        assert_eq!(github_app_gh_config_dir(root), root.join(".loom").join("gh-config"));
    }

    #[test]
    fn gh_hosts_yaml_embeds_token_and_skips_migration_fields() {
        // #4458: the format must include `oauth_token` (auth) and
        // `git_protocol` (gh reads this without a network call); `user` is
        // an unvalidated placeholder for a GitHub App token.
        let yaml = gh_hosts_yaml("ghs_example_token");
        assert!(yaml.contains("oauth_token: ghs_example_token"));
        assert!(yaml.contains("user: x-access-token"));
        assert!(yaml.contains("git_protocol: https"));
        assert!(yaml.starts_with("github.com:\n"));
    }

    #[test]
    fn publish_github_app_token_writes_hosts_and_config_atomically() {
        let dir = tempfile::tempdir().unwrap();
        let config_dir = dir.path().join("gh-config");

        publish_github_app_token(&config_dir, "ghs_first").unwrap();

        let hosts = std::fs::read_to_string(config_dir.join("hosts.yml")).unwrap();
        assert!(hosts.contains("oauth_token: ghs_first"));
        let config = std::fs::read_to_string(config_dir.join("config.yml")).unwrap();
        assert!(config.contains("version: 1"));

        // No leftover temp file after a successful publish (rename consumed it).
        assert!(!config_dir.join("hosts.yml.tmp").exists());
    }

    #[test]
    fn publish_github_app_token_rotation_overwrites_hosts_but_not_config() {
        let dir = tempfile::tempdir().unwrap();
        let config_dir = dir.path().join("gh-config");

        publish_github_app_token(&config_dir, "ghs_old").unwrap();
        let config_before = std::fs::read_to_string(config_dir.join("config.yml")).unwrap();

        // Simulate a real #4430 rotation: a second publish with a new value
        // (this is what the refresh tick calls in place of the old
        // `std::env::set_var("GH_TOKEN", …)` — a pure file rewrite, no env
        // mutation anywhere in this path).
        publish_github_app_token(&config_dir, "ghs_new").unwrap();

        let hosts_after = std::fs::read_to_string(config_dir.join("hosts.yml")).unwrap();
        assert!(hosts_after.contains("oauth_token: ghs_new"));
        assert!(!hosts_after.contains("ghs_old"));

        // `config.yml` is written once and never rewritten on rotation.
        let config_after = std::fs::read_to_string(config_dir.join("config.yml")).unwrap();
        assert_eq!(config_before, config_after);
    }

    #[test]
    #[cfg(unix)]
    fn publish_github_app_token_sets_private_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let config_dir = dir.path().join("gh-config");
        publish_github_app_token(&config_dir, "ghs_secret").unwrap();

        let hosts_mode = std::fs::metadata(config_dir.join("hosts.yml"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(hosts_mode, 0o600);

        let dir_mode = std::fs::metadata(&config_dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(dir_mode, 0o700);
    }

    // ========================================================================
    // #5630: configurable mint timeout, retry-once, credential-staleness window
    // ========================================================================

    /// The retry policy runs with a zero delay in tests so a retry costs no
    /// wall-clock time — the delay is a production tuning knob, not behavior
    /// under test.
    const NO_DELAY: Duration = Duration::from_millis(0);

    #[test]
    fn mint_with_retry_returns_first_success_without_retrying() {
        let mut calls = 0u32;
        let outcome = mint_with_retry(GITHUB_APP_MINT_ATTEMPTS, NO_DELAY, |_| {
            calls += 1;
            Ok(r#"{"status":"ok","token":"ghs_x","installation_id":"7","app_id":"1","expires_at":"z"}"#
                .to_string())
        });
        assert_eq!(calls, 1, "a successful first attempt must not be retried");
        assert!(matches!(outcome, GithubAppOutcome::Minted { .. }));
    }

    #[test]
    fn mint_with_retry_retries_once_after_a_transport_failure_and_succeeds() {
        // The exact #5630 shape: attempt 1 times out under host saturation,
        // attempt 2 (a moment later) completes normally.
        let mut calls = 0u32;
        let outcome = mint_with_retry(GITHUB_APP_MINT_ATTEMPTS, NO_DELAY, |attempt| {
            calls += 1;
            if attempt == 1 {
                Err("`bash` timed out after 90s".to_string())
            } else {
                Ok(
                    r#"{"status":"ok","token":"ghs_y","installation_id":"9","app_id":"1","expires_at":"z"}"#
                        .to_string(),
                )
            }
        });
        assert_eq!(calls, 2, "a transport failure must be retried once");
        assert!(
            matches!(outcome, GithubAppOutcome::Minted { ref installation_id, .. } if installation_id == "9"),
            "the retry's answer is the one that counts, got {outcome:?}"
        );
    }

    #[test]
    fn mint_with_retry_gives_up_after_the_attempt_budget_and_names_it() {
        let mut calls = 0u32;
        let outcome = mint_with_retry(GITHUB_APP_MINT_ATTEMPTS, NO_DELAY, |_| {
            calls += 1;
            Err("`bash` timed out after 90s".to_string())
        });
        assert_eq!(calls, GITHUB_APP_MINT_ATTEMPTS);
        match outcome {
            GithubAppOutcome::Error(reason) => {
                assert!(reason.contains("2 attempt(s)"), "reason should name the budget: {reason}");
                assert!(reason.contains("timed out"), "reason should carry the cause: {reason}");
            }
            other => panic!("expected Error, got {other:?}"),
        }
    }

    #[test]
    fn mint_with_retry_does_not_retry_a_parsed_error_envelope() {
        // A `{"status":"error"}` answer is deterministic (bad key, app not
        // installed) — re-running only doubles the latency for the same result.
        let mut calls = 0u32;
        let outcome = mint_with_retry(GITHUB_APP_MINT_ATTEMPTS, NO_DELAY, |_| {
            calls += 1;
            Ok(r#"{"status":"error","message":"private key not readable"}"#.to_string())
        });
        assert_eq!(calls, 1, "a parsed error envelope must not be retried");
        assert_eq!(outcome, GithubAppOutcome::Error("private key not readable".to_string()));
    }

    #[test]
    fn mint_with_retry_floors_the_attempt_budget_at_one() {
        let mut calls = 0u32;
        let _ = mint_with_retry(0, NO_DELAY, |_| {
            calls += 1;
            Err("boom".to_string())
        });
        assert_eq!(calls, 1, "a zero budget must still make one attempt");
    }

    // Every test that *reads* `resolve_github_app_mint_timeout` shares the
    // `github_app_mint_timeout_env` serial key with the one that *sets*
    // `LOOM_GITHUB_APP_MINT_TIMEOUT_SECS`. The env is process-wide, so without
    // it a "config wins" assertion can observe the env test's `120` and fail
    // spuriously (the #4385 hazard). A **named** key rather than the bare
    // `#[serial]` global one so these fast tests do not queue behind the
    // crate's minute-long subprocess fixtures.

    #[test]
    #[serial_test::serial(github_app_mint_timeout_env)]
    fn mint_timeout_default_is_the_raised_budget_when_unconfigured() {
        std::env::remove_var(GITHUB_APP_MINT_TIMEOUT_ENV);
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(resolve_github_app_mint_timeout(dir.path()), DEFAULT_GITHUB_APP_MINT_TIMEOUT);
        assert!(
            DEFAULT_GITHUB_APP_MINT_TIMEOUT > Duration::from_secs(20),
            "the whole point of #5630 is that the default is no longer the 20s that \
             a saturated host blows through"
        );
    }

    #[test]
    #[serial_test::serial(github_app_mint_timeout_env)]
    fn mint_timeout_reads_config_when_env_is_unset() {
        std::env::remove_var(GITHUB_APP_MINT_TIMEOUT_ENV);
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".loom")).unwrap();
        std::fs::write(
            dir.path().join(".loom/config.json"),
            r#"{"forge":{"githubApp":{"mintTimeoutSeconds":45}}}"#,
        )
        .unwrap();
        assert_eq!(resolve_github_app_mint_timeout(dir.path()), Duration::from_secs(45));
    }

    #[test]
    #[serial_test::serial(github_app_mint_timeout_env)]
    fn mint_timeout_ignores_a_zero_config_value() {
        std::env::remove_var(GITHUB_APP_MINT_TIMEOUT_ENV);
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".loom")).unwrap();
        std::fs::write(
            dir.path().join(".loom/config.json"),
            r#"{"forge":{"githubApp":{"mintTimeoutSeconds":0}}}"#,
        )
        .unwrap();
        assert_eq!(resolve_github_app_mint_timeout(dir.path()), DEFAULT_GITHUB_APP_MINT_TIMEOUT);
    }

    #[test]
    #[serial_test::serial(github_app_mint_timeout_env)]
    fn mint_timeout_env_overrides_config() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".loom")).unwrap();
        std::fs::write(
            dir.path().join(".loom/config.json"),
            r#"{"forge":{"githubApp":{"mintTimeoutSeconds":45}}}"#,
        )
        .unwrap();
        std::env::set_var(GITHUB_APP_MINT_TIMEOUT_ENV, "120");
        let resolved = resolve_github_app_mint_timeout(dir.path());
        std::env::remove_var(GITHUB_APP_MINT_TIMEOUT_ENV);
        assert_eq!(resolved, Duration::from_secs(120));
    }

    #[test]
    fn credential_stale_hold_is_false_without_a_streak() {
        assert!(!credential_stale_hold(None, Instant::now(), Duration::from_secs(600)));
    }

    #[test]
    fn credential_stale_hold_is_true_inside_the_grace_window() {
        let now = Instant::now();
        let first = now - Duration::from_secs(60);
        assert!(credential_stale_hold(Some(first), now, Duration::from_secs(600)));
    }

    #[test]
    fn credential_stale_hold_expires_at_the_grace_bound() {
        // Bounded on purpose: a credential broken for the whole window is a real
        // operator problem, and the pre-#5630 fail-safe must reassert rather
        // than freeze every repo's verdict forever.
        let now = Instant::now();
        let first = now - Duration::from_secs(601);
        assert!(!credential_stale_hold(Some(first), now, Duration::from_secs(600)));
    }

    #[test]
    fn credential_stale_hold_window_is_anchored_to_the_first_failure() {
        // A streak that keeps failing must NOT keep renewing its own window:
        // the anchor is the first failure, so an old streak has already expired
        // no matter how recently it last failed.
        let now = Instant::now();
        let first = now - Duration::from_secs(3600);
        assert!(!credential_stale_hold(Some(first), now, Duration::from_secs(600)));
    }

    /// The default grace window used by the local-instance streak tests. A
    /// plain value, so these tests never read the `LOOM_FORGE_CREDENTIAL_STALE_\
    /// GRACE_SECS` env either.
    const TEST_GRACE: Duration = Duration::from_secs(600);

    #[test]
    fn recording_a_failure_marks_the_credential_stale_until_a_success() {
        let now = Instant::now();
        let mut streaks = CredentialStreaks::default();
        assert!(!streaks.is_stale_at(now, TEST_GRACE), "a fresh tracker is not stale");
        assert!(streaks.summary_at(now, TEST_GRACE).is_none());

        streaks.record_failure_at(CREDENTIAL_SOURCE_PRIMARY, "`bash` timed out after 90s", now);
        assert!(streaks.is_stale_at(now, TEST_GRACE), "a refresh failure opens the stale window");
        let summary = streaks
            .summary_at(now, TEST_GRACE)
            .expect("a stale window has a summary");
        assert!(summary.contains("1 consecutive"), "{summary}");
        assert!(summary.contains("timed out"), "{summary}");
        assert!(summary.contains(CREDENTIAL_SOURCE_PRIMARY), "{summary}");

        streaks.record_failure_at(CREDENTIAL_SOURCE_PRIMARY, "`bash` timed out after 90s", now);
        let summary = streaks.summary_at(now, TEST_GRACE).expect("still stale");
        assert!(summary.contains("2 consecutive"), "{summary}");

        streaks.record_success(CREDENTIAL_SOURCE_PRIMARY);
        assert!(
            !streaks.is_stale_at(now, TEST_GRACE),
            "a successful mint ends the streak immediately — no lingering hold"
        );
        assert!(streaks.summary_at(now, TEST_GRACE).is_none());
    }

    #[test]
    fn one_sources_success_does_not_clear_another_sources_streak() {
        // The daemon runs several independent refresh loops (primary #4430 plus
        // one per cross-owner credential #5401). A single shared slot would let
        // a healthy loop's success wipe a failing loop's streak on its very next
        // tick — dissolving the hold exactly when it is needed.
        let now = Instant::now();
        let mut streaks = CredentialStreaks::default();
        let owner_a = credential_source_for_owner("2AMLogic/2am");
        let owner_b = credential_source_for_owner("rjwalters/loom");

        streaks.record_failure_at(&owner_a, "`bash` timed out after 90s", now);
        streaks.record_success(&owner_b);
        assert!(
            streaks.is_stale_at(now, TEST_GRACE),
            "owner B's healthy tick must not clear owner A's streak"
        );
        let summary = streaks.summary_at(now, TEST_GRACE).expect("still stale");
        assert!(
            summary.contains("2AMLogic/2am"),
            "the summary must name the failing source: {summary}"
        );

        streaks.record_success(&owner_a);
        assert!(
            !streaks.is_stale_at(now, TEST_GRACE),
            "clearing the only failing source ends the hold"
        );
    }

    #[test]
    fn a_streak_expires_at_its_own_grace_bound_not_another_sources() {
        // Each source's window is anchored to *its own* first failure, so an
        // old, expired streak stops holding even while a newer one still does.
        let now = Instant::now();
        let mut streaks = CredentialStreaks::default();
        let old = credential_source_for_owner("old/owner");
        streaks.record_failure_at(&old, "long dead", now - TEST_GRACE - Duration::from_secs(1));
        assert!(
            !streaks.is_stale_at(now, TEST_GRACE),
            "a streak past its grace window no longer holds — the fail-safe reasserts"
        );
        assert!(streaks.summary_at(now, TEST_GRACE).is_none());

        streaks.record_failure_at(CREDENTIAL_SOURCE_PRIMARY, "just now", now);
        assert!(
            streaks.is_stale_at(now, TEST_GRACE),
            "a fresh streak on another source still holds"
        );
        let summary = streaks.summary_at(now, TEST_GRACE).expect("stale");
        assert!(
            summary.starts_with(CREDENTIAL_SOURCE_PRIMARY),
            "the expired streak must not be described: {summary}"
        );
        assert!(
            !summary.contains("other failing credential source"),
            "an expired streak is not counted among the active ones: {summary}"
        );
    }

    #[test]
    fn the_summary_describes_the_oldest_source_and_counts_the_rest() {
        let now = Instant::now();
        let mut streaks = CredentialStreaks::default();
        streaks.record_failure_at(
            CREDENTIAL_SOURCE_PRIMARY,
            "first failure",
            now - Duration::from_secs(60),
        );
        let owner = credential_source_for_owner("2AMLogic/2am");
        streaks.record_failure_at(&owner, "second failure", now);

        let summary = streaks.summary_at(now, TEST_GRACE).expect("stale");
        assert!(
            summary.starts_with(CREDENTIAL_SOURCE_PRIMARY),
            "the oldest streak leads (its grace expires first): {summary}"
        );
        assert!(summary.contains("+1 other failing credential source(s)"), "{summary}");
    }

    #[test]
    #[serial_test::serial(forge_credential_grace_env)]
    fn a_tiny_grace_window_restores_pre_5630_behavior() {
        // The knob exists so an operator can dial the hold back to (effectively)
        // the pre-#5630 fail-safe. Assert the knob is *read*, with no global
        // streak state involved.
        std::env::set_var(FORGE_CREDENTIAL_STALE_GRACE_ENV, "1");
        let grace = resolve_forge_credential_stale_grace();
        std::env::remove_var(FORGE_CREDENTIAL_STALE_GRACE_ENV);
        assert_eq!(grace, Duration::from_secs(1));
        assert_eq!(
            resolve_forge_credential_stale_grace(),
            DEFAULT_FORGE_CREDENTIAL_STALE_GRACE,
            "unset env falls back to the default window"
        );
        // A one-second grace makes even a just-recorded failure stop holding
        // almost immediately — which is what "restores pre-#5630 behavior"
        // means in practice.
        let now = Instant::now();
        let mut streaks = CredentialStreaks::default();
        streaks.record_failure_at(CREDENTIAL_SOURCE_PRIMARY, "boom", now - Duration::from_secs(2));
        assert!(!streaks.is_stale_at(now, Duration::from_secs(1)));
    }

    // ========================================================================
    // Hot-apply for a newly registered workspace (#6171)
    // ========================================================================

    /// `mint_forced` has no override on `FixedMinter` — it must fall through
    /// to the trait's default (`self.mint(owner_repo)`), so a test double
    /// written before #6171 (like this one, which only implements `mint`)
    /// keeps working unchanged.
    #[test]
    fn mint_forced_default_delegates_to_mint() {
        let minter = FixedMinter(GithubAppOutcome::Minted {
            token: "ghs_default".to_string(),
            installation_id: "1".to_string(),
            app_id: "2".to_string(),
            expires_at: "2099-01-01T00:00:00Z".to_string(),
        });
        assert_eq!(minter.mint_forced("owner/repo"), minter.mint("owner/repo"));
    }

    /// Writes a fake `github-app-token.sh` under `dir` that records its own
    /// argv (space-joined) to `dir/argv.log` and answers a fixed `Minted`
    /// envelope, so [`RealGithubAppMinter::mint`] / `mint_forced` can be
    /// exercised end-to-end (through `run_capture_with_timeout` and
    /// `parse_github_app_response`) without a real GitHub App or network call.
    fn write_fake_app_token_script(dir: &Path) -> PathBuf {
        let script = dir.join("fake-github-app-token.sh");
        let argv_log = dir.join("argv.log");
        std::fs::write(
            &script,
            format!(
                "#!/usr/bin/env bash\necho \"$@\" > {}\necho '{{\"status\":\"ok\",\"token\":\"ghs_fake\",\"installation_id\":\"42\",\"app_id\":\"7\",\"expires_at\":\"2099-01-01T00:00:00Z\"}}'\n",
                argv_log.display()
            ),
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        script
    }

    #[test]
    fn real_github_app_minter_mint_does_not_pass_force() {
        let dir = tempfile::tempdir().unwrap();
        let script = write_fake_app_token_script(dir.path());
        let minter = RealGithubAppMinter {
            script_path: script,
            cwd: dir.path().to_path_buf(),
        };

        let outcome = minter.mint("owner/repo");
        assert!(matches!(outcome, GithubAppOutcome::Minted { .. }));

        let argv = std::fs::read_to_string(dir.path().join("argv.log")).unwrap();
        assert!(argv.contains("get-token"), "argv should include the subcommand: {argv}");
        assert!(argv.contains("owner/repo"), "argv should include the nwo: {argv}");
        assert!(!argv.contains("--force"), "a plain mint() must not pass --force: {argv}");
    }

    #[test]
    fn real_github_app_minter_mint_forced_passes_force() {
        let dir = tempfile::tempdir().unwrap();
        let script = write_fake_app_token_script(dir.path());
        let minter = RealGithubAppMinter {
            script_path: script,
            cwd: dir.path().to_path_buf(),
        };

        let outcome = minter.mint_forced("owner/repo");
        assert!(matches!(outcome, GithubAppOutcome::Minted { .. }));

        let argv = std::fs::read_to_string(dir.path().join("argv.log")).unwrap();
        assert!(argv.contains("--force"), "mint_forced() must pass --force through: {argv}");
        assert!(argv.contains("get-token"), "argv should include the subcommand: {argv}");
        assert!(argv.contains("owner/repo"), "argv should include the nwo: {argv}");
    }

    #[test]
    #[serial_test::serial]
    fn owner_refresh_sources_registers_and_snapshots() {
        clear_owner_root_registry();
        assert!(owner_refresh_sources().is_empty());

        let dir = tempfile::tempdir().unwrap();
        let owner_dir = dir.path().join(".loom/gh-config-by-owner/2AMLogic");
        register_owner_refresh_source("2AMLogic/marketing", &owner_dir);

        let sources = owner_refresh_sources();
        assert_eq!(sources, vec![("2AMLogic/marketing".to_string(), owner_dir.clone())]);

        // Re-registering the same owner (a different representative repo)
        // replaces, rather than duplicates, its entry.
        register_owner_refresh_source("2AMLogic/2am", &owner_dir);
        let sources = owner_refresh_sources();
        assert_eq!(sources, vec![("2AMLogic/2am".to_string(), owner_dir)]);

        clear_owner_root_registry();
    }

    /// Set up a throwaway git repo whose `origin` remote resolves to
    /// `owner_repo` via [`nwo_from_git_remote`] — the precondition every
    /// [`force_refresh_owner_credential_with`] test needs.
    fn git_repo_with_remote(owner_repo: &str) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        assert!(Command::new("git")
            .args(["init", "-q"])
            .current_dir(dir.path())
            .status()
            .unwrap()
            .success());
        assert!(Command::new("git")
            .args([
                "remote",
                "add",
                "origin",
                &format!("https://github.com/{owner_repo}.git")
            ])
            .current_dir(dir.path())
            .status()
            .unwrap()
            .success());
        dir
    }

    #[test]
    fn force_refresh_owner_credential_with_no_remote_is_a_no_op() {
        let workspace = tempfile::tempdir().unwrap();
        let repo_root = tempfile::tempdir().unwrap();
        // No `git init` at all in `repo_root` -> `nwo_from_git_remote` fails.
        let minter = FixedMinter(GithubAppOutcome::Minted {
            token: "ghs_unused".to_string(),
            installation_id: "1".to_string(),
            app_id: "2".to_string(),
            expires_at: "2099-01-01T00:00:00Z".to_string(),
        });
        assert!(!force_refresh_owner_credential_with(
            workspace.path(),
            repo_root.path(),
            &minter
        ));
    }

    #[test]
    fn force_refresh_owner_credential_with_not_configured_is_a_no_op() {
        let workspace = tempfile::tempdir().unwrap();
        let repo_root = git_repo_with_remote("2AMLogic/sky130-ldo");
        let minter = FixedMinter(GithubAppOutcome::NotConfigured);
        assert!(!force_refresh_owner_credential_with(
            workspace.path(),
            repo_root.path(),
            &minter
        ));
    }

    /// #6663: this is the **only** test in the lib target that drives a
    /// production path which writes the process-global
    /// `FORGE_CREDENTIAL_STREAKS` tracker. Left unattended it marks the whole
    /// process "credential stale" for the 1800s grace window — i.e. for the
    /// rest of the test binary — and every later reader of the singleton (the
    /// `main_health_gate` verdict path) short-circuits to
    /// `ForgeCredentialStale` instead of the verdict it asserts, RED-ing 11
    /// unrelated tests depending on scheduling order.
    ///
    /// So it is `#[serial]` on the default key and brackets itself with
    /// [`reset_forge_credential_streaks`] — and, since the side effect is
    /// real production behavior worth pinning rather than an accident, it now
    /// asserts the streak was recorded before clearing it.
    #[test]
    #[serial_test::serial]
    fn force_refresh_owner_credential_with_mint_error_is_a_no_op() {
        reset_forge_credential_streaks();
        let workspace = tempfile::tempdir().unwrap();
        let repo_root = git_repo_with_remote("2AMLogic/sky130-ldo");
        let minter =
            FixedMinter(GithubAppOutcome::Error("app not installed on 2AMLogic".to_string()));
        assert!(!force_refresh_owner_credential_with(
            workspace.path(),
            repo_root.path(),
            &minter
        ));

        // A failed forced mint must open a failure streak for that owner's
        // source — that is what makes the gate hold its verdict rather than
        // read the fan-out as a red main (#5630).
        assert!(
            forge_credential_stale(),
            "a failed forced mint must record a failure on the global streak tracker"
        );
        let summary = forge_credential_stale_summary().unwrap_or_default();
        assert!(
            summary.contains("2AMLogic/sky130-ldo") && summary.contains("app not installed"),
            "the summary must name the failing source and reason, got: {summary}"
        );

        reset_forge_credential_streaks();
    }

    #[test]
    #[serial_test::serial]
    fn force_refresh_owner_credential_with_minted_publishes_and_registers_everything() {
        clear_owner_root_registry();
        let workspace = tempfile::tempdir().unwrap();
        // #6171's whole scenario: a workspace registered under an owner this
        // daemon already manages, whose checkout root differs from every root
        // known when the owner's credential was first established.
        let repo_root = git_repo_with_remote("2AMLogic/sky130-ldo");
        let minter = FixedMinter(GithubAppOutcome::Minted {
            token: "ghs_fresh".to_string(),
            installation_id: "151241341".to_string(),
            app_id: "4486636".to_string(),
            expires_at: "2099-01-01T00:00:00Z".to_string(),
        });

        let retried =
            force_refresh_owner_credential_with(workspace.path(), repo_root.path(), &minter);
        assert!(retried, "a successful forced mint must signal 'retry me'");

        let expected_dir = github_app_gh_config_dir_for_owner(workspace.path(), "2AMLogic");

        // The new token actually landed on disk...
        let hosts = std::fs::read_to_string(expected_dir.join("hosts.yml")).unwrap();
        assert!(hosts.contains("oauth_token: ghs_fresh"));

        // ...and every consumer that could route this repo's future `gh`
        // calls now finds it: root-keyed (#5401), owner-slug-keyed (#5431),
        // and the periodic-refresh source list (#6171) that keeps it fresh
        // going forward without needing another 404.
        assert_eq!(gh_config_dir_for_root(repo_root.path()), Some(expected_dir.clone()));
        assert_eq!(gh_config_dir_for_owner_slug("2AMLogic/anything"), Some(expected_dir.clone()));
        assert_eq!(
            owner_refresh_sources(),
            vec![("2AMLogic/sky130-ldo".to_string(), expected_dir)]
        );

        clear_owner_root_registry();
    }
}
