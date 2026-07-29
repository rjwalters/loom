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

use super::{
    all_succeeded, default_fleet_registry_path, execute_plan, render_checklist, CommandOutput,
    CommandRunner, FleetRegistry, Plan, Step, StepStatus, StepStdin, VerifyResult, WorkerRecord,
};

/// Default upstream Loom repo cloned to the worker's machine-level layout.
pub const DEFAULT_LOOM_REPO_URL: &str = "https://github.com/rjwalters/loom";

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

/// Build the ordered bootstrap [`Plan`] from `config` + already-read `secrets`.
///
/// Pure (no I/O, no host contact) so the ordering, idempotency checks, secret
/// placement, and skip-with-notice branches are all unit-testable. Each step's
/// `check` phase is what makes a re-run idempotent (AC 2).
#[must_use]
pub fn build_plan(config: &AddWorkerConfig, secrets: &Secrets) -> Plan {
    let mut plan = Plan::new();
    let primary_rel = workspace_rel(&config.repos[0]);

    // 1. Base deps (safehouse#38: libsqlite3-dev is required).
    plan.push_step(Step::new(
        "base-deps",
        "install build-essential, pkg-config, libssl-dev, libsqlite3-dev, git, gh, rustup",
        Some(
            "dpkg -s build-essential pkg-config libssl-dev libsqlite3-dev git >/dev/null 2>&1 \
             && command -v gh >/dev/null 2>&1 && command -v cargo >/dev/null 2>&1"
                .to_string(),
        ),
        render_base_deps(),
    ));

    // 2. Machine-level layout: clone loom, build loom-daemon, install to ~/.local/bin.
    plan.push_step(Step::new(
        "machine-layout",
        "clone loom to ~/.local/share/loom, cargo build -p loom-daemon --release, install to ~/.local/bin",
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
        "clone workspace repo(s) and run loom-daemon init (installs /loom:sweep)",
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

    // 8. Start the daemon under a systemd --user unit (Restart=on-failure, linger).
    plan.push_step(Step::new(
        "daemon-unit",
        "install + enable the loom-daemon systemd --user unit (linger, Restart=on-failure)",
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

    // 10. Optional safehouse wiring (#3998 not yet landed → skip-with-notice).
    if config.safehouse_enabled {
        plan.push_skip(
            "safehouse",
            "wire safehouse fleet-comms (tailnet join, account, LOOM_SAFEHOUSE_* env)",
            "requires #3998 (safehoused provisioning fragment) which has not landed",
        );
    } else {
        plan.push_skip(
            "safehouse",
            "wire safehouse fleet-comms",
            "safehouse not requested (pass --safehouse to enable once #3998 lands)",
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

/// Top-level orchestration for `loom-daemon fleet add-worker`.
///
/// Preflight → build plan → (dry-run: print + return) → execute over ssh →
/// print the per-step checklist → on full success, upsert the fleet registry
/// record. Returns an error if any step failed (so the CLI exits non-zero).
pub fn run(config: &AddWorkerConfig) -> Result<()> {
    let secrets = preflight(config)?;
    let plan = build_plan(config, &secrets);

    if config.dry_run {
        print!("{}", plan.render_dry_run(&config.ssh_host));
        println!(
            "\n(dry run — no action taken on {}. Re-run without --dry-run to execute.)",
            config.ssh_host
        );
        return Ok(());
    }

    let runner = SshRunner::new(&config.ssh_host);
    let reports = execute_plan(&runner, &plan);
    print!("{}", render_checklist(&config.ssh_host, &reports));

    let verify_ok = reports
        .iter()
        .find(|r| r.name == "verify")
        .map(|r| matches!(r.status, StepStatus::Changed));

    if all_succeeded(&reports, &plan) {
        let record = WorkerRecord {
            ssh_host: config.ssh_host.clone(),
            repos: config.repos.clone(),
            priority: config.priority,
            bootstrapped_at: chrono::Utc::now().to_rfc3339(),
            last_verify: verify_ok.map(|ok| VerifyResult {
                ok,
                at: chrono::Utc::now().to_rfc3339(),
                summary: if ok {
                    "daemon reachable, ranking fresh, workspace registered".to_string()
                } else {
                    "verify step did not confirm".to_string()
                },
            }),
        };
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
    // NOTE: the rustup + gh installs are written as shell text here (never run
    // through the daemon's own shell), so the curl-pipe idiom is safe.
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
if ! command -v cargo >/dev/null 2>&1; then
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
fi
"#
    .to_string()
}

fn render_machine_layout(loom_repo_url: &str) -> String {
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
cd "$LOOM_SRC"
cargo build -p loom-daemon --release
mkdir -p "$HOME/.local/bin"
install -m 0755 "$LOOM_SRC/target/release/loom-daemon" "$HOME/.local/bin/loom-daemon"
# Ensure ~/.local/bin is on PATH for future logins (Linux worker skips codesign).
if ! echo "$PATH" | tr ':' '\n' | grep -qx "$HOME/.local/bin"; then
  echo 'export PATH="$HOME/.local/bin:$PATH"' >> "$HOME/.profile"
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
    r#"set -e
export PATH="$HOME/.local/bin:$PATH"
gh auth login --with-token
gh auth setup-git
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
    r#"set -e
export PATH="$HOME/.local/bin:$PATH"
LOOM_ACCOUNTS_ENV="$HOME/.loom/accounts.env" loom-daemon tokens bootstrap --shared --home-env "$HOME/.loom/accounts.env"
"#
    .to_string()
}

fn render_token_ranking() -> String {
    r#"set -e
export PATH="$HOME/.local/bin:$PATH"
loom-daemon tokens check --ranking --shared 2>/dev/null || loom-daemon tokens check --ranking
"#
    .to_string()
}

fn render_workspace_clone_check(repos: &[String]) -> String {
    let mut s = String::from("set -e\n");
    for repo in repos {
        let rel = workspace_rel(repo);
        s.push_str(&format!("test -d \"$HOME/{rel}/.git\" || exit 1\n"));
    }
    s
}

fn render_workspace_clone(repos: &[String]) -> String {
    let mut s = String::from(
        r#"set -e
export PATH="$HOME/.local/bin:$PATH"
LOOM_SRC="$HOME/.local/share/loom"
mkdir -p "$HOME/loom-workspaces"
"#,
    );
    for repo in repos {
        let rel = workspace_rel(repo);
        s.push_str(&format!(
            r#"if [ ! -d "$HOME/{rel}/.git" ]; then
  gh repo clone {repo} "$HOME/{rel}"
fi
loom-daemon init "$HOME/{rel}" --defaults "$LOOM_SRC/defaults" || true
"#
        ));
    }
    s
}

fn render_workspace_register_check(repos: &[String]) -> String {
    let mut s = String::from(
        r#"set -e
export PATH="$HOME/.local/bin:$PATH"
LIST="$(loom-daemon workspace list --json 2>/dev/null || echo '{}')"
"#,
    );
    for repo in repos {
        let rel = workspace_rel(repo);
        let name = repo_dir_name(repo);
        s.push_str(&format!("echo \"$LIST\" | grep -q \"{name}\" || exit 1  # {rel}\n"));
    }
    s
}

fn render_workspace_register(repos: &[String], priority: u32) -> String {
    let mut s = String::from(
        r#"set -e
export PATH="$HOME/.local/bin:$PATH"
"#,
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
    format!(
        r#"set -e
export PATH="$HOME/.local/bin:$PATH"
mkdir -p "$HOME/.config/systemd/user"
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
Restart=on-failure
RestartSec=5
Environment=PATH=%h/.local/bin:/usr/local/bin:/usr/bin:/bin

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
    // Guard: power off after N idle minutes, but never while a sweep child
    // (claude) runs or the daemon reports in-flight sweeps. Pilot-equivalent.
    format!(
        r#"set -e
mkdir -p "$HOME/.local/bin"
cat > "$HOME/.local/bin/loom-idle-shutdown.sh" <<'GUARD'
#!/usr/bin/env bash
# loom-idle-shutdown (#4341): power off after {minutes} idle minutes, skipping
# while claude or loom-daemon are actively working.
set -euo pipefail
export PATH="$HOME/.local/bin:$PATH"
LIMIT={minutes}
STAMP="$HOME/.loom/last-active"
mkdir -p "$HOME/.loom"
active=0
if pgrep -x claude >/dev/null 2>&1; then active=1; fi
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

fn render_verify(primary_rel: &str, repos: &[String]) -> String {
    let mut checks = String::new();
    for repo in repos {
        let name = repo_dir_name(repo);
        checks.push_str(&format!(
            "echo \"$LIST\" | grep -q \"{name}\" || {{ echo \"workspace {name} not registered\" >&2; exit 1; }}\n"
        ));
    }
    format!(
        r#"set -e
export PATH="$HOME/.local/bin:$PATH"
cd "$HOME/{primary_rel}" 2>/dev/null || true
# Daemon reachable + status sane from the workspace cwd.
loom-daemon status >/dev/null 2>&1 || {{ echo "loom-daemon status failed" >&2; exit 1; }}
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
mod tests {
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

    #[test]
    fn forge_auth_and_token_accounts_carry_secret_stdin() {
        let config = base_config();
        let secrets = Secrets {
            pat: Some("the-pat".to_string()),
            accounts_env: Some("ACCOUNT_EMAIL_1=a@b.c".to_string()),
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
        let dry = plan.render_dry_run("worker-1");
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

    #[test]
    fn safehouse_is_skip_with_3998_notice_when_enabled() {
        let mut config = base_config();
        config.safehouse_enabled = true;
        let plan = build_plan(&config, &Secrets::default());
        let entry = plan
            .entries
            .iter()
            .find(|e| e.name() == "safehouse")
            .unwrap();
        match entry {
            super::super::PlanEntry::Skip { reason, .. } => assert!(reason.contains("#3998")),
            other => panic!("expected safehouse skip, got {other:?}"),
        }
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
        assert!(step.apply.contains("Restart=on-failure"));
        assert!(step.apply.contains("enable-linger"));
    }

    #[test]
    fn no_python_loom_tools_or_pip_step_anywhere() {
        // #4228 landed — no interim Python install may appear (AC 4).
        let config = base_config();
        let secrets = Secrets {
            pat: Some("pat".to_string()),
            accounts_env: Some("ACCOUNT_EMAIL_1=a@b.c".to_string()),
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
        };
        let plan = build_plan(&config, &secrets);
        let out = plan.render_dry_run(&config.ssh_host);
        assert!(out.contains("13 steps"));
        assert!(out.contains("base-deps"));
        assert!(out.contains("verify"));
        assert!(out.contains("feeds a secret via stdin"));
    }
}
