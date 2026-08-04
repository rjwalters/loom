//! `fleet add-worker` — bootstrap a provisioned host into a working loom worker
//! (issue #4341, epic #4340). This module renders the ordered bootstrap plan
//! (the heredoc-style shell templates for each [`super::Step`]), the production
//! SSH [`super::CommandRunner`], and the top-level [`run`] orchestration.
//!
//! The plan encodes the #3979 Phase-2 pilot's verified hand bootstrap (the
//! 2026-07-28 pilot report on #3979 is ground truth). Deliberately **absent**,
//! because the underlying gaps have since landed:
//!
//! - **No Python `loom_tools` / `pip --break-system-packages` step** — #4228
//!   landed, so `spawn-claude.sh` execs the native `loom-daemon tokens select`;
//!   a Rust-only worker spawns sweeps with zero Python on the token hot path.
//! - **No single-repo daemon-cwd pin for dispatch** — #4299 landed (PR #4322):
//!   dispatch resolves the target workspace from the registry, so a worker can
//!   register several repos.
//!
//! The **only** remaining cwd coupling is the token pool (#4292, still open):
//! the systemd unit pins `WorkingDirectory=` to a workspace clone so
//! `.loom/tokens/` resolves. That workaround is marked with `#4292` in the
//! rendered unit so it is removed when #4292 lands.

use anyhow::{anyhow, bail, Context, Result};
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use super::path_bootstrap;
use super::{
    all_succeeded, default_fleet_registry_path, execute_plan, render_checklist, CommandOutput,
    CommandRunner, FleetRegistry, Plan, Step, StepStatus, StepStdin, VerifyResult, WorkerRecord,
    CHECKLIST_NOTE_PREFIX,
};

/// Default upstream Loom repo cloned to the worker's machine-level layout.
pub const DEFAULT_LOOM_REPO_URL: &str = "https://github.com/rjwalters/loom";

/// Default checkout the `safehoused` binary is built from on the worker
/// (issue #3998's spin-up half — `safehoused` itself is owned by the external
/// `rjwalters/safehouse` repo, never vendored here).
pub const DEFAULT_SAFEHOUSE_REPO_URL: &str = "https://github.com/rjwalters/safehouse";

/// The AF_UNIX socket path `safehoused` is supervised to bind on a fleet
/// worker, and the value wired into the worker's `loom-daemon` unit as
/// `LOOM_SAFEHOUSE_SOCKET` (#3998). Deterministic (not resolved via the
/// shared `mcp-config.sh` chain) so the two ends of the pipe always agree on
/// a fresh worker with no prior `.loom/config.json` safehouse block.
const SAFEHOUSE_SOCKET_PATH: &str = "$HOME/.loom/safehoused.sock";

/// Default invocation for the safehouse#39 daemon-side room-membership
/// `invite` op, run once against the freshly-written config so the host's
/// Matrix account joins the fleet room without raw CS-API temp devices.
/// Overridable via `--safehouse-invite-exec` (mirrors
/// `safehoused-service.sh`'s `--exec`: this repo does not vendor safehoused's
/// argv, since that is owned by the external `rjwalters/safehouse` repo).
const DEFAULT_SAFEHOUSE_INVITE_EXEC: &str =
    r#"safehoused invite --config "$HOME/.loom/safehoused/config.toml""#;

/// Base directory (systemd `%h`-relative) under which workspace repos are
/// cloned on the worker.
const WORKSPACE_BASE: &str = "loom-workspaces";

/// Operator inputs for a single `fleet add-worker` invocation.
#[derive(Debug, Clone)]
pub struct AddWorkerConfig {
    /// SSH alias/host to reach the worker (from `repo:remote` or operator
    /// supplied). Never embedded in a secret-carrying command.
    pub ssh_host: String,
    /// Workspace repos to clone + register on the worker (`owner/name`). At
    /// least one is required.
    pub repos: Vec<String>,
    /// Cross-repo dispatch priority the workspaces are registered at (#3946;
    /// lower = higher priority).
    pub priority: u32,
    /// Print the ordered plan without contacting the host.
    pub dry_run: bool,
    /// Upstream Loom repo URL to clone on the worker.
    pub loom_repo_url: String,
    /// Local path to the operator's fine-grained forge PAT (Contents+Issues+PRs
    /// on the target repos). Transferred to the worker only via ssh stdin.
    pub pat_file: Option<PathBuf>,
    /// Local path to the operator's `accounts.env` (the full token pool).
    /// Transferred to the worker only via ssh stdin.
    pub accounts_env_file: Option<PathBuf>,
    /// Whether to wire safehouse fleet-comms (config-gated; see [`build_plan`]).
    pub safehouse_enabled: bool,
    /// Idle-shutdown guard: power the host off after this many idle minutes.
    /// `None` disables the guard.
    pub idle_shutdown_minutes: Option<u32>,
    /// Local path to the operator-minted, ephemeral + `tag:loom-worker`
    /// Tailscale auth key. Read locally at preflight; transferred to the
    /// worker only via ssh stdin. Required when `safehouse_enabled`.
    pub safehouse_tailnet_auth_key_file: Option<PathBuf>,
    /// Local path to a `KEY=VALUE` env-style file carrying the per-host
    /// Matrix account credentials and store/recovery passphrases
    /// (`SAFEHOUSE_MATRIX_USER_ID`, `SAFEHOUSE_MATRIX_PASSWORD`,
    /// `SAFEHOUSE_STORE_PASSPHRASE`, `SAFEHOUSE_RECOVERY_PASSPHRASE`). Read
    /// locally at preflight; transferred to the worker only via ssh stdin.
    /// Required when `safehouse_enabled`.
    pub safehouse_secrets_file: Option<PathBuf>,
    /// The external `rjwalters/safehouse` checkout `safehoused` is built
    /// from on the worker.
    pub safehouse_repo_url: String,
    /// The homeserver URL (resolves inside the tailnet) written into
    /// safehoused's config. Not secret. Required when `safehouse_enabled`.
    pub safehouse_homeserver_url: Option<String>,
    /// The fleet room safehoused joins. Not secret. Required when
    /// `safehouse_enabled`.
    pub safehouse_room: Option<String>,
    /// The persona allowlist this host's `safehoused` boots with — mirrors
    /// the studio host's allowlist (#3999). Not secret. Must be non-empty
    /// when `safehouse_enabled`; written into the boot-time TOML **before**
    /// safehoused's first start (hard ordering constraint — no reload).
    pub safehouse_personas: Vec<String>,
    /// Override for the safehouse#39 room-`invite` op invocation. `None`
    /// uses [`DEFAULT_SAFEHOUSE_INVITE_EXEC`]. loom does not vendor
    /// safehoused's real CLI surface (owned by the external repo), so this
    /// mirrors `safehoused-service.sh --exec`.
    pub safehouse_invite_exec: Option<String>,
}

/// Secret payloads read locally during preflight, then fed to the plan over
/// stdin. Never logged, never placed on a command line.
#[derive(Debug, Clone, Default)]
pub struct Secrets {
    /// Fine-grained forge PAT contents, if `--pat-file` was supplied.
    pub pat: Option<String>,
    /// `accounts.env` contents, if `--accounts-env` was supplied.
    pub accounts_env: Option<String>,
    /// Tailnet auth-key contents, if `--safehouse-tailnet-auth-key-file` was
    /// supplied.
    pub safehouse_tailnet_auth_key: Option<String>,
    /// The safehouse secrets env-file contents, if
    /// `--safehouse-secrets-file` was supplied.
    pub safehouse_secrets: Option<String>,
}

/// The production [`CommandRunner`]: runs one rendered shell command per call
/// over `ssh <host> <shell>`, feeding secrets via the child's stdin so they
/// never appear on a command line (where `ps` on the worker would expose them).
pub struct SshRunner {
    /// The SSH alias/host.
    pub host: String,
}

impl SshRunner {
    /// A runner targeting `host`.
    #[must_use]
    pub fn new(host: &str) -> Self {
        Self {
            host: host.to_string(),
        }
    }
}

impl CommandRunner for SshRunner {
    fn run(&self, shell: &str, stdin: Option<&str>) -> Result<CommandOutput> {
        let mut cmd = Command::new("ssh");
        // BatchMode: never block on an interactive password/passphrase prompt —
        // an unreachable or auth-failing host fails fast instead of hanging.
        cmd.arg("-o").arg("BatchMode=yes");
        cmd.arg(&self.host);
        cmd.arg(shell);
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());
        if stdin.is_some() {
            cmd.stdin(Stdio::piped());
        } else {
            // Detach stdin so a remote command that reads stdin does not hang
            // waiting on this process's terminal.
            cmd.stdin(Stdio::null());
        }

        let mut child = cmd
            .spawn()
            .with_context(|| format!("failed to launch `ssh {}` (is ssh installed?)", self.host))?;

        if let Some(payload) = stdin {
            let mut sink = child
                .stdin
                .take()
                .ok_or_else(|| anyhow!("ssh child stdin was not captured"))?;
            sink.write_all(payload.as_bytes())
                .context("writing payload to ssh stdin")?;
            // Drop closes the pipe so the remote `cat`/`gh --with-token` sees EOF.
            drop(sink);
        }

        let out = child.wait_with_output().context("waiting on ssh child")?;
        Ok(CommandOutput {
            code: out.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        })
    }
}

/// A forge slug (`owner/name`) restricted to a safe character set, so it can be
/// interpolated into rendered shell without an injection hazard.
fn validate_repo(repo: &str) -> Result<()> {
    if repo.is_empty() {
        bail!("--repo must not be empty");
    }
    if !repo
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '/'))
    {
        bail!("--repo '{repo}' contains characters outside [A-Za-z0-9._/-]; refusing to render it into shell");
    }
    if !repo.contains('/') {
        bail!("--repo '{repo}' should be a forge slug like owner/name");
    }
    Ok(())
}

/// Derive the on-worker clone directory name for a repo slug (`owner/name` →
/// `name`). Validated by [`validate_repo`] first, so the result is shell-safe.
fn repo_dir_name(repo: &str) -> &str {
    repo.rsplit('/').next().unwrap_or(repo)
}

/// Non-secret operator strings that get interpolated into rendered shell
/// (homeserver URL, room) must pass this before they are ever formatted into
/// a step's `apply`/`check` text — mirrors [`validate_repo`]'s shell-injection
/// defense for the same reason: this text is echoed verbatim in `--dry-run`
/// output and executed on the worker.
fn validate_safe_token(label: &str, value: &str, extra: &str) -> Result<()> {
    if value.trim().is_empty() {
        bail!("--{label} must not be empty");
    }
    if !value
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || extra.contains(c))
    {
        bail!(
            "--{label} '{value}' contains characters outside [A-Za-z0-9{extra}]; refusing to \
             render it into shell"
        );
    }
    Ok(())
}

/// A safehouse persona name: `[a-z0-9_]`, 1..=64 chars (mirrors
/// `safehouse.rs`'s `valid_persona` charset — this module cannot depend on
/// `safehouse` directly without pulling in its async/tokio surface, so the
/// charset is duplicated deliberately, the same tradeoff `drain.rs`'s module
/// doc makes for `DEFAULT_DRAIN_TIMEOUT_SECS`).
fn validate_persona_name(persona: &str) -> Result<()> {
    if persona.is_empty() || persona.len() > 64 {
        bail!("--safehouse-persona '{persona}' must be 1..=64 characters");
    }
    if !persona
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
    {
        bail!("--safehouse-persona '{persona}' must match [a-z0-9_]");
    }
    Ok(())
}

/// `%h`-relative workspace path for a repo (systemd expands `%h` to the user's
/// home in a user unit; the shell steps use `"$HOME/..."` for the same path).
fn workspace_rel(repo: &str) -> String {
    format!("{WORKSPACE_BASE}/{}", repo_dir_name(repo))
}

