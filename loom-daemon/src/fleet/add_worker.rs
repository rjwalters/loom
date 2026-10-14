//! `fleet add-worker` — bootstrap a provisioned host into a working loom worker
//! (issue #4341, epic #4340). This module renders the ordered bootstrap plan
//! (the heredoc-style shell templates for each [`super::Step`]), the production
//! SSH [`super::CommandRunner`], and the top-level [`run`] orchestration.
//!
//! **Supported target platform: Linux only.** Every step is built on
//! `apt-get` and `systemd --user`, so [`ensure_supported_platform`] probes
//! `uname -s` on the host before the plan is built and refuses anything else
//! with the platform named + a pointer to the manual route (#5395) — rather
//! than failing opaquely partway through `base-deps` on a macOS box.
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
mod forge_auth;
use super::{
    all_succeeded, default_fleet_registry_path, execute_plan, render_checklist, CommandOutput,
    CommandRunner, FleetRegistry, Plan, Step, StepStatus, StepStdin, VerifyResult, WorkerRecord,
    CHECKLIST_NOTE_PREFIX,
};

/// Default upstream Loom repo cloned to the worker's machine-level layout.
pub const DEFAULT_LOOM_REPO_URL: &str = "https://github.com/rjwalters/loom";

/// The `uname -s` value of the only target platform this command's plan
/// supports (#5395). Every step is built on `apt-get` and `systemd --user`,
/// so a non-Linux target is refused up front rather than half-provisioned.
const SUPPORTED_TARGET_UNAME: &str = "Linux";

/// The probe run on the target host — before any plan step — to learn its
/// platform (#5395). Deliberately the cheapest possible remote command: it
/// needs no `sudo`, writes nothing, and exists on every POSIX host.
const PLATFORM_PROBE_SHELL: &str = "uname -s";

/// Where an operator is pointed when the target is not Linux: the reference
/// section that documents the supported platform and the manual-onboarding
/// route for everything else.
const PLATFORM_RUNBOOK_REF: &str =
    ".loom/docs/daemon-reference.md § \"fleet add-worker\" -> \"Supported platform\"";

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
    /// Cross-repo draw weight the workspaces are registered at (#11742;
    /// higher = more draws).
    pub weight: u32,
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
    /// Idle-shutdown guard: power the host off after this many idle minutes.
    /// `None` disables the guard.
    pub idle_shutdown_minutes: Option<u32>,
}

/// Secret payloads read locally during preflight, then fed to the plan over
/// stdin. Never logged, never placed on a command line.
#[derive(Debug, Clone, Default)]
pub struct Secrets {
    /// Fine-grained forge PAT contents, if `--pat-file` was supplied.
    pub pat: Option<String>,
    /// `accounts.env` contents, if `--accounts-env` was supplied.
    pub accounts_env: Option<String>,
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

    Ok(secrets)
}

/// Human-facing name for a raw `uname -s` value. Only the platforms an
/// operator is plausibly holding get a friendlier gloss; anything else is
/// reported verbatim (first line only, length-capped) so an unexpected value
/// still names *something* concrete instead of dumping arbitrary remote
/// output into the error.
fn platform_label(uname: &str) -> String {
    let raw: String = uname
        .lines()
        .next()
        .unwrap_or("")
        .trim()
        .chars()
        .take(40)
        .collect();
    match raw.as_str() {
        "Darwin" => "macOS (Darwin)".to_string(),
        "FreeBSD" | "OpenBSD" | "NetBSD" => format!("{raw} (BSD)"),
        other
            if other.starts_with("MINGW")
                || other.starts_with("MSYS")
                || other.starts_with("CYGWIN") =>
        {
            format!("Windows ({other})")
        }
        _ => raw,
    }
}

