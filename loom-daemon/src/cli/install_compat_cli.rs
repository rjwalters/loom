//! `loom-daemon install-compat` (#10716): the compatibility contract between
//! an installed Loom and the daemon, from the command line.
//!
//! * `show` prints this binary's side of the contract and, with `--repo`,
//!   the repo's recorded side (read from its default branch) and how they
//!   classify. Read-only.
//! * `check` is the CI proof across adjacent releases
//!   ([`loom_daemon::install_compat::harness`]). Exit `0` when every claim
//!   holds, `1` when one is violated, `2` when the check could not run.
//!
//! Rust rather than a script under `scripts/` because new executable logic
//! goes into the daemon (`.loom/docs/shell-language-policy.md`).

use anyhow::Result;
use loom_daemon::install_compat::{
    self, classify, harness, DaemonCompat, Version, DAEMON_INVOKED_INSTALLED_FILES,
    REQUIRES_DAEMON, SUPPORTS_INSTALLED,
};
use loom_daemon::release_resolve::host;
use std::path::PathBuf;

#[derive(clap::Subcommand)]
pub(crate) enum InstallCompatCommand {
    /// Print this daemon's side of the contract; with `--repo`, also the
    /// repo's recorded side and the classification.
    Show(ShowArgs),
    /// Prove the contract across adjacent releases (CI).
    Check(CheckArgs),
}

#[derive(clap::Args)]
pub(crate) struct ShowArgs {
    /// A repo clone whose default-branch `.loom/install-metadata.json` to
    /// classify against this daemon.
    #[arg(long, value_name = "PATH")]
    repo: Option<PathBuf>,
    /// Print one JSON object instead of text.
    #[arg(long)]
    json: bool,
}

#[derive(clap::Args)]
pub(crate) struct CheckArgs {
    /// The checkout under test. Defaults to the current directory.
    #[arg(long, value_name = "PATH", default_value = ".")]
    repo_root: PathBuf,
    /// A daemon binary for direction B: the oldest release at or above
    /// `requires_daemon` (releases skip versions, so not always that exact one).
    #[arg(long, value_name = "BIN", conflicts_with = "fetch_old_daemon")]
    old_daemon: Option<PathBuf>,
    /// Download direction B's daemon instead: the oldest published release at
    /// or above `requires_daemon` that carries `--asset`. When none is
    /// published yet, the new daemon stands in for it. Uses `gh`.
    #[arg(long)]
    fetch_old_daemon: bool,
    /// `owner/repo` to fetch from. Defaults to `$GITHUB_REPOSITORY`, then the
    /// repo root's `origin`.
    #[arg(long, value_name = "OWNER/REPO", requires = "fetch_old_daemon")]
    release_repo: Option<String>,
    /// The release asset to fetch. Defaults to `loom-daemon-<this host's
    /// target triple>`.
    #[arg(long, value_name = "NAME", requires = "fetch_old_daemon")]
    asset: Option<String>,
    /// The previous release's tag. Defaults to the newest `v*` tag at or
    /// below `VERSION`.
    #[arg(long, value_name = "TAG")]
    prev_ref: Option<String>,
    /// Check a proposed `requires_daemon` instead of the compiled one.
    #[arg(long, value_name = "VERSION")]
    requires_daemon: Option<String>,
    /// Check a proposed `supports_installed` instead of the compiled one.
    #[arg(long, value_name = "VERSION")]
    supports_installed: Option<String>,
}

impl InstallCompatCommand {
    pub(crate) fn run(self) -> Result<()> {
        match self {
            Self::Show(args) => args.run(),
            Self::Check(args) => args.run(),
        }
    }
}