/// Local preflight (AC 3): validate inputs and read the operator's secret files
/// **before any remote action**. Fails with a clear message if a supplied
/// secret file is missing/unreadable, or if inputs are malformed. Reading a
/// local file does not "touch the host", so this runs in dry-run mode too.
pub fn preflight(config: &AddWorkerConfig) -> Result<Secrets> {
    if config.ssh_host.trim().is_empty() {
        bail!("ssh-host must not be empty");
    }
    if config.repos.is_empty() {
        bail!("at least one --repo is required");
    }
    for repo in &config.repos {
        validate_repo(repo)?;
    }

    let mut secrets = Secrets::default();
    if let Some(path) = &config.pat_file {
        let contents = std::fs::read_to_string(path)
            .with_context(|| format!("reading --pat-file {} (does it exist?)", path.display()))?;
        let trimmed = contents.trim();
        if trimmed.is_empty() {
            bail!("--pat-file {} is empty", path.display());
        }
        secrets.pat = Some(trimmed.to_string());
    }
    if let Some(path) = &config.accounts_env_file {
        let contents = std::fs::read_to_string(path).with_context(|| {
            format!("reading --accounts-env {} (does it exist?)", path.display())
        })?;
        if contents.trim().is_empty() {
            bail!("--accounts-env {} is empty", path.display());
        }
        secrets.accounts_env = Some(contents);
    }

    // Safehouse (#3998): when requested, every input is required up front —
    // a half-specified `--safehouse` would otherwise render a broken plan
    // (a config step with no homeserver/room, or a join step with no key)
    // rather than failing fast before any remote action (AC: "no half-joined
    // host").
    if config.safehouse_enabled {
        let mut missing = Vec::new();
        if config.safehouse_tailnet_auth_key_file.is_none() {
            missing.push("--safehouse-tailnet-auth-key-file");
        }
        if config.safehouse_secrets_file.is_none() {
            missing.push("--safehouse-secrets-file");
        }
        if config
            .safehouse_homeserver_url
            .as_deref()
            .unwrap_or("")
            .trim()
            .is_empty()
        {
            missing.push("--safehouse-homeserver-url");
        }
        if config
            .safehouse_room
            .as_deref()
            .unwrap_or("")
            .trim()
            .is_empty()
        {
            missing.push("--safehouse-room");
        }
        if config.safehouse_personas.is_empty() {
            missing.push("--safehouse-persona (at least one)");
        }
        if !missing.is_empty() {
            bail!(
                "--safehouse requires the following input(s), none of which were supplied: {}",
                missing.join(", ")
            );
        }
        if let Some(homeserver) = &config.safehouse_homeserver_url {
            validate_safe_token("safehouse-homeserver-url", homeserver, ".:/_-")?;
        }
        if let Some(room) = &config.safehouse_room {
            validate_safe_token("safehouse-room", room, "!#:._-")?;
        }
        for persona in &config.safehouse_personas {
            validate_persona_name(persona)?;
        }
    }
    if let Some(path) = &config.safehouse_tailnet_auth_key_file {
        let contents = std::fs::read_to_string(path).with_context(|| {
            format!("reading --safehouse-tailnet-auth-key-file {} (does it exist?)", path.display())
        })?;
        let trimmed = contents.trim();
        if trimmed.is_empty() {
            bail!("--safehouse-tailnet-auth-key-file {} is empty", path.display());
        }
        secrets.safehouse_tailnet_auth_key = Some(trimmed.to_string());
    }
    if let Some(path) = &config.safehouse_secrets_file {
        let contents = std::fs::read_to_string(path).with_context(|| {
            format!("reading --safehouse-secrets-file {} (does it exist?)", path.display())
        })?;
        if contents.trim().is_empty() {
            bail!("--safehouse-secrets-file {} is empty", path.display());
        }
        secrets.safehouse_secrets = Some(contents);
    }

    Ok(secrets)
}

