//! `loom-daemon codex-hooks verify` — the readiness check behind
//! `provision-codex-hooks.sh verify` (issue #9390). See
//! `loom_daemon::tokens_pool::codex_hooks` for what "ready" means.
//!
//! # Contract (inherited from the shell verb)
//!
//! - Exit **0** ready, **78** (`EX_CONFIG`) not ready, **1** usage error.
//!   `--all-profiles` exits with the WORST per-profile code, so one unready
//!   profile fails the whole pool, and 78 when the root is missing or empty.
//! - `--json`: one object per profile on stdout (JSONL under
//!   `--all-profiles`). Human lines go to stderr, prefixed
//!   `[provision-codex-hooks]` as they always were.
//! - Only profile directory NAMES are printed. No credential is read.
//!
//! `--fallback-bridge` is the stub's own `../hooks/guard-codex-bridge.sh`,
//! used as "this checkout's bridge" when no `--workspace` is named.
//!
//! `--allow-sealed` (issue #10102) lets a sealed registration stand in for
//! recorded hook trust. Only a caller that will then pass the trust waiver may
//! ask for it, and only `spawn-codex.sh` does, with the launch's `--cwd`, its
//! `--container`, and the Codex argv after `--`. The verdict's
//! `bypassHookTrust` says whether the waiver is now REQUIRED, and
//! `containerVerified` whether the container's copies were proven.

use std::path::PathBuf;

use anyhow::Result;

use loom_daemon::tokens_pool::codex_hooks::{pooled_profiles, seal, Check, Registration};

#[derive(clap::Subcommand)]
pub(crate) enum CodexHooksCommand {
    /// Report whether Loom's managed Codex hook is ready in a profile (or
    /// every pooled profile), without changing anything.
    Verify(VerifyArgs),
}

#[derive(clap::Args)]
pub(crate) struct VerifyArgs {
    /// The profile (CODEX_HOME) to check. Defaults to `$CODEX_HOME`.
    #[arg(long)]
    codex_home: Option<PathBuf>,
    /// Check every pooled profile under the profile root instead.
    #[arg(long)]
    all_profiles: bool,
    /// The profile root for `--all-profiles` (default: `LOOM_CODEX_PROFILE_ROOT`,
    /// else `~/.loom/codex-profiles`).
    #[arg(long)]
    profile_root: Option<PathBuf>,
    /// The workspace the hook will run in.
    #[arg(long)]
    workspace: Option<PathBuf>,
    /// Expect a PINNED registration naming this bridge (managed version 1).
    /// Without it the workspace-independent registration is expected.
    #[arg(long)]
    bridge: Option<PathBuf>,
    /// The bridge standing in for "this checkout's" when no `--workspace` is
    /// named (the provisioner's sibling `../hooks/guard-codex-bridge.sh`).
    #[arg(long)]
    fallback_bridge: Option<PathBuf>,
    /// `CODEX_HOME` as Codex will see it when it runs (decides which
    /// `hooks.state` key counts as trust). Default: the session container's
    /// mount point for a session-managed profile, else the canonical profile.
    #[arg(long)]
    runtime_codex_home: Option<PathBuf>,
    /// Print one JSON verdict per profile on stdout.
    #[arg(long)]
    json: bool,
    /// Let a sealed registration stand in for recorded hook trust (#10102).
    /// The caller MUST then pass `--dangerously-bypass-hook-trust` when the
    /// verdict says `bypassHookTrust`.
    #[arg(long)]
    allow_sealed: bool,
    /// The directory Codex will start in (project-layer hook sources are
    /// vetted from here). Defaults to `--workspace`.
    #[arg(long, requires = "allow_sealed")]
    cwd: Option<PathBuf>,
    /// The session container whose copies of the profile controls must be
    /// the vetted bytes.
    #[arg(long, requires = "allow_sealed")]
    container: Option<String>,
    /// Docker binary for `--container`.
    #[arg(long, default_value = "docker")]
    docker: String,
    /// The arguments the launch will hand Codex (vetted for hook sources).
    #[arg(last = true)]
    codex_args: Vec<String>,
}

impl CodexHooksCommand {
    pub(crate) fn run(self) -> Result<()> {
        match self {
            Self::Verify(args) => std::process::exit(args.run()),
        }
    }
}

fn info(text: &str) {
    eprintln!("[provision-codex-hooks] {text}");
}

fn error(text: &str) {
    eprintln!("[provision-codex-hooks] ERROR {text}");
}

impl VerifyArgs {
    fn registration(&self) -> Registration {
        match &self.bridge {
            Some(bridge) => Registration::Pinned {
                bridge: bridge.clone(),
            },
            None => Registration::WorkspaceIndependent,
        }
    }

    fn check(&self, codex_home: PathBuf) -> i32 {
        let verdict = Check {
            codex_home,
            workspace: self.workspace.clone(),
            registration: self.registration(),
            fallback_bridge: self.fallback_bridge.clone(),
            runtime_home: self.runtime_codex_home.clone(),
            sealed: self.allow_sealed.then(|| seal::Request {
                launch_dir: self.cwd.clone().or_else(|| self.workspace.clone()),
                codex_args: self.codex_args.clone(),
                container: self.container.clone().map(|name| seal::Container {
                    docker: self.docker.clone(),
                    name,
                }),
            }),
        }
        .verify();
        if self.json {
            println!("{}", serde_json::to_string(&verdict).unwrap_or_default());
        }
        if verdict.ready {
            info(&format!(
                "Codex profile '{}': hook parity READY ({}).",
                verdict.profile, verdict.reason
            ));
            0
        } else {
            error(&format!(
                "Codex profile '{}': hook parity NOT ready — {}.",
                verdict.profile, verdict.reason
            ));
            78
        }
    }

    fn run(self) -> i32 {
        if !self.all_profiles {
            let Some(home) = self.codex_home.clone().or_else(|| {
                std::env::var_os("CODEX_HOME")
                    .filter(|v| !v.is_empty())
                    .map(PathBuf::from)
            }) else {
                error("--codex-home (or --all-profiles, or a CODEX_HOME in the environment) is required.");
                return 1;
            };
            return self.check(home);
        }
        let Some(root) = self
            .profile_root
            .clone()
            .or_else(loom_daemon::tokens_pool::paths::codex_profile_root)
        else {
            error("No Codex profile root is configured. Pass --profile-root.");
            return 78;
        };
        if !root.is_dir() {
            error(&format!(
                "Codex profile root '{}' does not exist. Create profiles first (loom-daemon accounts add codex <name>), or pass --profile-root.",
                root.display()
            ));
            return 78;
        }
        let registration = self.registration();
        let profiles = pooled_profiles(&root, &registration);
        if profiles.is_empty() {
            error(&format!("No Codex profiles found under '{}'.", root.display()));
            return 78;
        }
        let worst = profiles
            .iter()
            .map(|profile| self.check(profile.clone()))
            .max()
            .unwrap_or(0);
        info(&format!(
            "verify applied to {} Codex profile(s) under the pool root (worst exit: {worst}).",
            profiles.len()
        ));
        worst
    }
}