impl ShowArgs {
    fn run(self) -> Result<()> {
        let daemon = DaemonCompat::this_binary(None).ok_or_else(|| {
            anyhow::anyhow!("this binary's version or SUPPORTS_INSTALLED is not MAJOR.MINOR.PATCH")
        })?;
        let installed = match &self.repo {
            Some(repo) => install_compat::read_default_branch(repo)?,
            None => None,
        };
        let classification = installed
            .as_ref()
            .map(|m| format!("{:?}", classify(m, &daemon)));
        if self.json {
            let obj = serde_json::json!({
                "running": daemon.running.to_string(),
                "supports_installed": SUPPORTS_INSTALLED,
                "requires_daemon": REQUIRES_DAEMON,
                "invoked_files": DAEMON_INVOKED_INSTALLED_FILES,
                "installed": installed.as_ref().map(|m| serde_json::json!({
                    "loom_version": m.loom_version,
                    "requires_daemon": m.requires_daemon,
                })),
                "classification": classification,
            });
            println!("{obj}");
            return Ok(());
        }
        println!("running            {}", daemon.running);
        println!("supports_installed {SUPPORTS_INSTALLED}");
        println!("requires_daemon    {REQUIRES_DAEMON} (written into install-metadata.json)");
        if let Some(repo) = &self.repo {
            match &installed {
                Some(m) => {
                    println!(
                        "installed          loom_version={} requires_daemon={}",
                        m.loom_version.as_deref().unwrap_or("(absent)"),
                        m.requires_daemon.as_deref().unwrap_or("(absent)")
                    );
                    println!("classification     {}", classification.unwrap_or_default());
                }
                None => println!(
                    "installed          (no install-metadata.json on {}'s default branch)",
                    repo.display()
                ),
            }
        }
        Ok(())
    }
}

impl CheckArgs {
    fn run(self) -> Result<()> {
        let claim = |flag: Option<String>, compiled: &str, name: &str| -> Result<Version> {
            let text = flag.unwrap_or_else(|| compiled.to_string());
            Version::parse(&text)
                .ok_or_else(|| anyhow::anyhow!("{name} {text:?} is not MAJOR.MINOR.PATCH"))
        };
        let old_daemon = if let Some(bin) = self.old_daemon {
            harness::OldDaemon::Given(bin)
        } else if self.fetch_old_daemon {
            let repo = self
                .release_repo
                .or_else(|| {
                    std::env::var("GITHUB_REPOSITORY")
                        .ok()
                        .filter(|s| !s.is_empty())
                })
                .or_else(|| host::repo_slug(&self.repo_root))
                .ok_or_else(|| anyhow::anyhow!("--fetch-old-daemon: pass --release-repo"))?;
            let asset = match self.asset {
                Some(a) => a,
                None => format!(
                    "loom-daemon-{}",
                    host::target_triple().ok_or_else(|| anyhow::anyhow!(
                        "--fetch-old-daemon: unrecognized host platform; pass --asset"
                    ))?
                ),
            };
            harness::OldDaemon::Fetch { repo, asset }
        } else {
            harness::OldDaemon::Absent
        };
        let opts = harness::CheckOptions {
            repo_root: self.repo_root,
            new_daemon: std::env::current_exe()?,
            old_daemon,
            prev_ref: self.prev_ref,
            supports_installed: claim(
                self.supports_installed,
                SUPPORTS_INSTALLED,
                "supports_installed",
            )?,
            requires_daemon: claim(self.requires_daemon, REQUIRES_DAEMON, "requires_daemon")?,
            invoked_files: DAEMON_INVOKED_INSTALLED_FILES
                .iter()
                .map(|s| (*s).to_string())
                .collect(),
        };
        let report = match harness::run(&opts) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("install-compat check: could not run: {e:#}");
                std::process::exit(2);
            }
        };
        for note in &report.notes {
            println!("  {note}");
        }
        if report.violations.is_empty() {
            println!("install-compat check: OK, every claim holds");
            return Ok(());
        }
        for v in &report.violations {
            println!("VIOLATION: {v}");
        }
        println!(
            "install-compat check: FAILED, {} violated claim(s). Fix the code, or move the claim \
             (defaults/docs/release-cadence.md, \"Compatibility contract\").",
            report.violations.len()
        );
        std::process::exit(1);
    }
}