/// Fail fast when the target host is not Linux (#5395).
///
/// Every step in [`build_plan`] is `apt-get` + `systemd --user`; on a macOS or
/// BSD target the plan cannot succeed, and running it anyway would fail
/// partway through with an opaque per-step diagnostic after having already
/// touched the host. So the platform is probed with a single cheap
/// [`PLATFORM_PROBE_SHELL`] call **before the plan is built**, and a non-Linux
/// answer aborts the run with the platform named and a pointer to the manual
/// route.
///
/// Not called in `--dry-run` mode: that path is documented as printing the
/// plan *without contacting the host*, and this probe is host contact.
pub fn ensure_supported_platform(runner: &dyn CommandRunner, ssh_host: &str) -> Result<()> {
    let out = runner.run(PLATFORM_PROBE_SHELL, None).with_context(|| {
        format!("probing the platform of {ssh_host} with `{PLATFORM_PROBE_SHELL}`")
    })?;

    if !out.ok() {
        let stderr = out.stderr.trim();
        let detail = if stderr.is_empty() {
            String::new()
        } else {
            format!(": {}", stderr.lines().next_back().unwrap_or(stderr).trim())
        };
        bail!(
            "could not determine the platform of {ssh_host}: `{PLATFORM_PROBE_SHELL}` exited \
             {}{detail}\nfleet add-worker probes the target platform before running any step \
             (it supports {SUPPORTED_TARGET_UNAME} targets only). Check that the host is \
             reachable over `ssh {ssh_host}` with non-interactive auth.",
            out.code
        );
    }

    let uname = out.stdout.trim();
    if uname.is_empty() {
        bail!(
            "could not determine the platform of {ssh_host}: `{PLATFORM_PROBE_SHELL}` succeeded \
             but printed nothing. fleet add-worker supports {SUPPORTED_TARGET_UNAME} targets \
             only; see {PLATFORM_RUNBOOK_REF}."
        );
    }
    if uname.eq_ignore_ascii_case(SUPPORTED_TARGET_UNAME) {
        return Ok(());
    }

    bail!(
        "{ssh_host} is {}, but fleet add-worker supports {SUPPORTED_TARGET_UNAME} targets only.\n\
         Its plan is built entirely on `apt-get` and `systemd --user`, neither of which exists \
         there, so no step of it can succeed — refusing before anything on the host is touched.\n\
         Onboard this host by hand instead: re-run with `--dry-run` to print the ordered plan \
         (each step's intent, in order) as a manual checklist, and see {PLATFORM_RUNBOOK_REF} \
         for the supported-platform boundary and the per-step equivalents.",
        platform_label(uname)
    )
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

    // 1. Base deps (libsqlite3-dev for the from-source fallback). Deliberately
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

    // 4. Forge auth (PAT over stdin).
    forge_auth::push_forge_auth(&mut plan, secrets.pat.as_ref());

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
        &format!("loom-daemon workspace add each repo (weight {})", config.weight),
        Some(render_workspace_register_check(&config.repos)),
        render_workspace_register(&config.repos, config.weight),
    ));

    // 8. Start the daemon under a systemd --user unit (Restart=always, linger).
    plan.push_step(Step::new(
        "daemon-unit",
        "install + enable the loom-daemon systemd --user unit (linger, Restart=always + RestartPreventExitStatus, OOMPolicy=continue, LOOM_DAEMON_SUPERVISOR=systemd)",
        Some("systemctl --user is-enabled loom-daemon.service >/dev/null 2>&1".to_string()),
        render_daemon_unit(&primary_rel),
    ));

    // 8b. Watchdog timer (#5343): `daemon-unit` above hand-renders the systemd
    //     unit directly, never calling loom-daemon-start.sh, so its
    //     watchdog-provisioning branch never runs on THIS path -- the root
    //     cause of the "autonomy-desired marker present, watchdog job
    //     missing" gap discovered on loom-worker-2. See
    //     render_daemon_watchdog's doc comment for how this step closes it.
    plan.push_step(Step::new(
        "daemon-watchdog",
        "provision the loom-daemon-watchdog systemd --user timer (#5343)",
        Some("systemctl --user is-enabled loom-daemon-watchdog.timer >/dev/null 2>&1".to_string()),
        render_daemon_watchdog(&primary_rel),
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

    // 10. Verify: daemon reachable    // 11. Verify: daemon reachable from the workspace cwd, ranking fresh, repos registered.
    plan.push_step(Step::new(
        "verify",
        "verify: loom-daemon status sane from the workspace cwd, token ranking fresh, workspace registered",
        None,
        render_verify(&primary_rel, &config.repos),
    ));

    plan
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
        weight: config.weight,
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
/// Preflight → (dry-run: print the plan + return) → platform probe → build
/// plan → execute over ssh → print the per-step checklist → on full success,
/// upsert the fleet registry record. Returns an error if any step failed (so
/// the CLI exits non-zero).
///
/// The platform probe (#5395) sits between preflight and the plan: the plan is
/// Linux-only (`apt-get` + `systemd --user`), so a non-Linux target is refused
/// before a single `apt-get`/`systemd` step is rendered, let alone run.
pub fn run(config: &AddWorkerConfig) -> Result<()> {
    let secrets = preflight(config)?;

    if config.dry_run {
        let plan = build_plan(config, &secrets);
        print!("{}", plan.render_dry_run("fleet add-worker", &config.ssh_host));
        println!(
            "\n(dry run — no action taken on {}. Re-run without --dry-run to execute.)",
            config.ssh_host
        );
        println!(
            "(this plan supports {SUPPORTED_TARGET_UNAME} targets only; a real run probes \
             `{PLATFORM_PROBE_SHELL}` on the host first and refuses anything else — see \
             {PLATFORM_RUNBOOK_REF}.)"
        );
        return Ok(());
    }

    let runner = SshRunner::new(&config.ssh_host);
    ensure_supported_platform(&runner, &config.ssh_host)?;

    let plan = build_plan(config, &secrets);
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
# #11044: this is unattended, so the fleet floor confirmation must not stop it.
export LOOM_DAEMON_UPDATE_INVOKER=add-worker
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

fn render_workspace_register(repos: &[String], weight: u32) -> String {
    let export_line = path_bootstrap::canonical_path_export_line();
    let mut s = format!(
        r#"set -e
{export_line}"#
    );
    for repo in repos {
        let rel = workspace_rel(repo);
        s.push_str(&format!("loom-daemon workspace add \"$HOME/{rel}\" --weight {weight}\n"));
    }
    s
}

fn render_daemon_unit(primary_rel: &str) -> String {
    // WorkingDirectory pinned to a workspace clone is the #4292 token-pool cwd
    // workaround — REMOVE once #4292 lands (token pool no longer cwd-coupled).
    //
    // The supervision directives (Restart=always + RestartPreventExitStatus=,
    // RestartSec=, KillMode=mixed, TimeoutStopSec=20, OOMPolicy=continue,
    // SuccessExitStatus=) and the [Unit] start-rate limit come from the
    // canonical renderer (`daemon_start::render`, #11058) instead of a second
    // hand-kept copy -- this template drifted from it twice before (#5119 found
    // it without KillMode=mixed; it never got #6129's exit statuses).
    // `Environment=LOOM_DAEMON_SUPERVISOR=systemd` lets the daemon prove it is
    // supervised before `restart --drain` exits it (#4640).
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
    let start_limit = crate::daemon_start::render::SYSTEMD_START_LIMIT;
    let supervision = crate::daemon_start::render::systemd_supervision_block();
    format!(
        r#"set -e
{export_line}mkdir -p "$HOME/.config/systemd/user"
cat > "$HOME/.config/systemd/user/loom-daemon.service" <<'UNIT'
[Unit]
Description=Loom daemon (fleet worker)
After=network-online.target
Wants=network-online.target
{start_limit}
[Service]
Type=simple
# WORKAROUND(#4292): the token pool resolves via the daemon's cwd, so pin the
# working directory to a workspace clone whose .loom/tokens/ the daemon can
# find. Remove this WorkingDirectory line once #4292 (cwd-decoupled token pool)
# lands.
WorkingDirectory=%h/{primary_rel}
ExecStart=%h/.local/bin/loom-daemon
# The supervision block below is rendered by the canonical
# daemon_start::render::systemd_supervision_block() (#11058), so the fleet unit
# and the loom-daemon-start unit carry one exit-code contract, not two copies.
{supervision}Environment=PATH={systemd_path}
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

/// Provision the `loom-daemon-watchdog.timer` (#5343) by re-running the
/// already-cloned `loom-daemon-start.sh` from the workspace root
/// `render_daemon_unit` just (re)started the daemon under.
///
/// `daemon-unit` above hand-renders the systemd unit directly (its own
/// `cat > ... <<'UNIT'` heredoc + `systemctl --user enable --now`) instead of
/// calling `loom-daemon-start.sh` — so that script's own
/// `provision_watchdog_job_systemd` call, which normally arms the watchdog on
/// every fresh start, never runs on this path. That gap is exactly what left
/// `loom-worker-2` with the `autonomy-desired` marker present (written by the
/// daemon's own startup healing, `autonomy_marker.rs`, #4331, once it saw
/// `LOOM_DAEMON_SUPERVISOR=systemd`) but no watchdog job ever provisioned.
///
/// Rather than duplicating the watchdog unit/timer rendering a SECOND time in
/// Rust (drifting from the shell version is exactly the kind of two-copy bug
/// this codebase avoids elsewhere, e.g. `canonical-daemon-path.sh` / #4831),
/// this step re-runs `loom-daemon-start.sh` itself. By the time this step
/// executes, `daemon-unit` has already started the daemon, so
/// `loom-daemon-start.sh`'s "already running" guard fires — and its #5343
/// self-heal (`heal_watchdog_provisioning_gap`) provisions the missing
/// watchdog timer WITHOUT touching the running daemon, exactly mirroring what
/// an operator re-running the same script by hand would do. Idempotent on a
/// re-run: `provision_watchdog_job_systemd`'s own `enable --now` on an
/// already-active timer is a verified no-op (#4862), and this step's own
/// `check` (`systemctl --user is-enabled loom-daemon-watchdog.timer`) reports
/// `AlreadyDone` once the timer is provisioned.
fn render_daemon_watchdog(primary_rel: &str) -> String {
    let export_line = path_bootstrap::canonical_path_export_line();
    format!(
        r#"set -e
{export_line}cd "$HOME/{primary_rel}"
"$HOME/.local/share/loom/defaults/scripts/cli/loom-daemon-start.sh"
"#
    )
}

fn render_idle_shutdown(minutes: u32) -> String {
    // Stage 2 of power-off: autonomous.idleExit is stage 1. Before #5565 this
    // guard also vetoed on bare `pgrep -f loom-daemon` process presence — but
    // under the fleet's OWN `systemd --user … Restart=always` supervision
    // (`daemon-unit`'s step, same plan), the daemon is essentially always
    // running (a stage-1 self-exit gets immediately respawned), so that veto
    // could never observe an absent daemon and `--idle-shutdown-minutes` was
    // a no-op on every `fleet add-worker`-provisioned host. Instead, the
    // guard now ASKS the running daemon directly whether it is idle-exit
    // eligible (`loom-daemon status --json`'s `idle_exit.eligible` field —
    // the SAME determination `autonomous.idleExit`'s tracker computes) and
    // vetoes on THAT, not on the daemon's mere existence. A daemon too old to
    // report eligibility (`idle_exit` present but no `"eligible"` key, or
    // absent from the JSON) falls back to the raw in-flight-sweep count
    // (`in_flight_count`), preserving the pre-#5565 busy signal. A totally
    // unreachable daemon (socket gone — genuinely down, not idle)
    // contributes no veto of its own: there is nothing left to ask, and a
    // dead daemon cannot itself be "busy".
    //
    // This guard script runs unattended out of cron (#4831: cron's PATH is
    // typically minimal/empty), so it needs the full canonical PATH, not
    // just ~/.local/bin, to reliably find `loom-daemon`.
    let export_line = path_bootstrap::canonical_path_export_line();
    format!(
        r#"set -e
mkdir -p "$HOME/.local/bin"
cat > "$HOME/.local/bin/loom-idle-shutdown.sh" <<'GUARD'
#!/usr/bin/env bash
# loom-idle-shutdown (#4341/#4467/#5565), stage 2. autonomous.idleExit is
# stage 1 (the daemon's own tracker); this guard remains the sole power-off
# authority, and asks that SAME tracker's live verdict rather than vetoing on
# bare `loom-daemon` process presence (#5565).
set -euo pipefail
{export_line}LIMIT={minutes}
STAMP="$HOME/.loom/last-active"
mkdir -p "$HOME/.loom"
active=0
if pgrep -x claude >/dev/null 2>&1; then active=1; fi
STATUS_JSON="$(loom-daemon status --json 2>/dev/null || true)"
if [ -n "$STATUS_JSON" ]; then
  if echo "$STATUS_JSON" | grep -Eq '"eligible"[[:space:]]*:[[:space:]]*true'; then
    : # daemon reachable and self-reports idle-exit eligible — do not veto
      # on its own process presence (#5565).
  elif echo "$STATUS_JSON" | grep -Eq '"eligible"[[:space:]]*:[[:space:]]*false'; then
    active=1 # daemon reachable but reports NOT eligible (busy, or its own
             # idle window has not elapsed yet).
  elif echo "$STATUS_JSON" | grep -Eq '"in_flight_count"[[:space:]]*:[[:space:]]*[1-9]'; then
    active=1 # daemon reachable but too old to report eligibility — fall
             # back to the raw in-flight-sweep count.
  fi
fi
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
# `loom-daemon status` exits 5 (EXIT_AUTONOMY_MISMATCH, #5409) when the daemon
# is up and healthy but its work finder is off while the autonomy-desired
# marker is present. That is a reachable daemon with an autonomy
# configuration state, not "not ready" (#9064): accept 0 and 5, retry
# anything else within the bound. The exit code is captured via `|| rc=$?`
# so `set -e` does not abort the script on a nonzero status.
STATUS_OK=0
STATUS_RC=0
for _attempt in $(seq 1 15); do
  STATUS_RC=0
  loom-daemon status >/dev/null 2>&1 || STATUS_RC=$?
  if [ "$STATUS_RC" = "0" ]; then
    STATUS_OK=1
    break
  fi
  if [ "$STATUS_RC" = "5" ]; then
    STATUS_OK=1
    echo "warning: loom-daemon status exited 5 (autonomy mismatch: work finder is off on this worker); daemon is reachable, so verification continues. Autonomy settings are left unchanged -- use explicit dispatch or enable the work finder deliberately." >&2
    break
  fi
  sleep 2
done
if [ "$STATUS_OK" != "1" ]; then
  echo "loom-daemon status did not become ready within ~30s (last status exit code: $STATUS_RC)" >&2
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

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests;

// #11044: the update-script invoker marker, in its own sibling file rather
// than `tests.rs`, which is over the file-size ratchet's threshold (see
// `.loom/docs/file-size-policy.md`).
#[cfg(test)]
mod invoker_tests;

// #9064: verify-script `status` exit-code handling, same reason.
#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod verify_status_tests;
