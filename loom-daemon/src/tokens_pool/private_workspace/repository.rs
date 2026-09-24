use super::*;
use serde_json::json;
use std::io::{Read, Write};
use std::process::{Command, Stdio};

#[derive(clap::Subcommand)]
pub enum WorkerCommand {
    Protocol,
    /// Materialize private helper and hook paths.
    Setup,
    /// Read only, bounded checkpoint and Git metadata.
    Snapshot {
        #[arg(long)]
        issue: Option<u64>,
    },
    /// Refuse unsupported host execution from private workers.
    CheckHostJob,
    /// Validate direct adapter entry before any host Codex probe or launch.
    CheckAdapter {
        #[arg(long)]
        profile: Option<PathBuf>,
    },
    Execute {
        #[arg(last = true, required = true)]
        command: Vec<String>,
    },
    /// Verify containment policy for a mutable-role admission (#8787).
    VerifyPolicy {
        #[arg(long)]
        container_id: String,
        #[arg(long)]
        revision: String,
    },
    Prepare(PrepareArgs),
    /// Git credential-helper protocol; output goes only to Git's private pipe.
    Credential {
        operation: String,
    },
}

#[derive(clap::Args)]
pub struct PrepareArgs {
    #[arg(long)]
    pub account: String,
    #[arg(long)]
    pub repository: String,
    #[arg(long)]
    pub base: String,
    #[arg(long)]
    pub container_id: String,
    #[arg(long)]
    pub branch: Option<String>,
}

#[derive(Serialize, Deserialize)]
struct Identity {
    protocol: String,
    account: String,
    repository: String,
    container_id: String,
    revision: String,
}

impl WorkerCommand {
    pub fn run(self) -> Result<()> {
        match self {
            Self::Protocol => println!("{PROTOCOL}"),
            Self::Setup => println!("{}", serde_json::to_string(&worker_setup::report())?),
            Self::Snapshot { issue } => {
                println!("{}", serde_json::to_string(&export::snapshot(issue)?)?)
            }
            Self::CheckHostJob => worker_setup::check_host_job()?,
            Self::CheckAdapter { profile } => {
                adapter::check_environment()?;
                adapter::check(profile.as_deref())?;
            }
            Self::Execute { command } => worker_setup::execute(command)?,
            Self::VerifyPolicy {
                container_id,
                revision,
            } => println!(
                "{}",
                serde_json::to_string(&containment::report(&container_id, &revision))?
            ),
            Self::Prepare(args) => {
                repository_url(&args.repository)?;
                match prepare(Path::new(ROOT), &args) {
                    Ok(revision) => {
                        println!("{}", json!({"protocol": PROTOCOL, "revision": revision}))
                    }
                    Err(error) => {
                        println!("{}", json!({"protocol": PROTOCOL, "error": error.to_string()}))
                    }
                }
            }
            Self::Credential { operation } => credential(&operation)?,
        }
        Ok(())
    }
}

fn git(root: &Path, args: &[&str]) -> Result<String> {
    let result = Command::new("git")
        .arg("-C")
        .arg(root)
        .args([
            "-c",
            "credential.helper=",
            "-c",
            "credential.helper=!loom-daemon private-workspace credential",
        ])
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null())
        .output()
        .context("start workspace Git operation")?;
    if !result.status.success() {
        // Neither URLs nor Git's stderr are emitted: a configured helper may
        // include a secret in its own diagnostic. Preserve the clone in place.
        bail!(
            "workspace Git {} failed ({}); work is retained for recovery",
            args.first().unwrap_or(&"operation"),
            result.status
        );
    }
    String::from_utf8(result.stdout).context("workspace Git returned invalid text")
}