/// Build the ordered bootstrap [`Plan`] from `config` + already-read `secrets`.
///
/// Pure (no I/O, no host contact) so the ordering, idempotency checks, secret
/// placement, and skip-with-notice branches are all unit-testable. Each step's
/// `check` phase is what makes a re-run idempotent (AC 2).
#[must_use]
pub fn build_plan(config: &AddWorkerConfig, secrets: &Secrets) -> Plan {
    let mut plan = Plan::new();
    let primary_rel = workspace_rel(&config.repos[0]);

    // 1. Base deps (safehouse#38: libsqlite3-dev is required). Deliberately
    //    does NOT install a Rust toolchain (#5067, Epic #4990 Phase 4): the
    //    happy path (a release artifact resolves for this host's platform)
    //    never needs one. `machine-layout` below installs rustup itself, as a
    //    reactive fallback, only when it actually falls back to a from-source
    //    build.
    plan.push_step(Step::new(
        "base-deps",
        "install build-essential, pkg-config, libssl-dev, libsqlite3-dev, git, gh",
        Some(
            "dpkg -s build-essential pkg-config libssl-dev libsqlite3-dev git >/dev/null 2>&1 \
             && command -v gh >/dev/null 2>&1"
                .to_string(),
        ),
        render_base_deps(),
    ));

    // 2. Machine-level layout: clone loom, install loom-daemon to ~/.local/bin
    //    from a verified GitHub Release artifact when one resolves for this
    //    host's platform (#5067, Epic #4990 Phase 4), falling back to
    //    `cargo build -p loom-daemon --release` (installing rustup first,
    //    reactively) only when no artifact resolves.
    plan.push_step(Step::new(
        "machine-layout",
        "clone loom to ~/.local/share/loom, install loom-daemon to ~/.local/bin from a release artifact (falling back to cargo build -p loom-daemon --release when no artifact resolves)",
        Some(r#"test -x "$HOME/.local/bin/loom-daemon""#.to_string()),
        render_machine_layout(&config.loom_repo_url),
    ));

    // 3. Claude Code install.
    plan.push_step(Step::new(
        "claude-code",
        "install Claude Code CLI",
        Some("command -v claude >/dev/null 2>&1".to_string()),
        render_claude_code(),
    ));

    // 4. Forge auth via operator-supplied fine-grained PAT (over stdin).
    match &secrets.pat {
        Some(pat) => plan.push_step(
            Step::new(
                "forge-auth",
                "authenticate gh with the fine-grained PAT (via stdin) and set up git credential helper",
                Some("gh auth status >/dev/null 2>&1".to_string()),
                render_forge_auth(),
            )
            .with_stdin(StepStdin { content: pat.clone(), secret: true }),
        ),
        None => plan.push_skip(
            "forge-auth",
            "authenticate gh with the fine-grained PAT",
            "no --pat-file supplied",
        ),
    }

    // 5. Token pool: full account pool (#3979 decision — no pinned subsets).
    //    5a. Install accounts.env (secret, over stdin, 0600).
    match &secrets.accounts_env {
        Some(env) => plan.push_step(
            Step::new(
                "token-accounts",
                "install accounts.env to ~/.loom/accounts.env (0600, via stdin)",
                None,
                render_token_accounts(),
            )
            .with_stdin(StepStdin {
                content: env.clone(),
                secret: true,
            }),
        ),
        None => {
            plan.push_skip("token-accounts", "install accounts.env", "no --accounts-env supplied")
        }
    }
    //    5b. Bootstrap the shared pool (idempotent — check for existing tokens).
    if secrets.accounts_env.is_some() {
        plan.push_step(Step::new(
            "token-pool",
            "loom-daemon tokens bootstrap (shared machine pool)",
            Some(r#"ls "$HOME/.loom/tokens"/*.token >/dev/null 2>&1"#.to_string()),
            render_token_pool(),
        ));
    } else {
        plan.push_skip("token-pool", "bootstrap the token pool", "no --accounts-env supplied");
    }
    //    5c. Refresh the ranking (always — the verify AC wants a fresh ranking).
    if secrets.accounts_env.is_some() {
        plan.push_step(Step::new(
            "token-ranking",
            "loom-daemon tokens check --ranking (fresh availability probe)",
            None,
            render_token_ranking(),
        ));
    } else {
        plan.push_skip("token-ranking", "refresh the token ranking", "no --accounts-env supplied");
    }

    // 6. Clone + init workspace repos (init installs the /loom:sweep command, #4027).
    plan.push_step(Step::new(
        "workspace-clone",
        "clone workspace repo(s) and run loom-daemon init on unconfigured ones (installs /loom:sweep)",
        Some(render_workspace_clone_check(&config.repos)),
        render_workspace_clone(&config.repos),
    ));

    // 7. Register workspaces in the machine-level registry (idempotent).
    plan.push_step(Step::new(
        "workspace-register",
        &format!("loom-daemon workspace add each repo (priority {})", config.priority),
        Some(render_workspace_register_check(&config.repos)),
        render_workspace_register(&config.repos, config.priority),
    ));

    // 8. Start the daemon under a systemd --user unit (Restart=on-success, linger).
    plan.push_step(Step::new(
        "daemon-unit",
        "install + enable the loom-daemon systemd --user unit (linger, Restart=on-success, LOOM_DAEMON_SUPERVISOR=systemd)",
        Some("systemctl --user is-enabled loom-daemon.service >/dev/null 2>&1".to_string()),
        render_daemon_unit(&primary_rel),
    ));

    // 9. Idle-shutdown guard (optional).
    match config.idle_shutdown_minutes {
        Some(minutes) => plan.push_step(Step::new(
            "idle-shutdown",
            &format!("install idle-shutdown cron guard ({minutes} idle minutes)"),
            Some(r#"crontab -l 2>/dev/null | grep -q loom-idle-shutdown"#.to_string()),
            render_idle_shutdown(minutes),
        )),
        None => plan.push_skip(
            "idle-shutdown",
            "install idle-shutdown cron guard",
            "no --idle-shutdown-minutes supplied",
        ),
    }

    // 10. Optional safehouse wiring: tailnet join, safehoused build/config/
    //     room-invite/supervision, then restart the worker's loom-daemon with
    //     LOOM_SAFEHOUSE_* env so narration flows (#3998). Every sub-step
    //     below follows the same check/apply contract as the rest of the
    //     plan, so a re-run against an already-provisioned host (the pilot,
    //     `loom-worker-1`) reports every one `AlreadyDone`.
    if config.safehouse_enabled {
        push_safehouse_steps(&mut plan, config, secrets, &primary_rel);
    } else {
        plan.push_skip(
            "safehouse",
            "wire safehouse fleet-comms",
            "safehouse not requested (pass --safehouse plus its input flags to enable)",
        );
    }

    // 11. Verify: daemon reachable from the workspace cwd, ranking fresh, repos registered.
    plan.push_step(Step::new(
        "verify",
        "verify: loom-daemon status sane from the workspace cwd, token ranking fresh, workspace registered",
        None,
        render_verify(&primary_rel, &config.repos),
    ));

    plan
}

/// Append the safehouse spin-up sub-sequence (#3998) to `plan`: tailnet join,
/// `safehoused` build, boot-time TOML config, room-membership invite,
/// supervision, then a restart of the worker's own `loom-daemon` with
/// `LOOM_SAFEHOUSE_*` env. Split out of [`build_plan`] purely for
/// readability — still pure, still called only when `config.safehouse_enabled`.
///
/// **Ordering is the load-bearing part of this function**: `safehouse-config`
/// (which writes the boot-time persona TOML) MUST precede
/// `safehouse-supervise` (which first starts `safehoused`) — the allowlist is
/// boot-time-only, no reload (AC: "written before the unit first starts").
fn push_safehouse_steps(
    plan: &mut Plan,
    config: &AddWorkerConfig,
    secrets: &Secrets,
    primary_rel: &str,
) {
    // 10a. Install the tailscale package (apt repo, arch/codename-agnostic).
    plan.push_step(Step::new(
        "safehouse-tailscale-install",
        "install the tailscale package (apt repo)",
        Some("command -v tailscale >/dev/null 2>&1".to_string()),
        render_safehouse_tailscale_install(),
    ));

    // 10b. Join the tailnet with an operator-minted ephemeral + tag:loom-worker
    //     auth key (fed over stdin, never on a command line — an expired/
    //     revoked key fails this step's apply fast, with tailscale's own
    //     diagnostic surfaced via the checklist's stderr tail).
    plan.push_step(
        Step::new(
            "safehouse-tailscale-join",
            "tailscale up with an ephemeral + tag:loom-worker auth key (via stdin)",
            Some("tailscale ip -4 >/dev/null 2>&1".to_string()),
            render_safehouse_tailscale_join(),
        )
        .with_stdin(StepStdin {
            content: secrets
                .safehouse_tailnet_auth_key
                .clone()
                .unwrap_or_default(),
            secret: true,
        }),
    );

    // 10c. Build safehoused from the external rjwalters/safehouse checkout.
    plan.push_step(Step::new(
        "safehouse-build",
        "cargo build --release -p safehoused from a safehouse checkout",
        Some(r#"test -x "$HOME/.local/bin/safehoused""#.to_string()),
        render_safehouse_build(&config.safehouse_repo_url),
    ));

    // 10d. Write the boot-time config.toml (0600): homeserver URL, per-host
    //     Matrix account, fresh store/recovery passphrases (via stdin), and
    //     the persona allowlist mirroring the studio host. MUST land before
    //     10f (first start) — see this function's doc comment.
    let homeserver = config.safehouse_homeserver_url.as_deref().unwrap_or("");
    let room = config.safehouse_room.as_deref().unwrap_or("");
    plan.push_step(
        Step::new(
            "safehouse-config",
            "write ~/.loom/safehoused/config.toml (0600): homeserver, account, passphrases, personas",
            Some(r#"test -f "$HOME/.loom/safehoused/config.toml""#.to_string()),
            render_safehouse_config(homeserver, room, &config.safehouse_personas),
        )
        .with_stdin(StepStdin {
            content: secrets.safehouse_secrets.clone().unwrap_or_default(),
            secret: true,
        }),
    );

    // 10e. Room membership via safehouse#39's daemon-side `invite` op — not
    //     raw CS-API temp devices.
    plan.push_step(Step::new(
        "safehouse-room-invite",
        "join the fleet room via safehouse#39's invite op",
        Some(r#"test -f "$HOME/.loom/safehoused/.invited""#.to_string()),
        render_safehouse_room_invite(config.safehouse_invite_exec.as_deref()),
    ));

    // 10f. Supervise safehoused under systemd --user + linger (mirrors the
    //     daemon-unit step's own supervision pattern, step 8). This is the
    //     first point safehoused ever starts — 10d must precede this.
    plan.push_step(Step::new(
        "safehouse-supervise",
        "install + enable the safehoused systemd --user unit (linger)",
        Some("systemctl --user is-enabled safehoused.service >/dev/null 2>&1".to_string()),
        render_safehouse_supervise(primary_rel),
    ));

    // 10g. Restart the worker's own loom-daemon with LOOM_SAFEHOUSE_* env so
    //     sweep narration flows (env-only wiring, #3997 — no worker-side
    //     .loom/config.json edit).
    plan.push_step(Step::new(
        "safehouse-daemon-restart",
        "wire LOOM_SAFEHOUSE_ENABLED/SOCKET/ROOM into the loom-daemon unit and restart it",
        Some(
            r#"grep -q '^Environment=LOOM_SAFEHOUSE_ENABLED=true$' "$HOME/.config/systemd/user/loom-daemon.service" 2>/dev/null"#
                .to_string(),
        ),
        render_safehouse_daemon_restart(room),
    ));
}

/// Build the [`WorkerRecord`] a fully-successful bootstrap upserts into the
/// fleet registry. Pure (every ambient input — the config, the `verify` step's
/// outcome, and the clock — is a parameter) so the field mapping is unit
/// testable without an SSH host; [`run`] is the only production caller.
///
/// `verify_ok` is `None` when the plan carried no `verify` report at all,
/// `Some(false)` when it ran without confirming.
fn build_worker_record(
    config: &AddWorkerConfig,
    verify_ok: Option<bool>,
    now: chrono::DateTime<chrono::Utc>,
) -> WorkerRecord {
    let now_str = now.to_rfc3339();
    WorkerRecord {
        ssh_host: config.ssh_host.clone(),
        repos: config.repos.clone(),
        priority: config.priority,
        bootstrapped_at: now_str.clone(),
        last_verify: verify_ok.map(|ok| VerifyResult {
            ok,
            at: now_str.clone(),
            summary: if ok {
                "daemon reachable, ranking fresh, workspace registered".to_string()
            } else {
                "verify step did not confirm".to_string()
            },
        }),
        // #4342's extended roster fields (provider/instance id, tailnet
        // name, added-by) are not yet collected by `add-worker` — left
        // absent here rather than guessed; `fleet status` renders them as
        // "–" until a future pass wires them (e.g. from `--tailnet-name`/
        // `--provider-instance-id` flags or a cloud-metadata probe).
        provider_instance_id: None,
        tailnet_name: None,
        added_by: None,
        state: None,
        drain_phase: None,
        drain_captured: Vec::new(),
        // Populate the new #4697 fields from this run: the operator's
        // `--idle-shutdown-minutes` (absent => no guard installed, mirrors
        // the `render_idle_shutdown()` gate above), and `last_seen_up_at`
        // — a full bootstrap only reaches this branch when every step
        // (including `verify`) succeeded over SSH, so the host was
        // observably up moments ago; this seeds `fleet status`'s
        // expected-power-off heuristic with a reference point from the
        // very first poll, rather than leaving it `None` until some later
        // `fleet status` run happens to observe the host `Up`.
        idle_shutdown_minutes: config.idle_shutdown_minutes,
        last_seen_up_at: Some(now_str),
    }
}

/// Top-level orchestration for `loom-daemon fleet add-worker`.
///
/// Preflight → build plan → (dry-run: print + return) → execute over ssh →
/// print the per-step checklist → on full success, upsert the fleet registry
/// record. Returns an error if any step failed (so the CLI exits non-zero).
pub fn run(config: &AddWorkerConfig) -> Result<()> {
    let secrets = preflight(config)?;
    let plan = build_plan(config, &secrets);

    if config.dry_run {
        print!("{}", plan.render_dry_run("fleet add-worker", &config.ssh_host));
        println!(
            "\n(dry run — no action taken on {}. Re-run without --dry-run to execute.)",
            config.ssh_host
        );
        return Ok(());
    }

    let runner = SshRunner::new(&config.ssh_host);
    let reports = execute_plan(&runner, &plan);
    print!("{}", render_checklist("fleet add-worker", &config.ssh_host, &reports));

    let verify_ok = reports
        .iter()
        .find(|r| r.name == "verify")
        .map(|r| matches!(r.status, StepStatus::Changed));

    if all_succeeded(&reports, &plan) {
        let record = build_worker_record(config, verify_ok, chrono::Utc::now());
        let path = default_fleet_registry_path()?;
        let mut registry = FleetRegistry::load(&path)?;
        let replaced = registry.upsert(record);
        registry.save(&path)?;
        println!(
            "\nWorker {} {} in the fleet registry ({}).",
            config.ssh_host,
            if replaced { "updated" } else { "recorded" },
            path.display()
        );
        println!("Bootstrap complete: daemon running, workspace registered, tokens ranked, dispatch verified.");
        Ok(())
    } else {
        let failed = reports
            .iter()
            .find(|r| r.status.is_failure())
            .map_or_else(|| "unknown".to_string(), |r| r.name.clone());
        bail!(
            "fleet add-worker halted at step '{failed}' on {} — see the checklist above. \
             The run is idempotent: fix the cause and re-run to resume.",
            config.ssh_host
        )
    }
}

// ===========================================================================
// Shell templates (rendered on the daemon, executed on the worker over ssh)
// ===========================================================================

fn render_base_deps() -> String {
    // NOTE: the gh install is written as shell text here (never run through
    // the daemon's own shell), so the curl-pipe idiom is safe.
    //
    // Deliberately does NOT install a Rust toolchain (#5067): the happy path
    // (a release artifact resolves for this host's platform in the
    // `machine-layout` step below) never needs one. `machine-layout` installs
    // rustup itself, reactively, only when it actually falls back to a
    // from-source build.
    r#"set -e
export DEBIAN_FRONTEND=noninteractive
sudo apt-get update -qq
sudo apt-get install -y build-essential pkg-config libssl-dev libsqlite3-dev git curl ca-certificates
if ! command -v gh >/dev/null 2>&1; then
  sudo mkdir -p -m 755 /etc/apt/keyrings
  curl -fsSL https://cli.github.com/packages/githubcli-archive-keyring.gpg | sudo tee /etc/apt/keyrings/githubcli-archive-keyring.gpg >/dev/null
  sudo chmod go+r /etc/apt/keyrings/githubcli-archive-keyring.gpg
  echo "deb [arch=$(dpkg --print-architecture) signed-by=/etc/apt/keyrings/githubcli-archive-keyring.gpg] https://cli.github.com/packages stable main" | sudo tee /etc/apt/sources.list.d/github-cli.list >/dev/null
  sudo apt-get update -qq
  sudo apt-get install -y gh
fi
"#
    .to_string()
}

fn render_machine_layout(loom_repo_url: &str) -> String {
    // The canonical PATH export line (#4831) — see path_bootstrap.rs. Written
    // into ~/.profile (persisted for future logins) as well as exported
    // immediately by every other apply step below, so a worker that gets a
    // fresh interactive login (not just the one-shot provisioning SSH
    // sessions) also has cargo/homebrew resolvable without sourcing this
    // provisioning plan again.
    let export_line = path_bootstrap::canonical_path_export_line();
    let export_line = export_line.trim_end();
    format!(
        r#"set -e
. "$HOME/.cargo/env" 2>/dev/null || true
LOOM_SRC="$HOME/.local/share/loom"
if [ -d "$LOOM_SRC/.git" ]; then
  git -C "$LOOM_SRC" pull --ff-only
else
  mkdir -p "$(dirname "$LOOM_SRC")"
  git clone {loom_repo_url} "$LOOM_SRC"
fi
mkdir -p "$HOME/.local/bin"
# Install loom-daemon to ~/.local/bin (Epic #4990 Phase 4, #5067). Shells out
# to loom-daemon-update.sh's own already-tested "auto" resolution (Phase 3,
# #5020) rather than reimplementing the fetch/verify/checksum logic here: it
# prefers a checksum-verified GitHub Release artifact for this host's
# platform, and SOFTLY falls back to `cargo build -p loom-daemon --release`
# only when no artifact resolves (unrecognized platform, no gh CLI, no
# Releases, rate-limited/unreachable API, no matching asset for this target) —
# never a hard failure on that account. --no-restart is safe here: the
# loom-daemon systemd unit has not been installed yet (a later step), so there
# is never a running daemon for this invocation to try to restart.
UPDATE_SCRIPT="$LOOM_SRC/defaults/scripts/cli/loom-daemon-update.sh"
if ! "$UPDATE_SCRIPT" --no-restart; then
  # A missing Rust toolchain is the ONLY failure this can repair: install
  # rustup as a reactive fallback dependency (never on the happy path, see
  # base-deps above) and retry exactly once. Any other failure (e.g. cargo IS
  # present and the build itself failed) is not retried.
  if ! command -v cargo >/dev/null 2>&1; then
    echo "No Rust toolchain and no release artifact resolved for this platform -- installing rustup as a fallback dependency..."
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
    . "$HOME/.cargo/env" 2>/dev/null || true
    "$UPDATE_SCRIPT" --no-restart
  else
    exit 1
  fi
fi
# Ensure the canonical loom PATH (#4831: ~/.local/bin, ~/.cargo/bin, Homebrew,
# system dirs) is on PATH for future logins (Linux worker skips codesign).
if ! grep -qF 'loom-canonical-path (#4831)' "$HOME/.profile" 2>/dev/null; then
  {{
    echo '# loom-canonical-path (#4831)'
    printf '%s\n' '{export_line}'
  }} >> "$HOME/.profile"
fi
"#
    )
}

fn render_claude_code() -> String {
    r#"set -e
curl -fsSL https://claude.ai/install.sh | bash
"#
    .to_string()
}

fn render_forge_auth() -> String {
    // The PAT arrives on stdin; pipe it straight into `gh auth login` so it
    // never lands on a command line. `gh` stores it 0600 under ~/.config/gh.
    let export_line = path_bootstrap::canonical_path_export_line();
    format!(
        r#"set -e
{export_line}gh auth login --with-token
gh auth setup-git
"#
    )
}

fn render_token_accounts() -> String {
    // accounts.env arrives on stdin; umask 077 makes the written file 0600.
    r#"set -e
umask 077
mkdir -p "$HOME/.loom"
cat > "$HOME/.loom/accounts.env"
chmod 600 "$HOME/.loom/accounts.env"
"#
    .to_string()
}

fn render_token_pool() -> String {
    let export_line = path_bootstrap::canonical_path_export_line();
    format!(
        r#"set -e
{export_line}LOOM_ACCOUNTS_ENV="$HOME/.loom/accounts.env" loom-daemon tokens bootstrap --shared --home-env "$HOME/.loom/accounts.env"
"#
    )
}

/// Refresh the shared token ranking, then gate on account health (#5334).
///
/// `loom-daemon tokens check --ranking` only exits non-zero when *every*
/// probed account reports `error`/`skipped` — an all-`blocked`/`exhausted`
/// pool (the 2026-08-04 loom-worker-2 pilot's actual failure: a stale
/// `accounts.env` with 4/4 blocked accounts) still exits `0`, so a bare exit
/// code is not an "is this worker usable" signal. This step instead greps the
/// table's own `Total N: ...` summary line for the `available` count and:
///
/// - always emits a `LOOM_CHECKLIST_NOTE:` line with the `available/total`
///   split, so `execute_step` surfaces it in the checklist even on success —
///   never just a bare "changed" (AC 3);
/// - fails loudly (non-zero exit, `WARNING` on stderr) when the pool has zero
///   `available` accounts, so the run halts here instead of silently
///   bootstrapping a worker that will sit permanently idle (AC 2).
fn render_token_ranking() -> String {
    let export_line = path_bootstrap::canonical_path_export_line();
    format!(
        r#"set -e
{export_line}OUT="$(loom-daemon tokens check --ranking --shared 2>/dev/null)" || OUT="$(loom-daemon tokens check --ranking)"
printf '%s\n' "$OUT"
# `|| true` on every extraction below: an absent "Total …" line, or one with
# no "N available" substring (a genuinely all-blocked/exhausted pool), is a
# *result* to report via the WARNING/note below, not a script-ending grep
# failure — under `set -e`, a failing (no-match) grep at the tail of a
# `VAR="$(...)"` command substitution would otherwise abort the script right
# here, silently, before either the note or the WARNING is ever printed.
TOTAL_LINE="$(printf '%s\n' "$OUT" | grep -E '^Total ' | tail -n1 || true)"
TOTAL="$(printf '%s\n' "$TOTAL_LINE" | sed -n 's/^Total \([0-9][0-9]*\):.*/\1/p' || true)"
AVAILABLE="$(printf '%s\n' "$TOTAL_LINE" | grep -oE '[0-9]+ available' | head -n1 | grep -oE '^[0-9]+' || true)"
TOTAL="${{TOTAL:-0}}"
AVAILABLE="${{AVAILABLE:-0}}"
echo "{CHECKLIST_NOTE_PREFIX}token pool ${{AVAILABLE}}/${{TOTAL}} accounts available"
if [ "$TOTAL" -gt 0 ] && [ "$AVAILABLE" -eq 0 ]; then
  echo "WARNING: token pool bootstrapped with 0/${{TOTAL}} accounts available -- this worker will sit idle until a live accounts.env is installed and re-bootstrapped (loom-daemon tokens bootstrap --shared --force)" >&2
  exit 1
fi
"#
    )
}

fn render_workspace_clone_check(repos: &[String]) -> String {
    let mut s = String::from("set -e\n");
    for repo in repos {
        let rel = workspace_rel(repo);
        s.push_str(&format!("test -d \"$HOME/{rel}/.git\" || exit 1\n"));
    }
    s
}

/// Render the workspace clone + first-time init step.
///
/// **`loom-daemon init` runs only on a workspace that is not yet configured**
/// (issue #4641). It used to run unconditionally — unlike the `gh repo clone`
/// above it, which has always been guarded by `[ ! -d .../.git ]` — so every
/// re-run of `fleet add-worker` (or any idempotent self-heal pass over an
/// existing host) re-entered `init`'s `.loom/config.json` merge on a workspace
/// an operator had since hand-tuned. That merge is existing-wins and normally
/// harmless, but its invalid-JSON fallback branch overwrites the file
/// wholesale, which is a plausible cause of the observed silent reversion of
/// `autonomous.workFinder.maxConcurrent` on `loom-worker-1`.
///
/// Provisioning only needs `init` to *establish* a workspace; keeping an
/// already-established one up to date is `loom update` / the resync script's
/// job, neither of which touches `.loom/config.json`. Gating on
/// `.loom/config.json` (rather than on having just cloned) also self-heals a
/// workspace that was cloned by a previous run whose `init` failed.
fn render_workspace_clone(repos: &[String]) -> String {
    let export_line = path_bootstrap::canonical_path_export_line();
    let mut s = format!(
        r#"set -e
{export_line}LOOM_SRC="$HOME/.local/share/loom"
mkdir -p "$HOME/loom-workspaces"
"#
    );
    for repo in repos {
        let rel = workspace_rel(repo);
        s.push_str(&format!(
            r#"if [ ! -d "$HOME/{rel}/.git" ]; then
  gh repo clone {repo} "$HOME/{rel}"
fi
if [ ! -f "$HOME/{rel}/.loom/config.json" ]; then
  loom-daemon init "$HOME/{rel}" --defaults "$LOOM_SRC/defaults" || true
else
  echo "skip init: $HOME/{rel} already configured (.loom/config.json present)"
fi
"#
        ));
    }
    s
}

fn render_workspace_register_check(repos: &[String]) -> String {
    let export_line = path_bootstrap::canonical_path_export_line();
    let mut s = format!(
        r#"set -e
{export_line}LIST="$(loom-daemon workspace list --json 2>/dev/null || echo '{{}}')"
"#
    );
    for repo in repos {
        let rel = workspace_rel(repo);
        let name = repo_dir_name(repo);
        s.push_str(&format!("echo \"$LIST\" | grep -q \"{name}\" || exit 1  # {rel}\n"));
    }
    s
}

fn render_workspace_register(repos: &[String], priority: u32) -> String {
    let export_line = path_bootstrap::canonical_path_export_line();
    let mut s = format!(
        r#"set -e
{export_line}"#
    );
    for repo in repos {
        let rel = workspace_rel(repo);
        s.push_str(&format!("loom-daemon workspace add \"$HOME/{rel}\" --priority {priority}\n"));
    }
    s
}

fn render_daemon_unit(primary_rel: &str) -> String {
    // WorkingDirectory pinned to a workspace clone is the #4292 token-pool cwd
    // workaround — REMOVE once #4292 lands (token pool no longer cwd-coupled).
    //
    // Restart=on-success + Environment=LOOM_DAEMON_SUPERVISOR=systemd mirror the
    // canonical systemd --user unit rendered by `render_systemd_unit()` in
    // `loom-daemon-start.sh` (#4268) — the same supervised-restart contract
    // documented in `ipc.rs` (`detect_supervisor` / `EXIT_RESTART` /
    // `EXIT_SIGINT` / `EXIT_SHUTDOWN`, #4054). `LOOM_DAEMON_SUPERVISOR=systemd`
    // lets the daemon prove it is supervised before `restart --drain` exits it
    // (#4640); `Restart=on-success` relaunches ONLY on the clean `EXIT_RESTART`
    // (exit 0) the restart primitive uses, and leaves the daemon down on
    // `EXIT_SIGINT` (130) / `EXIT_SHUTDOWN` (143) / a crash — deliberately NOT
    // `Restart=on-failure`, which would do the opposite (never relaunch a
    // requested restart, but crash-loop-relaunch is watchdog territory the
    // fleet unit does not attempt).
    //
    // Environment=PATH= below is rendered from the SAME canonical set
    // (path_bootstrap::canonical_path_systemd(), #4831) as
    // resolve_plist_path() in loom-daemon-start.sh — previously this was a
    // THIRD, narrower, hand-hardcoded PATH
    // (`%h/.local/bin:/usr/local/bin:/usr/bin:/bin`, missing `%h/.cargo/bin`
    // and Homebrew) that disagreed with both the launchd/systemd plist
    // renderer and this same function's own `export PATH=` line above it.
    let export_line = path_bootstrap::canonical_path_export_line();
    let systemd_path = path_bootstrap::canonical_path_systemd();
    format!(
        r#"set -e
{export_line}mkdir -p "$HOME/.config/systemd/user"
cat > "$HOME/.config/systemd/user/loom-daemon.service" <<'UNIT'
[Unit]
Description=Loom daemon (fleet worker)
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
# WORKAROUND(#4292): the token pool resolves via the daemon's cwd, so pin the
# working directory to a workspace clone whose .loom/tokens/ the daemon can
# find. Remove this WorkingDirectory line once #4292 (cwd-decoupled token pool)
# lands.
WorkingDirectory=%h/{primary_rel}
ExecStart=%h/.local/bin/loom-daemon
# Restart=on-success == the launchd KeepAlive:{{SuccessfulExit:true}} analog
# (#4054, mirrored from loom-daemon-start.sh's render_systemd_unit): only the
# clean EXIT_RESTART (0) relaunches; EXIT_SIGINT (130) / EXIT_SHUTDOWN (143) /
# a crash all exit non-zero and stay down.
Restart=on-success
Environment=PATH={systemd_path}
# Lets detect_supervisor() (ipc.rs) prove this daemon is supervised so
# `restart --drain` will actually exit for a relaunch instead of refusing.
Environment=LOOM_DAEMON_SUPERVISOR=systemd

[Install]
WantedBy=default.target
UNIT
# Linger so the user manager (and the daemon) survive logout + reboot on an
# SSH-only host.
loginctl enable-linger "$USER" 2>/dev/null || true
systemctl --user daemon-reload
systemctl --user enable --now loom-daemon.service
"#
    )
}

fn render_idle_shutdown(minutes: u32) -> String {
    // Stage 2 of power-off: autonomous.idleExit is stage 1. A running daemon
    // remains a veto because only it can interpret queued/rate-limited work.
    // This guard script runs unattended out of cron (#4831: cron's PATH is
    // typically minimal/empty), so it needs the full canonical PATH, not
    // just ~/.local/bin, to reliably find `loom-daemon`.
    let export_line = path_bootstrap::canonical_path_export_line();
    format!(
        r#"set -e
mkdir -p "$HOME/.local/bin"
cat > "$HOME/.local/bin/loom-idle-shutdown.sh" <<'GUARD'
#!/usr/bin/env bash
# loom-idle-shutdown (#4341/#4467), stage 2. autonomous.idleExit must first
# stop loom-daemon; this guard remains the sole power-off authority.
set -euo pipefail
{export_line}LIMIT={minutes}
STAMP="$HOME/.loom/last-active"
mkdir -p "$HOME/.loom"
active=0
if pgrep -x claude >/dev/null 2>&1; then active=1; fi
if pgrep -f '[l]oom-daemon' >/dev/null 2>&1; then active=1; fi
if loom-daemon status --json 2>/dev/null | grep -Eq '"active_sweeps"[[:space:]]*:[[:space:]]*[1-9]'; then active=1; fi
if [ "$active" = "1" ]; then
  date +%s > "$STAMP"
  exit 0
fi
now=$(date +%s)
last=$(cat "$STAMP" 2>/dev/null || echo "$now")
idle_min=$(( (now - last) / 60 ))
if [ "$idle_min" -ge "$LIMIT" ]; then
  sudo systemctl poweroff
fi
GUARD
chmod 0755 "$HOME/.local/bin/loom-idle-shutdown.sh"
# Run the guard every 5 minutes.
( crontab -l 2>/dev/null | grep -v loom-idle-shutdown; echo "*/5 * * * * $HOME/.local/bin/loom-idle-shutdown.sh >/dev/null 2>&1" ) | crontab -
"#
    )
}

/// Verify: daemon reachable from a registered workspace root, ranking fresh,
/// workspace(s) registered.
///
/// Two fixes from the 2026-08-04 loom-worker-2 pilot (#5334):
///
/// - **cwd**: `cd "$HOME/{{primary_rel}}"` used to swallow a failed `cd` with
///   `|| true` and silently carry on from whatever cwd the SSH session
///   started in (never a registered workspace root) — now a missing
///   workspace root fails loudly instead.
/// - **race**: this step used to run `loom-daemon status` exactly once,
///   immediately after `daemon-unit` (re)started the systemd unit — a real
///   run observed 12/13 steps green, halted only because `status` was probed
///   before the daemon finished starting (the identical command succeeded
///   seconds later by hand). Retries now bound the wait to ~30s (15 attempts
///   × 2s) before failing.
fn render_verify(primary_rel: &str, repos: &[String]) -> String {
    let mut checks = String::new();
    for repo in repos {
        let name = repo_dir_name(repo);
        checks.push_str(&format!(
            "echo \"$LIST\" | grep -q \"{name}\" || {{ echo \"workspace {name} not registered\" >&2; exit 1; }}\n"
        ));
    }
    let export_line = path_bootstrap::canonical_path_export_line();
    format!(
        r#"set -e
{export_line}WORKSPACE_ROOT="$HOME/{primary_rel}"
cd "$WORKSPACE_ROOT" || {{ echo "verify: workspace root $WORKSPACE_ROOT not found (workspace-clone step must run first)" >&2; exit 1; }}
# Daemon reachable + status sane from the workspace cwd. The daemon may still
# be finishing startup right after daemon-unit's `systemctl --user enable
# --now` (re)started it -- retry with a bounded ~30s wait instead of failing
# on the very first race (#5334).
STATUS_OK=0
for _attempt in $(seq 1 15); do
  if loom-daemon status >/dev/null 2>&1; then
    STATUS_OK=1
    break
  fi
  sleep 2
done
if [ "$STATUS_OK" != "1" ]; then
  echo "loom-daemon status did not become ready within ~30s" >&2
  exit 1
fi
# Token ranking is present + fresh (bootstrap + check ran).
test -f "$HOME/.loom/tokens/.ranking" || test -f "$HOME/{primary_rel}/.loom/tokens/.ranking" \
  || {{ echo "no token ranking found" >&2; exit 1; }}
# Workspace(s) registered — the dispatch target must resolve from the registry (#4299).
LIST="$(loom-daemon workspace list --json 2>/dev/null || echo '{{}}')"
{checks}"#
    )
}

// ---------------------------------------------------------------------------
// Safehouse spin-up templates (#3998)
// ---------------------------------------------------------------------------

fn render_safehouse_tailscale_install() -> String {
    // Codename-derived apt repo (works across Ubuntu releases, not just the
    // pilot's 24.04/"noble") — the same approach `base-deps` uses for the gh
    // apt repo.
    r#"set -e
if ! command -v tailscale >/dev/null 2>&1; then
  CODENAME="$(lsb_release -cs 2>/dev/null || echo noble)"
  curl -fsSL "https://pkgs.tailscale.com/stable/ubuntu/${CODENAME}.noarmor.gpg" | sudo tee /usr/share/keyrings/tailscale-archive-keyring.gpg >/dev/null
  curl -fsSL "https://pkgs.tailscale.com/stable/ubuntu/${CODENAME}.tailscale-keyring.list" | sudo tee /etc/apt/sources.list.d/tailscale.list >/dev/null
  sudo apt-get update -qq
  sudo apt-get install -y tailscale
fi
sudo systemctl enable --now tailscaled 2>/dev/null || true
"#
    .to_string()
}

fn render_safehouse_tailscale_join() -> String {
    // The auth key arrives on stdin; written to a 0600 tempfile and fed to
    // `tailscale up` via the `file:` prefix so the raw key never appears on
    // the command line (where `ps` on the worker would expose it) or in a
    // rendered plan/log line. The key is operator-minted ephemeral +
    // tag:loom-worker (epic #4340's boundary: loom never calls the Tailscale
    // API itself), so no --advertise-tags flag is needed here — the tag is
    // baked into the key server-side.
    r#"set -e
umask 077
KEY_FILE="$(mktemp)"
cat > "$KEY_FILE"
chmod 600 "$KEY_FILE"
sudo tailscale up --auth-key="file:$KEY_FILE" --hostname="$(hostname -s)" --ssh=false
rm -f "$KEY_FILE"
"#
    .to_string()
}

fn render_safehouse_build(safehouse_repo_url: &str) -> String {
    format!(
        r#"set -e
. "$HOME/.cargo/env" 2>/dev/null || true
SH_SRC="$HOME/.local/share/safehouse"
if [ -d "$SH_SRC/.git" ]; then
  git -C "$SH_SRC" pull --ff-only
else
  mkdir -p "$(dirname "$SH_SRC")"
  git clone {safehouse_repo_url} "$SH_SRC"
fi
cd "$SH_SRC"
cargo build --release -p safehoused
mkdir -p "$HOME/.local/bin"
install -m 0755 "$SH_SRC/target/release/safehoused" "$HOME/.local/bin/safehoused"
"#
    )
}

/// `homeserver`/`room` are pre-validated shell-safe (`validate_safe_token`) by
/// [`preflight`] before this is ever called. `personas` are pre-validated by
/// [`validate_persona_name`] — safe to interpolate as bare TOML string
/// literals. The Matrix account + store/recovery passphrases arrive on stdin
/// as a `KEY=VALUE` env file and are sourced into shell variables that are
/// referenced (never literal) in the rendered heredoc, so the secret VALUES
/// never appear in this rendered text (only the variable NAMES do).
fn render_safehouse_config(homeserver: &str, room: &str, personas: &[String]) -> String {
    let persona_list = personas
        .iter()
        .map(|p| format!("\"{p}\""))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        r#"set -e
umask 077
mkdir -p "$HOME/.loom/safehoused"
ENV_FILE="$(mktemp)"
cat > "$ENV_FILE"
chmod 600 "$ENV_FILE"
set -a
. "$ENV_FILE"
set +a
cat > "$HOME/.loom/safehoused/config.toml" <<TOML
homeserver_url = "{homeserver}"
room = "{room}"
personas = [{persona_list}]

[account]
user_id = "$SAFEHOUSE_MATRIX_USER_ID"
password = "$SAFEHOUSE_MATRIX_PASSWORD"

[encryption]
store_passphrase = "$SAFEHOUSE_STORE_PASSPHRASE"
recovery_passphrase = "$SAFEHOUSE_RECOVERY_PASSPHRASE"
TOML
chmod 600 "$HOME/.loom/safehoused/config.toml"
rm -f "$ENV_FILE"
"#
    )
}

fn render_safehouse_room_invite(invite_exec: Option<&str>) -> String {
    let exec = invite_exec.unwrap_or(DEFAULT_SAFEHOUSE_INVITE_EXEC);
    let export_line = path_bootstrap::canonical_path_export_line();
    format!(
        r#"set -e
{export_line}{exec}
touch "$HOME/.loom/safehoused/.invited"
"#
    )
}

fn render_safehouse_supervise(primary_rel: &str) -> String {
    // Reuses the already-shipped `safehoused-service.sh` (cloned onto the
    // worker by the workspace-clone step's `loom-daemon init`) rather than a
    // one-off `systemd-run` invocation, so the worker gets the same
    // Restart=always + linger-aware supervision contract the interactive-host
    // runbook documents (`.loom/docs/safehouse.md`). `--socket` is pinned to
    // a deterministic path (not the config-driven resolver) so a fresh worker
    // with no prior `.loom/config.json` safehouse block still agrees with the
    // env this plan wires into loom-daemon in the next step.
    let export_line = path_bootstrap::canonical_path_export_line();
    format!(
        r#"set -e
{export_line}"$HOME/{primary_rel}/.loom/scripts/cli/safehoused-service.sh" install \
  --bin "$HOME/.local/bin/safehoused" \
  --socket "{SAFEHOUSE_SOCKET_PATH}" \
  --config "$HOME/.loom/safehoused/config.toml"
loginctl enable-linger "$USER" 2>/dev/null || true
"#
    )
}

fn render_safehouse_daemon_restart(room: &str) -> String {
    // Patches the loom-daemon unit's env in place (idempotent: strips any
    // prior LOOM_SAFEHOUSE_* lines before re-inserting) rather than a
    // worker-side .loom/config.json edit — #3997's decision that loom's
    // safehouse config is wired entirely via env on a fleet worker.
    format!(
        r#"set -e
UNIT="$HOME/.config/systemd/user/loom-daemon.service"
test -f "$UNIT" || {{ echo "loom-daemon systemd unit not found (the daemon-unit step must run first)" >&2; exit 1; }}
sed -i '/^Environment=LOOM_SAFEHOUSE_/d' "$UNIT"
sed -i "/^Environment=PATH=/a Environment=LOOM_SAFEHOUSE_ENABLED=true\nEnvironment=LOOM_SAFEHOUSE_SOCKET={SAFEHOUSE_SOCKET_PATH}\nEnvironment=LOOM_SAFEHOUSE_ROOM={room}" "$UNIT"
systemctl --user daemon-reload
systemctl --user restart loom-daemon.service
"#
    )
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::super::PlanEntry;
    use super::*;

    fn base_config() -> AddWorkerConfig {
        AddWorkerConfig {
            ssh_host: "worker-1".to_string(),
            repos: vec!["rjwalters/anvil".to_string()],
            priority: 50,
            dry_run: false,
            loom_repo_url: DEFAULT_LOOM_REPO_URL.to_string(),
            pat_file: None,
            accounts_env_file: None,
            safehouse_enabled: false,
            idle_shutdown_minutes: None,
            safehouse_tailnet_auth_key_file: None,
            safehouse_secrets_file: None,
            safehouse_repo_url: DEFAULT_SAFEHOUSE_REPO_URL.to_string(),
            safehouse_homeserver_url: None,
            safehouse_room: None,
            safehouse_personas: Vec::new(),
            safehouse_invite_exec: None,
        }
    }

    /// A full, valid `--safehouse` config — every required input present and
    /// well-formed. Individual tests mutate a field to exercise a specific
    /// failure.
    fn safehouse_config() -> AddWorkerConfig {
        let mut config = base_config();
        config.safehouse_enabled = true;
        config.safehouse_tailnet_auth_key_file = Some(PathBuf::from("/does/not/matter"));
        config.safehouse_secrets_file = Some(PathBuf::from("/does/not/matter"));
        config.safehouse_homeserver_url = Some("matrix.internal.example".to_string());
        config.safehouse_room = Some("!fleet:matrix.internal.example".to_string());
        config.safehouse_personas = vec!["loom_daemon".to_string()];
        config
    }

    fn safehouse_secrets() -> Secrets {
        Secrets {
            pat: None,
            accounts_env: None,
            safehouse_tailnet_auth_key: Some("tskey-auth-ephemeral-tagged".to_string()),
            safehouse_secrets: Some(
                "SAFEHOUSE_MATRIX_USER_ID=@safehoused-worker1:matrix.internal.example\n\
                 SAFEHOUSE_MATRIX_PASSWORD=hunter2-matrix-pw\n\
                 SAFEHOUSE_STORE_PASSPHRASE=store-pass-xyz\n\
                 SAFEHOUSE_RECOVERY_PASSPHRASE=recovery-pass-xyz\n"
                    .to_string(),
            ),
        }
    }

    // ---- validation / preflight ----------------------------------------

    #[test]
    fn validate_repo_accepts_slug_and_rejects_injection() {
        assert!(validate_repo("rjwalters/anvil").is_ok());
        assert!(validate_repo("owner/name.with-dots_and-dashes").is_ok());
        assert!(validate_repo("no-slash").is_err());
        assert!(validate_repo("owner/name; rm -rf /").is_err());
        assert!(validate_repo("owner/$(whoami)").is_err());
        assert!(validate_repo("").is_err());
    }

    #[test]
    fn repo_dir_name_takes_last_segment() {
        assert_eq!(repo_dir_name("rjwalters/anvil"), "anvil");
        assert_eq!(repo_dir_name("a/b/c"), "c");
    }

    #[test]
    fn preflight_requires_a_repo() {
        let mut config = base_config();
        config.repos.clear();
        assert!(preflight(&config).is_err());
    }

    #[test]
    fn preflight_rejects_empty_host() {
        let mut config = base_config();
        config.ssh_host = "   ".to_string();
        assert!(preflight(&config).is_err());
    }

    #[test]
    fn preflight_missing_pat_file_fails_before_remote() {
        let mut config = base_config();
        config.pat_file = Some(PathBuf::from("/nonexistent/pat-file-xyz"));
        let err = preflight(&config).unwrap_err().to_string();
        assert!(err.contains("pat-file"), "err: {err}");
    }

    #[test]
    fn preflight_missing_accounts_env_fails_before_remote() {
        let mut config = base_config();
        config.accounts_env_file = Some(PathBuf::from("/nonexistent/accounts-xyz.env"));
        assert!(preflight(&config).is_err());
    }

    #[test]
    fn preflight_reads_secret_files_and_trims_pat() {
        let dir = tempfile::tempdir().unwrap();
        let pat = dir.path().join("pat");
        let accounts = dir.path().join("accounts.env");
        std::fs::write(&pat, "  github_pat_abc123\n").unwrap();
        std::fs::write(&accounts, "ACCOUNT_EMAIL_1=a@b.c\n").unwrap();
        let mut config = base_config();
        config.pat_file = Some(pat);
        config.accounts_env_file = Some(accounts);
        let secrets = preflight(&config).unwrap();
        assert_eq!(secrets.pat.as_deref(), Some("github_pat_abc123"));
        assert!(secrets
            .accounts_env
            .as_deref()
            .unwrap()
            .contains("ACCOUNT_EMAIL_1"));
    }

    #[test]
    fn preflight_empty_pat_file_fails() {
        let dir = tempfile::tempdir().unwrap();
        let pat = dir.path().join("pat");
        std::fs::write(&pat, "   \n").unwrap();
        let mut config = base_config();
        config.pat_file = Some(pat);
        assert!(preflight(&config).is_err());
    }

    // ---- plan shape ----------------------------------------------------

    #[test]
    fn plan_step_ordering_matches_the_eight_pilot_steps() {
        let config = base_config();
        // Full secrets so the token/forge steps are executable, not skipped.
        let secrets = Secrets {
            pat: Some("pat".to_string()),
            accounts_env: Some("ACCOUNT_EMAIL_1=a@b.c".to_string()),
            ..Secrets::default()
        };
        let plan = build_plan(&config, &secrets);
        let names: Vec<&str> = plan
            .entries
            .iter()
            .map(super::super::PlanEntry::name)
            .collect();
        assert_eq!(
            names,
            vec![
                "base-deps",
                "machine-layout",
                "claude-code",
                "forge-auth",
                "token-accounts",
                "token-pool",
                "token-ranking",
                "workspace-clone",
                "workspace-register",
                "daemon-unit",
                "idle-shutdown",
                "safehouse",
                "verify",
            ]
        );
    }

    #[test]
    fn workspace_clone_only_inits_an_unconfigured_workspace() {
        // #4641: `loom-daemon init` used to run unconditionally here, so every
        // re-run of provisioning re-entered the `.loom/config.json` merge on a
        // workspace an operator had since hand-tuned. The init call must now sit
        // behind a `.loom/config.json` existence guard, the same way the clone
        // sits behind a `.git` guard.
        let script = render_workspace_clone(&["rjwalters/anvil".to_string()]);

        assert!(
            script.contains(r#"if [ ! -f "$HOME/loom-workspaces/anvil/.loom/config.json" ]; then"#),
            "init must be guarded on the workspace being unconfigured:\n{script}"
        );

        // The guard must actually enclose the init call: the only `loom-daemon
        // init` line has to appear after the guard and before its `else`.
        let guard = script
            .find(r#"if [ ! -f "$HOME/loom-workspaces/anvil/.loom/config.json" ]"#)
            .expect("guard present");
        let init = script.find("loom-daemon init").expect("init present");
        let else_branch = script.find("\nelse\n").expect("else present");
        assert!(guard < init && init < else_branch, "init is outside the guard:\n{script}");
        assert_eq!(
            script.matches("loom-daemon init").count(),
            1,
            "no second, unguarded init call may remain:\n{script}"
        );

        // The clone itself stays guarded on .git (unchanged behavior).
        assert!(
            script.contains(r#"if [ ! -d "$HOME/loom-workspaces/anvil/.git" ]; then"#),
            "clone guard regressed:\n{script}"
        );
    }

    #[test]
    fn base_deps_check_includes_libsqlite3_dev() {
        // safehouse#38: libsqlite3-dev must be part of the base deps.
        let plan = build_plan(&base_config(), &Secrets::default());
        let base = plan
            .entries
            .iter()
            .find_map(|e| match e {
                super::super::PlanEntry::Step(s) if s.name == "base-deps" => Some(s),
                _ => None,
            })
            .unwrap();
        assert!(base.check.as_ref().unwrap().contains("libsqlite3-dev"));
        assert!(base.apply.contains("libsqlite3-dev"));
    }

    // ---- machine-layout: artifact-first provisioning (#5067, Epic #4990 Phase 4) ----

    fn machine_layout_step() -> Step {
        let plan = build_plan(&base_config(), &Secrets::default());
        plan.entries
            .iter()
            .find_map(|e| match e {
                super::super::PlanEntry::Step(s) if s.name == "machine-layout" => Some(s.clone()),
                _ => None,
            })
            .unwrap()
    }

    #[test]
    fn base_deps_no_longer_installs_or_requires_a_rust_toolchain() {
        // AC: base-deps no longer requires rustup/a Rust toolchain on the
        // happy path (an artifact resolves for this host's platform); it is
        // only pulled in — by machine-layout, reactively — as a fallback
        // dependency when the build path is actually taken.
        let plan = build_plan(&base_config(), &Secrets::default());
        let base = plan
            .entries
            .iter()
            .find_map(|e| match e {
                super::super::PlanEntry::Step(s) if s.name == "base-deps" => Some(s),
                _ => None,
            })
            .unwrap();
        assert!(
            !base.apply.contains("rustup"),
            "base-deps must not install rustup unconditionally:\n{}",
            base.apply
        );
        assert!(
            !base.check.as_ref().unwrap().contains("cargo"),
            "base-deps' idempotency check must not require cargo (it no longer installs it):\n{}",
            base.check.as_ref().unwrap()
        );
        // gh remains required (needed for artifact resolution + downloads).
        assert!(base.check.as_ref().unwrap().contains("command -v gh"));
    }

    #[test]
    fn machine_layout_attempts_artifact_fetch_before_any_toolchain_fallback() {
        // AC: machine-layout attempts a release-artifact fetch first (reusing
        // Phase 3's already-tested fetch/verify/checksum logic by shelling
        // out to loom-daemon-update.sh's own "auto" resolution), falling back
        // to `cargo build -p loom-daemon --release` (installing rustup first)
        // only when that invocation fails.
        let step = machine_layout_step();
        let script = &step.apply;

        // Delegates to loom-daemon-update.sh (Phase 3, #5020) rather than
        // duplicating the fetch/verify/checksum implementation.
        let update_call = script
            .find(r#"defaults/scripts/cli/loom-daemon-update.sh""#)
            .expect(
                "machine-layout must invoke loom-daemon-update.sh to reuse Phase 3's \
                 fetch/verify/checksum logic",
            );

        // The toolchain fallback (rustup install + `cargo build` retry) must
        // sit strictly INSIDE the `if ! "$UPDATE_SCRIPT" ...; then` failure
        // branch — i.e. after the first invocation and gated on `cargo`
        // being absent — so it is never reached on the artifact-available
        // happy path.
        let toolchain_fallback = script
            .find("installing rustup as a fallback dependency")
            .expect("a rustup-install fallback must exist for when no artifact resolves");
        let cargo_guard = script
            .find("if ! command -v cargo")
            .expect("the toolchain fallback must be gated on cargo being absent");
        assert!(
            update_call < cargo_guard && cargo_guard < toolchain_fallback,
            "artifact-fetch attempt must precede the cargo-absent guard, which must precede \
             the rustup install:\n{script}"
        );

        // No literal `cargo build` shell invocation: the actual build now
        // happens inside loom-daemon-update.sh's own (already-tested)
        // fallback path, not duplicated here.
        assert!(
            !script.contains("cargo build --release")
                && !script
                    .lines()
                    .any(|l| l.trim_start().starts_with("cargo build")),
            "machine-layout must not duplicate the source-build invocation; it delegates to \
             loom-daemon-update.sh instead:\n{script}"
        );

        // --no-restart: fleet add-worker has not installed the loom-daemon
        // systemd unit yet at this point in the plan, so there is never a
        // running daemon for this invocation to try to restart.
        assert!(
            script.contains(r#""$UPDATE_SCRIPT" --no-restart"#),
            "loom-daemon-update.sh must be invoked with --no-restart:\n{script}"
        );
    }

    #[test]
    fn machine_layout_toolchain_fallback_retries_the_same_update_script() {
        // The retry after installing rustup must call the SAME update-script
        // invocation (not a hand-rolled `cargo build`), so it goes through
        // the identical fetch/verify/checksum + build logic a second time
        // (now with cargo available) rather than a second implementation.
        let step = machine_layout_step();
        let script = &step.apply;
        assert_eq!(
            script.matches(r#""$UPDATE_SCRIPT" --no-restart"#).count(),
            2,
            "expected exactly two invocations (initial attempt + one retry after installing \
             rustup):\n{script}"
        );
    }

    #[test]
    fn machine_layout_idempotency_check_unchanged() {
        // AC: the plan's idempotency check (test -x .../loom-daemon) and
        // existing re-run semantics are preserved — a re-run of `fleet
        // add-worker` against an already-provisioned host still no-ops this
        // step.
        let step = machine_layout_step();
        assert_eq!(step.check.as_deref(), Some(r#"test -x "$HOME/.local/bin/loom-daemon""#));
    }

    #[test]
    fn machine_layout_rendered_script_is_valid_shell() {
        // Sanity-check the generated script actually parses as shell (bash -n
        // — syntax check only, no execution) — catches an unbalanced
        // if/fi or quoting mistake in the artifact-fetch/fallback wiring
        // that a pure string-content assertion would miss.
        let step = machine_layout_step();
        let output = std::process::Command::new("bash")
            .arg("-n")
            .arg("-c")
            .arg(&step.apply)
            .output();
        match output {
            Ok(out) => assert!(
                out.status.success(),
                "rendered machine-layout script failed `bash -n`:\n{}\nscript:\n{}",
                String::from_utf8_lossy(&out.stderr),
                step.apply
            ),
            Err(e) => {
                // bash not available in this environment — skip rather than fail.
                eprintln!("skipping bash -n check: could not launch bash ({e})");
            }
        }
    }

    #[test]
    fn forge_auth_and_token_accounts_carry_secret_stdin() {
        let config = base_config();
        let secrets = Secrets {
            pat: Some("the-pat".to_string()),
            accounts_env: Some("ACCOUNT_EMAIL_1=a@b.c".to_string()),
            ..Secrets::default()
        };
        let plan = build_plan(&config, &secrets);
        for name in ["forge-auth", "token-accounts"] {
            let step = plan
                .entries
                .iter()
                .find_map(|e| match e {
                    super::super::PlanEntry::Step(s) if s.name == name => Some(s),
                    _ => None,
                })
                .unwrap();
            let stdin = step.stdin.as_ref().expect("secret step must carry stdin");
            assert!(stdin.secret, "{name} stdin must be marked secret");
        }
        // The apply strings must NOT embed the secret values (stdin only).
        let dry = plan.render_dry_run("fleet add-worker", "worker-1");
        assert!(!dry.contains("the-pat"));
    }

    #[test]
    fn missing_secrets_become_skips_not_failures() {
        // No PAT, no accounts.env → those steps are skip-with-notice.
        let plan = build_plan(&base_config(), &Secrets::default());
        for name in [
            "forge-auth",
            "token-accounts",
            "token-pool",
            "token-ranking",
        ] {
            let entry = plan.entries.iter().find(|e| e.name() == name).unwrap();
            assert!(
                matches!(entry, super::super::PlanEntry::Skip { .. }),
                "{name} should be a skip when its secret is absent"
            );
        }
    }

    // ---- verify: daemon-readiness retry + strict workspace cwd (#5334) ----

    #[test]
    fn verify_cd_into_workspace_root_fails_loudly_instead_of_falling_through() {
        // A missing workspace root used to be swallowed by `|| true` and the
        // rest of the script ran from whatever cwd the SSH session started
        // in (never a registered workspace). It must now fail loudly, naming
        // the expected root.
        let script = render_verify("loom-workspaces/anvil", &["rjwalters/anvil".to_string()]);
        assert!(
            !script.contains("|| true"),
            "verify must not silently swallow a failed cd:\n{script}"
        );
        assert!(script.contains(r#"cd "$WORKSPACE_ROOT""#), "verify:\n{script}");
        assert!(script.contains("workspace root $WORKSPACE_ROOT not found"), "verify:\n{script}");
    }

    #[test]
    fn verify_retries_daemon_status_with_a_bounded_wait() {
        let script = render_verify("loom-workspaces/anvil", &["rjwalters/anvil".to_string()]);
        assert!(
            script.contains("for _attempt in $(seq 1 15)"),
            "verify must bound its daemon-readiness retry loop:\n{script}"
        );
        assert!(script.contains("sleep 2"), "verify:\n{script}");
        assert!(
            script.contains("loom-daemon status did not become ready within ~30s"),
            "verify:\n{script}"
        );
    }

    #[test]
    fn verify_rendered_script_is_valid_shell() {
        let script = render_verify("loom-workspaces/anvil", &["rjwalters/anvil".to_string()]);
        let output = Command::new("bash")
            .arg("-n")
            .arg("-c")
            .arg(&script)
            .output();
        match output {
            Ok(out) => assert!(
                out.status.success(),
                "rendered verify script failed `bash -n`:\n{}\nscript:\n{}",
                String::from_utf8_lossy(&out.stderr),
                script
            ),
            Err(e) => eprintln!("skipping bash -n check: could not launch bash ({e})"),
        }
    }

    #[test]
    fn verify_status_retry_loop_eventually_succeeds_past_early_failures() {
        // Functional check (not just string content): a `loom-daemon` that
        // fails its first two `status` calls (the exact startup race #5334
        // reports) and succeeds from the third must let the retry loop pass,
        // without needing to reach the full 15-attempt bound.
        let Some(bash) = which_bash() else {
            eprintln!("skipping: bash not available");
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        write_executable(
            &dir.path().join("loom-daemon"),
            r#"#!/bin/sh
if [ "$1" = "status" ]; then
  COUNTER_FILE="$STUB_STATE_DIR/status-calls"
  n=0
  [ -f "$COUNTER_FILE" ] && n="$(cat "$COUNTER_FILE")"
  n=$((n + 1))
  echo "$n" > "$COUNTER_FILE"
  if [ "$n" -lt 3 ]; then
    exit 1
  fi
  exit 0
fi
if [ "$1" = "workspace" ]; then
  echo '{"workspaces":["anvil"]}'
  exit 0
fi
exit 0
"#,
        );
        let repos = vec!["rjwalters/anvil".to_string()];
        let script = render_verify("loom-workspaces/anvil", &repos);
        // A tiny substitution so the test does not actually sleep 2s per
        // retry (still exercises the identical loop/branch structure).
        let fast_script = script.replace("sleep 2", "sleep 0.05");
        let home = dir.path().join("home");
        std::fs::create_dir_all(home.join("loom-workspaces/anvil")).unwrap();
        std::fs::create_dir_all(home.join(".loom/tokens")).unwrap();
        std::fs::write(home.join(".loom/tokens/.ranking"), "x").unwrap();
        let out = Command::new(bash)
            .arg("-c")
            .arg(&fast_script)
            .env("PATH", format!("{}:{}", dir.path().display(), std::env::var("PATH").unwrap()))
            .env("HOME", &home)
            .env("STUB_STATE_DIR", dir.path())
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "verify should pass once status recovers:\nstdout: {}\nstderr: {}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
    }

    // ---- token-ranking: health gate + checklist note (#5334) --------------

    #[test]
    fn token_ranking_marks_the_checklist_note_with_the_prefix_extract_checklist_note_expects() {
        let script = render_token_ranking();
        assert!(
            script.contains(CHECKLIST_NOTE_PREFIX),
            "token-ranking must use the shared checklist-note marker:\n{script}"
        );
    }

    #[test]
    fn token_ranking_rendered_script_is_valid_shell() {
        let script = render_token_ranking();
        let output = Command::new("bash")
            .arg("-n")
            .arg("-c")
            .arg(&script)
            .output();
        match output {
            Ok(out) => assert!(
                out.status.success(),
                "rendered token-ranking script failed `bash -n`:\n{}\nscript:\n{}",
                String::from_utf8_lossy(&out.stderr),
                script
            ),
            Err(e) => eprintln!("skipping bash -n check: could not launch bash ({e})"),
        }
    }

    #[test]
    fn token_ranking_reports_the_available_split_and_succeeds_when_some_available() {
        let Some(bash) = which_bash() else {
            eprintln!("skipping: bash not available");
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        write_executable(
            &dir.path().join("loom-daemon"),
            "#!/bin/sh\ncat <<'EOF'\nToken pool ranking (probed at 2026-08-04T00:00:00Z)\n====\nAccount  5h util  7d util  Status\n----\na-1  0.10  0.10  available\nb-2  0.20  0.20  available\nc-3  1.00  1.00  blocked\nd-4  1.00  1.00  exhausted\n\nTotal 4: 2 available, 1 blocked, 1 exhausted\nEOF\nexit 0\n",
        );
        let out = Command::new(bash)
            .arg("-c")
            .arg(render_token_ranking())
            .env("PATH", format!("{}:{}", dir.path().display(), std::env::var("PATH").unwrap()))
            .env("HOME", dir.path())
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "a pool with some available accounts must not fail:\nstdout: {}\nstderr: {}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            stdout.contains(&format!("{CHECKLIST_NOTE_PREFIX}token pool 2/4 accounts available")),
            "stdout: {stdout}"
        );
    }

    #[test]
    fn token_ranking_fails_loudly_when_every_account_is_blocked() {
        // The exact 2026-08-04 loom-worker-2 incident: `tokens check
        // --ranking` itself exits 0 (nothing is `error`/`skipped`), but every
        // account is `blocked` — zero `available`. This must halt the run,
        // not report a green "changed".
        let Some(bash) = which_bash() else {
            eprintln!("skipping: bash not available");
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        write_executable(
            &dir.path().join("loom-daemon"),
            "#!/bin/sh\ncat <<'EOF'\nToken pool ranking (probed at 2026-08-04T00:00:00Z)\n====\nAccount  5h util  7d util  Status\n----\na-1  1.00  1.00  blocked\nb-2  1.00  1.00  blocked\nc-3  1.00  1.00  blocked\nd-4  1.00  1.00  blocked\n\nTotal 4: 4 blocked\nEOF\nexit 0\n",
        );
        let out = Command::new(bash)
            .arg("-c")
            .arg(render_token_ranking())
            .env("PATH", format!("{}:{}", dir.path().display(), std::env::var("PATH").unwrap()))
            .env("HOME", dir.path())
            .output()
            .unwrap();
        assert!(
            !out.status.success(),
            "an all-blocked pool must fail the step, not bootstrap silently"
        );
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(stderr.contains("WARNING"), "stderr: {stderr}");
        assert!(stderr.contains("0/4"), "stderr: {stderr}");
    }

    /// Resolve a `bash` binary for the functional (actually-executes-shell)
    /// tests above, or `None` when the test environment has no bash — mirrors
    /// `machine_layout_rendered_script_is_valid_shell`'s skip-not-fail
    /// posture for a bash-less CI image.
    fn which_bash() -> Option<PathBuf> {
        // Resolve an *absolute* path up front (rather than the bare name
        // "bash") so overriding the child process's own `PATH` env (below,
        // to expose the stub `loom-daemon`) cannot also change which `bash`
        // binary gets exec'd.
        let out = Command::new("sh")
            .arg("-c")
            .arg("command -v bash")
            .output()
            .ok()?;
        if !out.status.success() {
            return None;
        }
        let path = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if path.is_empty() {
            None
        } else {
            Some(PathBuf::from(path))
        }
    }

    /// Write an executable shell-script stub at `path` (mode 0755).
    fn write_executable(path: &std::path::Path, contents: &str) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::write(path, contents).unwrap();
        let mut perms = std::fs::metadata(path).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(path, perms).unwrap();
    }

    #[test]
    fn safehouse_disabled_stays_a_single_skip_and_plain_worker_unchanged() {
        // AC: "Without --safehouse, behavior is unchanged: skip-with-notice,
        // plain worker, zero safehouse provisioning."
        let plan = build_plan(&base_config(), &Secrets::default());
        let names: Vec<&str> = plan.entries.iter().map(PlanEntry::name).collect();
        assert_eq!(names.iter().filter(|n| n.starts_with("safehouse")).count(), 1);
        let entry = plan
            .entries
            .iter()
            .find(|e| e.name() == "safehouse")
            .unwrap();
        match entry {
            PlanEntry::Skip { reason, .. } => assert!(
                reason.contains("not requested"),
                "reason should explain safehouse was not requested: {reason}"
            ),
            other => panic!("expected safehouse skip, got {other:?}"),
        }
    }

    // ---- safehouse enabled: real steps (#3998) --------------------------

    fn safehouse_step_names() -> Vec<&'static str> {
        vec![
            "safehouse-tailscale-install",
            "safehouse-tailscale-join",
            "safehouse-build",
            "safehouse-config",
            "safehouse-room-invite",
            "safehouse-supervise",
            "safehouse-daemon-restart",
        ]
    }

    #[test]
    fn safehouse_enabled_renders_full_step_sequence_in_order_between_idle_shutdown_and_verify() {
        let config = safehouse_config();
        let plan = build_plan(&config, &safehouse_secrets());
        let names: Vec<&str> = plan.entries.iter().map(PlanEntry::name).collect();

        // No bare "safehouse" skip entry remains once real steps render.
        assert!(!names.contains(&"safehouse"));

        let idle_pos = names.iter().position(|n| *n == "idle-shutdown").unwrap();
        let verify_pos = names.iter().position(|n| *n == "verify").unwrap();
        let safehouse_positions: Vec<usize> = safehouse_step_names()
            .iter()
            .map(|want| {
                names
                    .iter()
                    .position(|n| n == want)
                    .unwrap_or_else(|| panic!("missing step {want}"))
            })
            .collect();

        // Exact ordering, and the whole block sits between idle-shutdown and verify.
        let mut sorted = safehouse_positions.clone();
        sorted.sort_unstable();
        assert_eq!(
            safehouse_positions, sorted,
            "safehouse steps must render in the documented order"
        );
        assert!(safehouse_positions
            .iter()
            .all(|&p| p > idle_pos && p < verify_pos));
    }

    #[test]
    fn safehouse_config_step_precedes_supervise_step_boot_time_ordering_constraint() {
        // AC: the persona allowlist is written before safehoused's first
        // start (boot-time-only, no reload) — asserted directly on the
        // rendered plan's step order, not just by construction.
        let config = safehouse_config();
        let plan = build_plan(&config, &safehouse_secrets());
        let names: Vec<&str> = plan.entries.iter().map(PlanEntry::name).collect();
        let config_pos = names.iter().position(|n| *n == "safehouse-config").unwrap();
        let supervise_pos = names
            .iter()
            .position(|n| *n == "safehouse-supervise")
            .unwrap();
        assert!(
            config_pos < supervise_pos,
            "safehouse-config ({config_pos}) must precede safehouse-supervise ({supervise_pos})"
        );
    }

    #[test]
    fn safehouse_every_step_has_a_check_for_idempotent_rerun() {
        // AC: a re-run on an already-provisioned host reports AlreadyDone —
        // that classification is driven entirely by a present `check`.
        let config = safehouse_config();
        let plan = build_plan(&config, &safehouse_secrets());
        for want in safehouse_step_names() {
            let step = plan
                .entries
                .iter()
                .find_map(|e| match e {
                    PlanEntry::Step(s) if s.name == want => Some(s),
                    _ => None,
                })
                .unwrap_or_else(|| panic!("missing step {want}"));
            assert!(step.check.is_some(), "{want} must have a check phase (idempotency)");
        }
    }

    #[test]
    fn safehouse_full_plan_reruns_idempotently_when_every_check_passes() {
        struct AlwaysOkRunner;
        impl CommandRunner for AlwaysOkRunner {
            fn run(&self, _shell: &str, _stdin: Option<&str>) -> Result<CommandOutput> {
                Ok(CommandOutput {
                    code: 0,
                    stdout: String::new(),
                    stderr: String::new(),
                })
            }
        }
        let config = safehouse_config();
        let plan = build_plan(&config, &safehouse_secrets());
        let reports = super::super::execute_plan(&AlwaysOkRunner, &plan);
        for name in safehouse_step_names() {
            let report = reports.iter().find(|r| r.name == name).unwrap();
            assert_eq!(
                report.status,
                StepStatus::AlreadyDone,
                "{name} should be AlreadyDone on a passing check"
            );
        }
    }

    #[test]
    fn safehouse_tailscale_join_and_config_steps_carry_secret_stdin_not_in_rendered_text() {
        let config = safehouse_config();
        let secrets = safehouse_secrets();
        let plan = build_plan(&config, &secrets);

        for name in ["safehouse-tailscale-join", "safehouse-config"] {
            let step = plan
                .entries
                .iter()
                .find_map(|e| match e {
                    PlanEntry::Step(s) if s.name == name => Some(s),
                    _ => None,
                })
                .unwrap();
            let stdin = step.stdin.as_ref().expect("secret step must carry stdin");
            assert!(stdin.secret, "{name} stdin must be marked secret");
        }

        let dry = plan.render_dry_run("fleet add-worker", "worker-1");
        assert!(!dry.contains("tskey-auth-ephemeral-tagged"));
        assert!(!dry.contains("hunter2-matrix-pw"));
        assert!(!dry.contains("store-pass-xyz"));
        assert!(!dry.contains("recovery-pass-xyz"));
    }

    #[test]
    fn safehouse_config_step_apply_references_secret_vars_not_values() {
        let config = safehouse_config();
        let secrets = safehouse_secrets();
        let plan = build_plan(&config, &secrets);
        let step = plan
            .entries
            .iter()
            .find_map(|e| match e {
                PlanEntry::Step(s) if s.name == "safehouse-config" => Some(s),
                _ => None,
            })
            .unwrap();
        // Non-secret operator values ARE interpolated.
        assert!(step.apply.contains("matrix.internal.example"));
        assert!(step.apply.contains("!fleet:matrix.internal.example"));
        assert!(step.apply.contains("loom_daemon"));
        // Secret values are referenced only via their sourced variable names.
        assert!(step.apply.contains("$SAFEHOUSE_MATRIX_USER_ID"));
        assert!(step.apply.contains("$SAFEHOUSE_MATRIX_PASSWORD"));
        assert!(step.apply.contains("$SAFEHOUSE_STORE_PASSPHRASE"));
        assert!(step.apply.contains("$SAFEHOUSE_RECOVERY_PASSPHRASE"));
        assert!(!step.apply.contains("hunter2-matrix-pw"));
    }

    #[test]
    fn safehouse_room_invite_uses_the_daemon_side_op_not_raw_cs_api() {
        let config = safehouse_config();
        let plan = build_plan(&config, &safehouse_secrets());
        let step = plan
            .entries
            .iter()
            .find_map(|e| match e {
                PlanEntry::Step(s) if s.name == "safehouse-room-invite" => Some(s),
                _ => None,
            })
            .unwrap();
        assert!(step.apply.contains("safehoused invite"));
        assert!(!step.apply.to_lowercase().contains("client-server"));
        assert!(!step.apply.contains("/_matrix/client"));

        // An operator override replaces the default invocation.
        let mut overridden = safehouse_config();
        overridden.safehouse_invite_exec = Some("safehoused invite --room-override x".to_string());
        let plan2 = build_plan(&overridden, &safehouse_secrets());
        let step2 = plan2
            .entries
            .iter()
            .find_map(|e| match e {
                PlanEntry::Step(s) if s.name == "safehouse-room-invite" => Some(s),
                _ => None,
            })
            .unwrap();
        assert!(step2.apply.contains("--room-override x"));
    }

    #[test]
    fn safehouse_supervise_step_installs_via_the_shared_service_script_and_lingers() {
        let config = safehouse_config();
        let plan = build_plan(&config, &safehouse_secrets());
        let step = plan
            .entries
            .iter()
            .find_map(|e| match e {
                PlanEntry::Step(s) if s.name == "safehouse-supervise" => Some(s),
                _ => None,
            })
            .unwrap();
        assert!(step.apply.contains("safehoused-service.sh"));
        assert!(step.apply.contains("install"));
        assert!(step.apply.contains("enable-linger"));
    }

    #[test]
    fn safehouse_daemon_restart_step_wires_env_and_restarts() {
        let config = safehouse_config();
        let plan = build_plan(&config, &safehouse_secrets());
        let step = plan
            .entries
            .iter()
            .find_map(|e| match e {
                PlanEntry::Step(s) if s.name == "safehouse-daemon-restart" => Some(s),
                _ => None,
            })
            .unwrap();
        assert!(step.apply.contains("LOOM_SAFEHOUSE_ENABLED=true"));
        assert!(step.apply.contains("LOOM_SAFEHOUSE_SOCKET"));
        assert!(step
            .apply
            .contains("LOOM_SAFEHOUSE_ROOM=!fleet:matrix.internal.example"));
        assert!(step
            .apply
            .contains("systemctl --user restart loom-daemon.service"));
    }

    // ---- safehouse preflight ---------------------------------------------

    #[test]
    fn preflight_safehouse_enabled_requires_every_input() {
        let mut config = base_config();
        config.safehouse_enabled = true;
        let err = preflight(&config).unwrap_err().to_string();
        assert!(err.contains("--safehouse-tailnet-auth-key-file"), "err: {err}");
        assert!(err.contains("--safehouse-secrets-file"), "err: {err}");
        assert!(err.contains("--safehouse-homeserver-url"), "err: {err}");
        assert!(err.contains("--safehouse-room"), "err: {err}");
        assert!(err.contains("--safehouse-persona"), "err: {err}");
    }

    #[test]
    fn preflight_safehouse_enabled_with_full_inputs_reads_secrets() {
        let dir = tempfile::tempdir().unwrap();
        let key_file = dir.path().join("tailnet.key");
        let secrets_file = dir.path().join("safehouse.env");
        std::fs::write(&key_file, "tskey-ephemeral-tagged\n").unwrap();
        std::fs::write(
            &secrets_file,
            "SAFEHOUSE_MATRIX_USER_ID=@w1:example\nSAFEHOUSE_MATRIX_PASSWORD=pw\n\
             SAFEHOUSE_STORE_PASSPHRASE=sp\nSAFEHOUSE_RECOVERY_PASSPHRASE=rp\n",
        )
        .unwrap();

        let mut config = safehouse_config();
        config.safehouse_tailnet_auth_key_file = Some(key_file);
        config.safehouse_secrets_file = Some(secrets_file);

        let secrets = preflight(&config).unwrap();
        assert_eq!(secrets.safehouse_tailnet_auth_key.as_deref(), Some("tskey-ephemeral-tagged"));
        assert!(secrets
            .safehouse_secrets
            .as_ref()
            .unwrap()
            .contains("SAFEHOUSE_MATRIX_USER_ID"));
    }

    #[test]
    fn preflight_rejects_invalid_persona_name() {
        let mut config = safehouse_config();
        config.safehouse_personas = vec!["Not-Valid!".to_string()];
        let err = preflight(&config).unwrap_err().to_string();
        assert!(err.contains("safehouse-persona"), "err: {err}");
    }

    #[test]
    fn preflight_rejects_unsafe_homeserver_url_and_room() {
        let mut config = safehouse_config();
        config.safehouse_homeserver_url = Some("https://evil; rm -rf /".to_string());
        assert!(preflight(&config).is_err());

        let mut config2 = safehouse_config();
        config2.safehouse_room = Some("!room`whoami`:example".to_string());
        assert!(preflight(&config2).is_err());
    }

    #[test]
    fn preflight_safehouse_disabled_ignores_missing_safehouse_inputs() {
        // AC: without --safehouse, nothing about safehouse gates preflight.
        let config = base_config();
        assert!(preflight(&config).is_ok());
    }

    #[test]
    fn idle_shutdown_step_present_only_when_configured() {
        // Absent → skip.
        let plan = build_plan(&base_config(), &Secrets::default());
        let entry = plan
            .entries
            .iter()
            .find(|e| e.name() == "idle-shutdown")
            .unwrap();
        assert!(matches!(entry, super::super::PlanEntry::Skip { .. }));

        // Present → executable step whose apply renders the minute limit.
        let mut config = base_config();
        config.idle_shutdown_minutes = Some(45);
        let plan = build_plan(&config, &Secrets::default());
        let step = plan
            .entries
            .iter()
            .find_map(|e| match e {
                super::super::PlanEntry::Step(s) if s.name == "idle-shutdown" => Some(s),
                _ => None,
            })
            .unwrap();
        assert!(step.apply.contains("LIMIT=45"));
    }

    // ---- build_worker_record / #4697 registry fields ----------------------

    #[test]
    fn worker_record_carries_configured_idle_shutdown_window() {
        // #4697 AC 3's prerequisite: `fleet status` can only tell an EXPECTED
        // power-off from an outage if the window the guard was installed with
        // is persisted on the record at bootstrap time.
        let mut config = base_config();
        config.idle_shutdown_minutes = Some(45);
        let now = chrono::Utc::now();

        let record = build_worker_record(&config, Some(true), now);
        assert_eq!(record.idle_shutdown_minutes, Some(45));
        // A successful bootstrap observed the host up over SSH moments ago, so
        // the heuristic has a reference point from the very first poll.
        assert_eq!(record.last_seen_up_at.as_deref(), Some(now.to_rfc3339()).as_deref());
    }

    #[test]
    fn worker_record_leaves_idle_shutdown_absent_when_not_configured() {
        // Mirrors `render_idle_shutdown()`'s gate: no --idle-shutdown-minutes
        // => no guard installed => nothing for the heuristic to compare
        // against, so the host stays UNREACHABLE (never "expected") when
        // silent.
        let config = base_config();
        assert!(config.idle_shutdown_minutes.is_none());
        let record = build_worker_record(&config, Some(true), chrono::Utc::now());
        assert_eq!(record.idle_shutdown_minutes, None);
    }

    #[test]
    fn worker_record_roundtrips_idle_shutdown_fields_through_the_registry() {
        // The registry file is the only carrier between `add-worker` (write)
        // and a LATER `fleet status` process (read), so the #4697 fields must
        // survive a real save/load — and a record written before they existed
        // must still load (the `#[serde(default)]` backward-compat pattern).
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fleet.json");

        let mut config = base_config();
        config.idle_shutdown_minutes = Some(30);
        let record = build_worker_record(&config, Some(true), chrono::Utc::now());
        let expected_last_seen = record.last_seen_up_at.clone();

        let mut registry = FleetRegistry::default();
        registry.upsert(record);
        registry.save(&path).unwrap();

        let loaded = FleetRegistry::load(&path).unwrap();
        let w = loaded.get(&config.ssh_host).unwrap();
        assert_eq!(w.idle_shutdown_minutes, Some(30));
        assert_eq!(w.last_seen_up_at, expected_last_seen);

        // Pre-#4697 record: both fields absent from the JSON entirely.
        std::fs::write(
            &path,
            r#"{ "version": 1, "workers": [ { "ssh_host": "legacy", "bootstrapped_at": "t" } ] }"#,
        )
        .unwrap();
        let legacy = FleetRegistry::load(&path).unwrap();
        let w = legacy.get("legacy").unwrap();
        assert_eq!(w.idle_shutdown_minutes, None);
        assert_eq!(w.last_seen_up_at, None);
    }

    #[test]
    fn daemon_unit_pins_workingdirectory_with_4292_marker() {
        // AC 4: the #4292 token-pool cwd workaround must be marked with its
        // tracking issue so it is removed when #4292 lands.
        let config = base_config();
        let plan = build_plan(&config, &Secrets::default());
        let step = plan
            .entries
            .iter()
            .find_map(|e| match e {
                super::super::PlanEntry::Step(s) if s.name == "daemon-unit" => Some(s),
                _ => None,
            })
            .unwrap();
        assert!(step
            .apply
            .contains("WorkingDirectory=%h/loom-workspaces/anvil"));
        assert!(step.apply.contains("#4292"), "workaround must be marked with #4292");
        assert!(step.apply.contains("Restart=on-success"));
        assert!(step.apply.contains("enable-linger"));
    }

    #[test]
    fn daemon_unit_sets_supervisor_env_and_correct_restart_policy() {
        // #4640: without LOOM_DAEMON_SUPERVISOR=systemd, detect_supervisor()
        // (ipc.rs) can't tell the fleet daemon is systemd-supervised, so
        // `restart --drain` refuses on every fleet worker. Restart=on-failure
        // additionally inverts the exit-code contract: it never relaunches on
        // the restart primitive's clean exit 0, and (had it been changed to
        // `always` instead) would incorrectly relaunch on EXIT_SHUTDOWN (143).
        // Restart=on-success mirrors the canonical `render_systemd_unit()` in
        // loom-daemon-start.sh (#4268) and gets both right.
        let config = base_config();
        let plan = build_plan(&config, &Secrets::default());
        let step = plan
            .entries
            .iter()
            .find_map(|e| match e {
                super::super::PlanEntry::Step(s) if s.name == "daemon-unit" => Some(s),
                _ => None,
            })
            .unwrap();
        assert!(
            step.apply
                .contains("Environment=LOOM_DAEMON_SUPERVISOR=systemd"),
            "rendered unit must set LOOM_DAEMON_SUPERVISOR=systemd so detect_supervisor() \
             recognizes the fleet daemon as supervised"
        );
        assert!(
            step.apply.contains("Restart=on-success"),
            "rendered unit must use Restart=on-success (the EXIT_RESTART/EXIT_SIGINT/\
             EXIT_SHUTDOWN contract), not Restart=on-failure or Restart=always"
        );
        assert!(
            !step.apply.contains("Restart=on-failure"),
            "the old Restart=on-failure policy must be fully replaced"
        );
        assert!(
            step.summary.contains("Restart=on-success"),
            "step summary must match the rendered policy"
        );
    }

    /// #4831: `daemon-unit`'s `Environment=PATH=` used to be a THIRD,
    /// narrower hand-hardcoded set
    /// (`%h/.local/bin:/usr/local/bin:/usr/bin:/bin`, missing
    /// `%h/.cargo/bin` and Homebrew) that disagreed with both
    /// `resolve_plist_path()` (loom-daemon-start.sh) and this file's own
    /// provisioning `export PATH=` lines. It must now render the FULL
    /// shared canonical superset (`path_bootstrap::canonical_path_systemd`),
    /// byte-for-byte, not a hand-picked subset.
    #[test]
    fn daemon_unit_path_is_the_full_canonical_systemd_superset() {
        let config = base_config();
        let plan = build_plan(&config, &Secrets::default());
        let step = plan
            .entries
            .iter()
            .find_map(|e| match e {
                super::super::PlanEntry::Step(s) if s.name == "daemon-unit" => Some(s),
                _ => None,
            })
            .unwrap();
        let want =
            format!("Environment=PATH={}", super::super::path_bootstrap::canonical_path_systemd());
        assert!(
            step.apply.contains(&want),
            "daemon-unit must render the full canonical systemd PATH ({want}), got: {}",
            step.apply
        );
        assert!(
            step.apply.contains("%h/.cargo/bin"),
            "the pre-#4831 narrower systemd PATH omitted %h/.cargo/bin -- must be present now"
        );
        assert!(
            step.apply.contains("/opt/homebrew/bin"),
            "the pre-#4831 narrower systemd PATH omitted Homebrew -- must be present now"
        );
    }

    /// #4831: every one of the ~12 duplicated `export PATH="$HOME/.local/bin:
    /// $PATH"` provisioning lines omitted `${HOME}/.cargo/bin` and
    /// `/opt/homebrew/bin`. Spot-check a representative sample of rendered
    /// steps (spanning machine layout, forge auth, token bootstrap, and
    /// workspace registration) to prove they all now render the FULL
    /// canonical export line, not just one fixed-up call site.
    #[test]
    fn provisioning_steps_export_the_full_canonical_path_not_a_narrower_subset() {
        let config = base_config();
        // forge-auth/token-pool/token-ranking only render as Steps (not
        // Skips) when their secrets are present -- mirrors
        // forge_auth_and_token_accounts_carry_secret_stdin above.
        let secrets = Secrets {
            pat: Some("the-pat".to_string()),
            accounts_env: Some("ACCOUNT_EMAIL_1=a@b.c".to_string()),
            ..Secrets::default()
        };
        let plan = build_plan(&config, &secrets);
        let export_line = super::super::path_bootstrap::canonical_path_export_line();
        for name in [
            "machine-layout",
            "forge-auth",
            "token-pool",
            "token-ranking",
            "workspace-clone",
            "workspace-register",
        ] {
            let step = plan
                .entries
                .iter()
                .find_map(|e| match e {
                    super::super::PlanEntry::Step(s) if s.name == name => Some(s),
                    _ => None,
                })
                .unwrap_or_else(|| panic!("expected a step named {name}"));
            assert!(
                step.apply.contains(export_line.trim_end()),
                "step {name} must export the full canonical PATH ({}), got: {}",
                export_line.trim_end(),
                step.apply
            );
            assert!(
                step.apply.contains("${HOME}/.cargo/bin"),
                "step {name} is missing ${{HOME}}/.cargo/bin -- the pre-#4831 duplicated \
                 export lines omitted this"
            );
            assert!(
                step.apply.contains("/opt/homebrew/bin"),
                "step {name} is missing /opt/homebrew/bin -- the pre-#4831 duplicated export \
                 lines omitted this"
            );
        }
    }

    #[test]
    fn no_python_loom_tools_or_pip_step_anywhere() {
        // #4228 landed — no interim Python install may appear (AC 4).
        let config = base_config();
        let secrets = Secrets {
            pat: Some("pat".to_string()),
            accounts_env: Some("ACCOUNT_EMAIL_1=a@b.c".to_string()),
            ..Secrets::default()
        };
        let plan = build_plan(&config, &secrets);
        for entry in &plan.entries {
            if let super::super::PlanEntry::Step(s) = entry {
                let hay = format!("{}\n{}", s.apply, s.check.clone().unwrap_or_default());
                assert!(!hay.contains("break-system-packages"), "step {} has pip step", s.name);
                assert!(
                    !hay.contains("loom_tools"),
                    "step {} references python loom_tools",
                    s.name
                );
                assert!(!hay.contains("pip install"), "step {} has pip install", s.name);
            }
        }
    }

    #[test]
    fn workspace_register_uses_the_priority() {
        let mut config = base_config();
        config.priority = 7;
        config.repos = vec!["a/anvil".to_string(), "b/repo2".to_string()];
        let plan = build_plan(&config, &Secrets::default());
        let step = plan
            .entries
            .iter()
            .find_map(|e| match e {
                super::super::PlanEntry::Step(s) if s.name == "workspace-register" => Some(s),
                _ => None,
            })
            .unwrap();
        assert!(step.apply.contains("--priority 7"));
        assert!(step.apply.contains("loom-workspaces/anvil"));
        assert!(step.apply.contains("loom-workspaces/repo2"));
    }

    #[test]
    fn dry_run_render_is_stable_and_lists_all_steps() {
        let config = base_config();
        let secrets = Secrets {
            pat: Some("pat".to_string()),
            accounts_env: Some("env".to_string()),
            ..Secrets::default()
        };
        let plan = build_plan(&config, &secrets);
        let out = plan.render_dry_run("fleet add-worker", &config.ssh_host);
        assert!(out.contains("13 steps"));
        assert!(out.contains("base-deps"));
        assert!(out.contains("verify"));
        assert!(out.contains("feeds a secret via stdin"));
    }
}
