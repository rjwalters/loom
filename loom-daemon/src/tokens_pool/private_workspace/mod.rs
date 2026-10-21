//! Opt-in account-private clone lifecycle and supervised dispatch. Account
//! coordination stays outside repositories; bounded recovery metadata stays
//! with the logical host repository. Static runtime capability manifests are
//! unchanged; a verified clone whose `loom-private-control-v1` boundary is
//! intact can satisfy the repository-isolation requirement for ONE launch
//! through [`containment`] (#8787).
mod adapter;
pub mod bundle;
pub mod containment;
#[cfg(test)]
mod containment_tests;
mod control;
pub mod dispatch;
mod docker;
pub mod export;
mod lease;
mod lifecycle;
#[cfg(test)]
mod recover_tests;
mod repository;
#[cfg(test)]
mod tests;
pub mod transport;
mod worker_setup;

pub use lifecycle::{configured, recover, run_job, start, status, stop};
pub use repository::WorkerCommand;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub const ROOT: &str = "/workspace";
pub const REPO: &str = "/workspace/repo";
/// Parent of every additional repository clone an account holds beyond the
/// legacy primary at [`REPO`] (#10795). Lives on the account's own volume.
pub const WORK_ROOT: &str = "/workspace/work";
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
    /// The primary repository, cloned at [`REPO`]. Kept as-is so a volume,
    /// container label and credential scope created before multi-repository
    /// sessions (#10795) stay valid byte for byte.
    pub repository: String,
    pub base: String,
    /// Further repositories registered on this account, each cloned lazily at
    /// `WORK_ROOT/<slug>`. Absent in state written before #10795.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub repositories: Vec<Registered>,
    pub volume: String,
    pub profile: PathBuf,
    pub gh_config: Option<PathBuf>,
}

/// One additional repository of an account (registered, not necessarily cloned).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Registered {
    pub repository: String,
    pub base: String,
    pub slug: String,
}

/// The clone one operation acts on: where it lives and what it must be.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Target {
    pub repository: String,
    pub base: String,
    pub path: String,
    /// `None` for the legacy primary at [`REPO`].
    pub slug: Option<String>,
}

impl Config {
    pub(super) fn primary(&self) -> Target {
        Target {
            repository: self.repository.clone(),
            base: self.base.clone(),
            path: REPO.to_owned(),
            slug: None,
        }
    }

    /// Resolve the requested repository (default: the primary) to its clone.
    /// An unregistered repository is refused, never registered implicitly.
    pub(super) fn target(&self, requested: Option<&str>) -> Result<Target> {
        let Some(requested) = requested else {
            return Ok(self.primary());
        };
        if same_repository(&self.repository, requested) {
            return Ok(self.primary());
        }
        let registered = self
            .repositories
            .iter()
            .find(|r| same_repository(&r.repository, requested))
            .context("repository is not registered on this private account; run session start --private-clone URL for it while the account is idle")?;
        Ok(Target {
            repository: registered.repository.clone(),
            base: registered.base.clone(),
            path: format!("{WORK_ROOT}/{}", registered.slug),
            slug: Some(registered.slug.clone()),
        })
    }
}

/// Canonical identity of a forge repository: the key two spellings of one
/// repository share, and the collision-safe directory name derived from it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct RepoId {
    /// Credential-free clone URL, without a trailing `.git`.
    pub url: String,
    /// Lowercase `host[:port]/owner/name`.
    pub key: String,
    /// `owner-name-<8 hex of sha256(key)>`: one path component, the same
    /// basename under different owners (or hosts) never shares it.
    pub slug: String,
}

fn safe_segment(segment: &str) -> bool {
    segment
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_alphanumeric() || c == '_')
        && segment
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "._-".contains(c))
}

pub(super) fn identify(value: &str) -> Result<RepoId> {
    use sha2::{Digest, Sha256};
    let url = reqwest::Url::parse(&repository_url(value)?)?;
    let authority = match url.port() {
        Some(port) => format!("{}:{port}", url.host_str().unwrap_or("")),
        None => url.host_str().unwrap_or("").to_owned(),
    };
    let segments: Vec<&str> = url.path().trim_matches('/').split('/').collect();
    // A forge may sit under a path prefix (a Gitea sub-path install), so the
    // whole path is the identity and the last two segments name the directory.
    let [.., owner, name] = segments[..] else {
        bail!("private clones need an https://host/owner/name repository URL");
    };
    let name_segment = name.strip_suffix(".git").unwrap_or(name);
    if !segments.iter().all(|s| safe_segment(s)) || name_segment.is_empty() {
        bail!("repository path contains characters that are unsafe as a path component");
    }
    let path = segments[..segments.len() - 1].join("/");
    let key = format!("{authority}/{path}/{name_segment}").to_ascii_lowercase();
    let digest = hex::encode(Sha256::digest(key.as_bytes()));
    Ok(RepoId {
        url: format!("https://{authority}/{path}/{name_segment}"),
        slug: format!(
            "{}-{}-{}",
            owner.to_ascii_lowercase(),
            name_segment.to_ascii_lowercase(),
            &digest[..8]
        ),
        key,
    })
}

/// Whether two spellings name one repository. An unparseable one only matches
/// itself byte for byte (a legacy URL shape is never reinterpreted).
pub(super) fn same_repository(a: &str, b: &str) -> bool {
    match (identify(a), identify(b)) {
        (Ok(a), Ok(b)) => a.key == b.key,
        _ => a == b,
    }
}

#[derive(Debug, Serialize)]
pub struct Status {
    pub workspace_mode: &'static str,
    pub private_root: &'static str,
    pub config: Config,
    pub container_id: Option<String>,
    pub running: bool,
    pub lease: Option<lease::Job>,
    /// Secret-free containment-admission provenance for the CURRENT job
    /// (#8787), so `session status` reports what an execution-specific
    /// admission actually relied on. Absent on every ordinary session.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub admission: Option<serde_json::Value>,
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
    /// Repository the job acts on; defaults to the account's primary. Must
    /// already be registered with `session start --private-clone`.
    #[arg(long)]
    pub repository: Option<String>,
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
