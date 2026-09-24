//! Opt-in account-private clone lifecycle and supervised dispatch. Account
//! coordination stays outside repositories; bounded recovery metadata stays
//! with the logical host repository. Runtime capability admission is unchanged.
mod adapter;
mod control;
pub mod dispatch;
mod docker;
pub mod export;
mod lease;
mod lifecycle;
mod repository;
#[cfg(test)]
mod tests;
pub mod transport;
mod worker_setup;

pub use lifecycle::{configured, run_job, start, status, stop};
pub use repository::WorkerCommand;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub const ROOT: &str = "/workspace";
pub const REPO: &str = "/workspace/repo";
pub const MODE: &str = "private-clone";
pub const PROTOCOL: &str = "loom-private-workspace-v1";
const PROFILE: &str = "/home/loom/.codex-profile";
const GH_CONFIG: &str = "/run/loom-gh";
// Keep preparation and supervised jobs on the same external auth context.
const FORGE_ENV: [&str; 6] = [
    "GH_TOKEN",
    "GITHUB_TOKEN",
    "GH_HOST",
    "GITEA_TOKEN",
    "FORGE_TOKEN",
    "GITEA_USERNAME",
];

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Config {
    pub schema_version: u32,
    pub account: String,
    pub container: String,
    pub engine: String,
    #[serde(default)]
    pub docker_desktop: bool,
    pub repository: String,
    pub base: String,
    pub volume: String,
    pub profile: PathBuf,
    pub gh_config: Option<PathBuf>,
}

#[derive(Debug, Serialize)]
pub struct Status {
    pub workspace_mode: &'static str,
    pub private_root: &'static str,
    pub config: Config,
    pub container_id: Option<String>,
    pub running: bool,
    pub lease: Option<lease::Job>,
}

#[derive(Clone, Debug, clap::Args)]
pub struct JobArgs {
    pub name: String,
    /// One exclusion domain for standalone roles, full sweeps and manual jobs.
    #[arg(long, value_enum)]
    pub kind: JobKind,
    #[arg(long)]
    pub owner: String,
    #[arg(long)]
    pub issue: Option<u64>,
    /// Existing issue helpers may create a worktree inside the private clone.
    #[arg(long)]
    pub branch: Option<String>,
    #[arg(last = true, required = true)]
    pub command: Vec<String>,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub enum JobKind {
    Role,
    Sweep,
    Interactive,
}

fn account(workspace: &Path, name: &str) -> Result<super::account_registry::AccountDescriptor> {
    use super::account_registry::{account_inventory, account_matches_reference, AccountProvider};
    account_inventory(workspace, AccountProvider::Codex)?
        .into_iter()
        .find(|a| account_matches_reference(a, name))
        .context("Codex account not found")
}

fn state_dir(profile: &Path) -> Result<PathBuf> {
    let profile = profile.canonicalize().context("resolve account profile")?;
    // Never place even secret-free coordination state inside an agent checkout.
    for parent in profile.ancestors() {
        if parent.join(".git").exists() {
            bail!("private session profiles must be outside every repository/worktree");
        }
    }
    let parent = profile.parent().context("account profile has no parent")?;
    Ok(parent
        .join(".private-sessions")
        .join(profile.file_name().unwrap()))
}

fn load(dir: &Path) -> Result<Config> {
    serde_json::from_slice(&std::fs::read(dir.join("workspace.json"))?)
        .context("invalid private workspace identity; preserve state for operator recovery")
}

fn save<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    use std::io::Write;
    let mut file = tempfile::NamedTempFile::new_in(path.parent().context("state parent")?)?;
    file.write_all(&serde_json::to_vec_pretty(value)?)?;
    file.as_file().sync_all()?;
    file.persist(path).map_err(|error| error.error)?;
    Ok(())
}

fn repository_url(value: &str) -> Result<String> {
    let url =
        reqwest::Url::parse(value).context("repository must be a credential-free HTTPS URL")?;
    if url.scheme() != "https"
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || url.host_str().is_none()
        || url.path().trim_matches('/').is_empty()
    {
        bail!("private clones require a credential-free HTTPS forge URL; local paths, SSH, URL credentials and URL parameters are unsupported");
    }
    Ok(url.to_string())
}

fn valid_base(base: &str) -> Result<()> {
    if base.is_empty()
        || base.starts_with('-')
        || base.contains("..")
        || base
            .chars()
            .any(|c| !(c.is_ascii_alphanumeric() || "/._-".contains(c)))
    {
        bail!("base must be a remote branch name or commit revision");
    }
    Ok(())
}