pub(super) fn prepare(root: &Path, args: &PrepareArgs) -> Result<String> {
    valid_base(&args.base)?;
    if args.account.is_empty() || args.container_id.is_empty() {
        bail!("workspace account and container identity are required");
    }
    let root = root.canonicalize()?;
    let repo = root.join("repo");
    let record = root.join("identity.json");
    let mut identity = if record.exists() {
        let existing: Identity = serde_json::from_slice(&std::fs::read(&record)?)?;
        if existing.protocol != PROTOCOL
            || existing.account != args.account
            || existing.repository != args.repository
        {
            bail!("private clone identity mismatch; do not reset or remove unknown work");
        }
        existing
    } else {
        // Empty means truly empty; an interrupted/foreign clone is not ours to
        // clean. Docker's named volume does not require filesystem lost+found.
        if std::fs::read_dir(&root)?.next().is_some() {
            bail!("unrecognized nonempty workspace volume; retain it for recovery");
        }
        let identity = Identity {
            protocol: PROTOCOL.into(),
            account: args.account.clone(),
            repository: args.repository.clone(),
            container_id: args.container_id.clone(),
            revision: String::new(),
        };
        save(&record, &identity)?;
        identity
    };
    if !repo.exists() {
        git(
            &root,
            &[
                "clone",
                "--no-local",
                "--no-hardlinks",
                "--",
                &args.repository,
                "repo",
            ],
        )?;
    }
    if !repo.join(".git").is_dir()
        || repo.canonicalize()? != repo
        || repo.join(".git").canonicalize()? != repo.join(".git")
        || repo.join(".git/objects/info/alternates").exists()
    {
        bail!("workspace must own a real clone and private Git objects, without symlinks or alternates");
    }
    private_metadata(&repo.join(".git"))?;
    if git(&repo, &["remote", "get-url", "origin"])?.trim() != args.repository {
        bail!("private clone origin changed; preserve work and restore its verified identity");
    }
    clean_worktrees(&root, &repo)?;
    git(&repo, &["fetch", "--prune", "origin"])?;
    if !git(&repo, &["rev-list", "--branches", "--not", "--remotes=origin"])?
        .trim()
        .is_empty()
    {
        bail!("private clone contains unpushed commits; push/recover them before reusing the account (nothing was reset)");
    }
    // Detached HEAD can contain work not named by a branch; inspect every
    // worktree HEAD as well as branch refs before changing the root checkout.
    {
        for worktree in worktrees(&repo)? {
            if !git(&worktree, &["rev-list", "HEAD", "--not", "--remotes=origin"])?
                .trim()
                .is_empty()
            {
                bail!("private worktree has unpushed/detached commits; preserve it for recovery");
            }
        }
    }
    let remote = format!("refs/remotes/origin/{}", args.base);
    let revision = git(&repo, &["rev-parse", "--verify", &format!("{remote}^{{commit}}")])
        .or_else(|_| {
            if args.base.len() == 40 && args.base.chars().all(|c| c.is_ascii_hexdigit()) {
                git(
                    &repo,
                    &[
                        "rev-parse",
                        "--verify",
                        &format!("{}^{{commit}}", args.base),
                    ],
                )
            } else {
                bail!("base branch is not present on origin");
            }
        })?
        .trim()
        .to_owned();
    if let Some(branch) = &args.branch {
        git(&repo, &["check-ref-format", "--branch", branch])?;
        // Never reset an existing branch. Resumption keeps its published tip.
        if git(&repo, &["show-ref", "--verify", &format!("refs/heads/{branch}")]).is_ok() {
            git(&repo, &["switch", branch])?;
        } else {
            let remote_branch = format!("refs/remotes/origin/{branch}");
            let start = if git(&repo, &["show-ref", "--verify", &remote_branch]).is_ok() {
                &remote_branch
            } else {
                &revision
            };
            git(&repo, &["switch", "--create", branch, start])?;
        }
    } else {
        git(&repo, &["switch", "--detach", &revision])?;
    }
    git(
        &repo,
        &[
            "config",
            "credential.helper",
            "!loom-daemon private-workspace credential",
        ],
    )?;
    std::fs::create_dir_all(root.join("cache"))?;
    identity.container_id = args.container_id.clone();
    identity.revision.clone_from(&revision);
    save(&record, &identity)?;
    Ok(revision)
}

fn worktrees(repo: &Path) -> Result<Vec<PathBuf>> {
    Ok(git(repo, &["worktree", "list", "--porcelain", "-z"])?
        .split('\0')
        .filter_map(|field| field.strip_prefix("worktree "))
        .map(PathBuf::from)
        .collect())
}

fn clean_worktrees(root: &Path, repo: &Path) -> Result<()> {
    for path in worktrees(repo)? {
        if !path.canonicalize()?.starts_with(root) {
            bail!("worktree points outside the private volume; refuse reuse");
        }
        if !git(&path, &["status", "--porcelain=v1", "--untracked-files=all"])?.is_empty() {
            bail!("private clone/worktree is dirty; retain and recover its work before reuse");
        }
    }
    Ok(())
}

fn credential(operation: &str) -> Result<()> {
    if operation != "get" {
        return Ok(());
    }
    let repository = std::env::var("LOOM_PRIVATE_REPOSITORY")
        .context("credential request has no repository scope")?;
    let expected = reqwest::Url::parse(&repository)?;
    let mut request = String::new();
    std::io::stdin().take(8192).read_to_string(&mut request)?;
    let host = request.lines().find_map(|line| line.strip_prefix("host="));
    let scheme = request
        .lines()
        .find_map(|line| line.strip_prefix("protocol="));
    let expected_host = match expected.port() {
        Some(port) => format!("{}:{port}", expected.host_str().unwrap_or("")),
        None => expected.host_str().unwrap_or("").to_owned(),
    };
    if scheme != Some("https") || host != Some(expected_host.as_str()) {
        bail!("credential request is outside the configured forge");
    }
    let gh_host = std::env::var("GH_HOST").unwrap_or_else(|_| "github.com".into());
    let github = expected.host_str() == Some(gh_host.as_str());
    let gitea_username = std::env::var("GITEA_USERNAME").ok();
    let username = credential_username(github, gitea_username.as_deref())?;
    let names = if github {
        ["GH_TOKEN", "GITHUB_TOKEN"]
    } else {
        ["GITEA_TOKEN", "FORGE_TOKEN"]
    };
    for name in names {
        if let Ok(token) = std::env::var(name) {
            if !token.is_empty() && !token.chars().any(char::is_control) {
                println!("username={username}\npassword={token}\n");
                return Ok(());
            }
        }
    }
    let mut helper = Command::new("gh").args(["auth", "git-credential", "get"])
        .stdin(Stdio::piped()).stdout(Stdio::inherit()).stderr(Stdio::null()).spawn()
        .context("forge authentication unavailable; provide an external gh profile or supported token environment")?;
    helper.stdin.take().unwrap().write_all(request.as_bytes())?;
    if !helper.wait()?.success() {
        bail!("forge authentication helper failed");
    }
    Ok(())
}

pub(super) fn credential_username(github: bool, gitea_username: Option<&str>) -> Result<&str> {
    let username = if github {
        "x-access-token"
    } else {
        gitea_username
            .filter(|name| !name.is_empty())
            .unwrap_or("x-access-token")
    };
    if username.contains(':') || username.chars().any(char::is_control) {
        bail!("Gitea username must not contain ':' or control characters");
    }
    Ok(username)
}

fn private_metadata(path: &Path) -> Result<()> {
    let metadata = std::fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() {
        bail!("private Git metadata/objects must not contain symlinks");
    }
    if metadata.is_dir() {
        for entry in std::fs::read_dir(path)? {
            private_metadata(&entry?.path())?;
        }
    }
    Ok(())
}
