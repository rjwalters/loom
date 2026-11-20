use super::*;
use serde_json::json;
use std::io::Read;
use std::process::{Command, Stdio};

#[derive(clap::Subcommand)]
pub enum WorkerCommand {
    Protocol,
    /// Materialize private helper and hook paths.
    Setup,
    /// Materialize the account profile's control files before the session
    /// container binds them read-only. Host-driven, in a throwaway container.
    ProvisionControls,
    /// Report the versioned control boundary (guard code, forced policy, hook
    /// registration, readiness receipt) for host verification and binding.
    Control,
    /// Seal a staged control bundle at image build time (never at runtime).
    SealControl {
        #[arg(long, default_value = bundle::CONTROL_ROOT)]
        root: PathBuf,
    },
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
    Prepare(PrepareArgs),
    /// List, publish or discard the unpublished branches of this clone
    /// (`accounts session recover`, issue #14030). Without `--publish` or
    /// `--discard` nothing is mutated.
    Recover(RecoverArgs),
    /// Move an additional clone to its canonical (transferred) repository
    /// location, preserving every branch and file (`accounts session
    /// rebind`, issue #14357).
    Rebind(RebindArgs),
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
    /// Collision-safe directory name of an additional repository (#10795),
    /// cloned at `<volume>/work/<slug>`. Absent: the legacy `<volume>/repo`.
    #[arg(long)]
    pub slug: Option<String>,
}

#[derive(clap::Args)]
pub struct RecoverArgs {
    #[arg(long)]
    pub account: String,
    #[arg(long)]
    pub repository: String,
    /// The base the checkout is moved to when a discarded branch held it.
    #[arg(long)]
    pub base: String,
    /// Publish every in-scope unpublished branch to origin.
    #[arg(long, conflicts_with = "discard")]
    pub publish: bool,
    /// Delete the named branches with their unpublished commits. Requires an
    /// explicit branch scope; never touches a branch it was not named for.
    #[arg(long, conflicts_with = "publish")]
    pub discard: bool,
    /// Scope the action to one issue's own branch (`feature/issue-N`).
    #[arg(long)]
    pub issue: Option<u64>,
    /// Scope the action to a named branch (repeatable).
    #[arg(long, value_name = "BRANCH")]
    pub branch: Vec<String>,
    /// With `--discard`: remove this linked worktree (repeatable) and whatever
    /// uncommitted or untracked files it holds, when nothing at its HEAD is
    /// unpublished (#14458). Never deletes a branch; never touches a worktree
    /// it was not named for.
    #[arg(long, value_name = "PATH", requires = "discard", conflicts_with_all = ["branch", "issue"])]
    pub worktree: Vec<PathBuf>,
    /// Validate which paths this clone owns before a cross-clone discard.
    #[arg(long, requires = "worktree", hide = true)]
    pub validate_worktrees: bool,
    /// Collision-safe directory name of an additional repository (#10795).
    /// Absent: the legacy `<volume>/repo`.
    #[arg(long)]
    pub slug: Option<String>,
}

#[derive(clap::Args)]
pub struct RebindArgs {
    #[arg(long)]
    pub account: String,
    /// The registered (pre-transfer) repository URL.
    #[arg(long)]
    pub from: String,
    #[arg(long)]
    pub from_slug: String,
    /// The canonical repository URL after the transfer.
    #[arg(long)]
    pub to: String,
    #[arg(long)]
    pub to_slug: String,
    #[arg(long)]
    pub base: String,
    /// Verify and report; change nothing.
    #[arg(long)]
    pub dry_run: bool,
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
            Self::ProvisionControls => {
                println!("{}", serde_json::to_string(&worker_setup::provision_controls())?)
            }
            Self::Control => println!(
                "{}",
                serde_json::to_string(&bundle::observe(
                    Path::new(bundle::CONTROL_ROOT),
                    Path::new(PROFILE)
                ))?
            ),
            Self::SealControl { root } => {
                println!("{}", serde_json::to_string(&bundle::seal(&root)?)?)
            }
            Self::Snapshot { issue } => {
                println!("{}", serde_json::to_string(&export::snapshot(issue)?)?)
            }
            Self::CheckHostJob => worker_setup::check_host_job()?,
            Self::CheckAdapter { profile } => {
                adapter::check_environment()?;
                adapter::check(profile.as_deref())?;
            }
            Self::Execute { command } => worker_setup::execute(command)?,
            Self::Prepare(args) => {
                repository_url(&args.repository)?;
                match prepare(Path::new(ROOT), &args) {
                    Ok(outcome) => println!(
                        "{}",
                        json!({
                            "protocol": PROTOCOL,
                            "revision": outcome.revision,
                            "reaped_worktrees": outcome.reaped,
                        })
                    ),
                    Err(error) => {
                        println!("{}", json!({"protocol": PROTOCOL, "error": error.to_string()}))
                    }
                }
            }
            Self::Recover(args) => {
                repository_url(&args.repository)?;
                valid_base(&args.base)?;
                match recover(Path::new(ROOT), &args) {
                    Ok(outcome) => {
                        println!("{}", json!({"protocol": PROTOCOL, "outcome": outcome}))
                    }
                    Err(error) => {
                        println!("{}", json!({"protocol": PROTOCOL, "error": error.to_string()}))
                    }
                }
            }
            Self::Rebind(args) => {
                repository_url(&args.from)?;
                repository_url(&args.to)?;
                valid_base(&args.base)?;
                match rebind(Path::new(ROOT), &args) {
                    Ok(outcome) => {
                        println!("{}", json!({"protocol": PROTOCOL, "outcome": outcome}))
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

/// The credential helper's own diagnostics (`credential`), the only text from
/// Git's stderr that is ever surfaced (#14433).
const HELPER_CAUSES: [&str; 2] = [
    "forge authentication helper failed",
    "forge authentication unavailable",
];

fn helper_cause(stderr: &[u8]) -> Option<&'static str> {
    let text = String::from_utf8_lossy(stderr);
    HELPER_CAUSES.into_iter().find(|cause| text.contains(cause))
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
        // The one exception is a fixed allowlist of this binary's own helper
        // messages (never Git's text), so a failed push names its cause.
        let cause = helper_cause(&result.stderr)
            .map(|cause| format!(", cause: {cause}"))
            .unwrap_or_default();
        bail!(
            "workspace Git {} failed ({}{cause}); work is retained for recovery",
            args.first().unwrap_or(&"operation"),
            result.status
        );
    }
    String::from_utf8(result.stdout).context("workspace Git returned invalid text")
}

/// What one `prepare` resolved, plus the finished-PR linked worktrees its
/// automatic reap removed (display paths, same form as
/// `RecoverOutcome::removed_worktrees`).
#[derive(Clone, Debug, PartialEq)]
pub(super) struct PrepareOutcome {
    pub(super) revision: String,
    pub(super) reaped: Vec<String>,
}

pub(super) fn prepare(root: &Path, args: &PrepareArgs) -> Result<PrepareOutcome> {
    prepare_with_pr_lookup(root, args, &finished_pr)
}

/// The real lookup uses the credentials that the host already scopes to this
/// clone's exec. Local tests supply a lookup and never contact a forge.
pub(super) fn prepare_with_pr_lookup(
    root: &Path,
    args: &PrepareArgs,
    has_finished_pr: &dyn Fn(&Path, &str, &str, &str) -> bool,
) -> Result<PrepareOutcome> {
    valid_base(&args.base)?;
    if args.account.is_empty() || args.container_id.is_empty() {
        bail!("workspace account and container identity are required");
    }
    let root = root.canonicalize()?;
    let (repo, record, staging) = match &args.slug {
        None => (root.join("repo"), root.join("identity.json"), None),
        Some(slug) => additional_paths(&root, slug, &args.account)?,
    };
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
        if staging.is_none() && std::fs::read_dir(&root)?.next().is_some() {
            bail!("unrecognized nonempty workspace volume; retain it for recovery");
        }
        if staging.is_some() && repo.symlink_metadata().is_ok() {
            bail!(
                "unrecognized directory where an additional clone belongs; retain it for recovery"
            );
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
    if let Some(staging) = &staging {
        if repo.symlink_metadata().is_err() {
            // A staging directory is only ever a clone that never completed:
            // nothing runs in it and it is not the ready path, so it carries
            // no work and is safe to discard (a ready clone is never staged).
            discard_staging(staging)?;
            let relative = staging.strip_prefix(&root)?.to_string_lossy().into_owned();
            git(
                &root,
                &[
                    "clone",
                    "--no-local",
                    "--no-hardlinks",
                    "--",
                    &args.repository,
                    &relative,
                ],
            )?;
            // The ready path appears only once the clone is complete.
            std::fs::rename(staging, &repo)?;
        }
    } else if !repo.exists() {
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
    if Path::new(git(&repo, &["rev-parse", "--show-toplevel"])?.trim()).canonicalize()? != repo {
        bail!("workspace clone is not its own independent Git root");
    }
    if git(&repo, &["remote", "get-url", "origin"])?.trim() != args.repository {
        bail!("private clone origin changed; preserve work and restore its verified identity");
    }
    git(&repo, &["fetch", "--prune", "origin"])?;
    let reaped = reap_finished_worktrees(&root, &repo, &args.repository, has_finished_pr)?;
    clean_worktrees(&root, &repo, &args.account)?;
    // Unpublished commits are only stranded when no branch names them:
    // preparation never moves a branch (a resumed one keeps its local tip)
    // and never touches another one, so work carried by a branch ref
    // survives every later step — on this clone and on every other clone of
    // the account, whose preparations are independent (#14030). A detached
    // HEAD here or in a linked worktree can be lost by re-preparing and
    // still refuses; the refusal names the branches the `accounts session
    // recover` verb clears.
    let stranded = stranded_heads(&repo)?;
    if !stranded.is_empty() {
        let mut detail = String::new();
        for head in &stranded {
            let short = head.revision.get(..8).unwrap_or(&head.revision);
            detail.push_str(&format!("\n  {} at {} ({} unpushed)", head.path, short, head.ahead));
        }
        let mut message = format!(
            "private work is stranded on a detached HEAD and would be lost by re-preparation{detail}; \
             name it with a branch (git switch -c NAME) before reuse (nothing was reset)"
        );
        let unpublished = unpublished_branches(&repo)?;
        if !unpublished.is_empty() {
            let names = unpublished
                .iter()
                .map(|b| format!("{} ({} ahead)", b.branch, b.ahead))
                .collect::<Vec<_>>()
                .join(", ");
            message.push_str(&format!(
                "; unpublished branches {names} can be cleared with \
                 `loom-daemon accounts session recover {} --publish` \
                 (or `--discard --branch BRANCH`)",
                args.account
            ));
        }
        bail!("{message}");
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
            // A contained step creates its own worktree under the clone
            // (#14122): clean and inside the volume, so `clean_worktrees`
            // passes it, `stranded_heads` skips its named branch, and Git
            // would refuse the switch below with its stderr suppressed.
            // Name the holder and the cure that actually clears each shape
            // of the hold instead.
            if let Some(holder) = holding_worktree(&repo, branch)? {
                bail!("{}", held_branch_refusal(&repo, &holder, branch, &args.account)?);
            }
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
    Ok(PrepareOutcome { revision, reaped })
}

/// Worktree HEADs (the main checkout included) whose detached HEAD carries
/// commits no ref of `origin` reaches: preparation could leave them unnamed.
pub(super) fn stranded_heads(repo: &Path) -> Result<Vec<DetachedWork>> {
    let mut stranded = Vec::new();
    for worktree in worktrees(repo)? {
        // `rev-parse --abbrev-ref HEAD` answers `HEAD` only when detached.
        if git(&worktree, &["rev-parse", "--abbrev-ref", "HEAD"])?.trim() != "HEAD" {
            continue;
        }
        let ahead = ahead_of_origin(&worktree, "HEAD")?;
        if ahead > 0 {
            stranded.push(DetachedWork {
                path: worktree.display().to_string(),
                revision: git(&worktree, &["rev-parse", "HEAD"])?.trim().to_owned(),
                ahead,
            });
        }
    }
    Ok(stranded)
}

/// The first linked worktree whose checkout holds `branch`, if any. At most
/// one exists: Git never checks a branch out in two worktrees. The main
/// checkout is skipped — holding the branch there is what preparation itself
/// establishes, and switching to it is a no-op, not a refusal.
pub(super) fn holding_worktree(repo: &Path, branch: &str) -> Result<Option<PathBuf>> {
    let main = repo.canonicalize()?;
    for worktree in worktrees(repo)? {
        if worktree.canonicalize()? == main {
            continue;
        }
        if git(&worktree, &["rev-parse", "--abbrev-ref", "HEAD"])?.trim() == branch {
            return Ok(Some(worktree));
        }
    }
    Ok(None)
}

/// The refusal for a linked worktree holding the requested branch (#14122).
/// Names the holding path, the branch, and the cure that actually clears the
/// hold. Recovery cannot clear a published branch's hold — `--publish` only
/// re-pushes and `--discard` never deletes a branch carrying no unpublished
/// commits — so an already-published hold names the hand removal that works.
pub(super) fn held_branch_refusal(
    repo: &Path,
    holder: &Path,
    branch: &str,
    account: &str,
) -> Result<String> {
    let ahead = ahead_of_origin(repo, &format!("refs/heads/{branch}"))?;
    let mut message = format!(
        "linked worktree {} holds {branch}; Git refuses a branch already \
         checked out in another worktree, and the hold survives every reuse",
        holder.display()
    );
    if ahead > 0 {
        message.push_str(&format!(
            "; clear it with `loom-daemon accounts session recover {account} \
             --discard --branch {branch}` (removes this worktree and deletes \
             the branch, its {ahead} unpublished commit(s) included), or push \
             them first (`recover {account} --publish`) and remove the \
             worktree by hand (`accounts session attach {account}`, then \
             `git worktree remove {path}`) — a published branch is never \
             discarded",
            path = holder.display(),
        ));
    } else {
        message.push_str(&format!(
            "; the branch is already on origin, where `recover {account}` \
             cannot clear the hold (`--publish` only re-pushes it, `--discard` \
             ignores a branch with no unpublished commits), so remove the \
             worktree by hand (`accounts session attach {account}`, then \
             `git worktree remove {path}`) — nothing unpublished is lost",
            path = holder.display(),
        ));
    }
    Ok(message)
}

/// Every local branch with commits no remote-tracking ref of `origin`
/// reaches, with its ahead count.
pub(super) fn unpublished_branches(repo: &Path) -> Result<Vec<UnpublishedBranch>> {
    let names = git(repo, &["for-each-ref", "--format=%(refname:short)", "refs/heads"])?;
    let mut unpublished = Vec::new();
    for name in names.lines().map(str::trim).filter(|name| !name.is_empty()) {
        let ahead = ahead_of_origin(repo, &format!("refs/heads/{name}"))?;
        if ahead > 0 {
            unpublished.push(UnpublishedBranch {
                branch: name.to_owned(),
                ahead,
            });
        }
    }
    Ok(unpublished)
}

fn ahead_of_origin(repo: &Path, revision: &str) -> Result<u64> {
    git(repo, &["rev-list", "--count", revision, "--not", "--remotes=origin"])?
        .trim()
        .parse::<u64>()
        .context("workspace Git returned a non-numeric ahead count")
}

/// The clone paths a `recover` call acts on. Unlike `additional_paths` this
/// never creates anything: a clone that was never materialized has nothing
/// to recover.
fn recover_paths(root: &Path, args: &RecoverArgs) -> Result<(PathBuf, PathBuf)> {
    match &args.slug {
        None => Ok((root.join("repo"), root.join("identity.json"))),
        Some(slug) => {
            if !safe_segment(slug) || slug.contains("..") {
                bail!("additional repository directory name is unsafe");
            }
            let primary: Identity =
                serde_json::from_slice(&std::fs::read(root.join("identity.json")).context(
                    "the account's primary workspace is not prepared; start the session first",
                )?)?;
            if primary.protocol != PROTOCOL || primary.account != args.account {
                bail!("private workspace account identity mismatch; do not reset or remove unknown work");
            }
            Ok((
                root.join("work").join(slug),
                root.join("work")
                    .join(".identity")
                    .join(format!("{slug}.json")),
            ))
        }
    }
}

/// Nothing to recover where the account never materialized a clone.
fn empty_outcome(repository: &str) -> RecoverOutcome {
    RecoverOutcome {
        repository: repository.to_owned(),
        cloned: false,
        branches: Vec::new(),
        published: Vec::new(),
        discarded: Vec::new(),
        ignored: Vec::new(),
        detached: Vec::new(),
        dirty_worktrees: Vec::new(),
        removed_worktrees: Vec::new(),
    }
}

/// List, publish or discard the clone's unpublished branches (#14030).
/// Publish only ever fast-forwards `origin`; discard requires an explicit
/// branch scope, deletes only branches it was named for, and never removes
/// the volume or moves a branch ref other than the ones being discarded.
pub(super) fn recover(root: &Path, args: &RecoverArgs) -> Result<RecoverOutcome> {
    if !args.worktree.is_empty() && (!args.branch.is_empty() || args.issue.is_some()) {
        bail!("discard accepts a worktree scope or a branch/issue scope, separately");
    }
    let (repo, record) = recover_paths(root, args)?;
    if !repo.join(".git").is_dir() {
        let mut outcome = empty_outcome(&args.repository);
        if args.validate_worktrees {
            outcome.ignored = args
                .worktree
                .iter()
                .map(|path| path.display().to_string())
                .collect();
        } else if !args.worktree.is_empty() {
            bail!("named worktree is unavailable in this clone");
        }
        return Ok(outcome);
    }
    let identity: Identity = serde_json::from_slice(&std::fs::read(&record)?)?;
    if identity.protocol != PROTOCOL
        || identity.account != args.account
        || identity.repository != args.repository
    {
        bail!("private clone identity mismatch; do not reset or remove unknown work");
    }
    let mut outcome = RecoverOutcome {
        repository: args.repository.clone(),
        cloned: true,
        branches: unpublished_branches(&repo)?,
        published: Vec::new(),
        discarded: Vec::new(),
        ignored: Vec::new(),
        detached: stranded_heads(&repo)?,
        dirty_worktrees: Vec::new(),
        removed_worktrees: Vec::new(),
    };
    if !args.worktree.is_empty() {
        let targets = named_worktrees(root, &repo, &args.worktree, &mut outcome.ignored)?;
        if args.validate_worktrees {
            return Ok(outcome);
        }
        if !args.discard || !outcome.ignored.is_empty() {
            bail!(
                "discard requires paths in this clone; unavailable: {}",
                outcome.ignored.join(", ")
            );
        }
        for path in targets {
            git(&repo, &["worktree", "remove", "--force", &path.display().to_string()])?;
            outcome.removed_worktrees.push(path.display().to_string());
        }
    }
    let mut scope = args.branch.clone();
    if let Some(issue) = args.issue {
        let name = format!("feature/issue-{issue}");
        git(&repo, &["check-ref-format", "--branch", &name])?;
        scope.push(name);
    }
    scope.sort();
    scope.dedup();
    if args.discard && scope.is_empty() && args.worktree.is_empty() {
        bail!("discard requires an explicit scope (--branch BRANCH, --issue N or --worktree PATH)");
    }
    if !scope.is_empty() {
        let local = git(&repo, &["for-each-ref", "--format=%(refname:short)", "refs/heads"])?;
        for name in &scope {
            if !local.lines().any(|line| line.trim() == name) {
                outcome.ignored.push(name.clone());
            }
        }
    }
    if args.publish {
        let targets: Vec<String> = if scope.is_empty() {
            outcome
                .branches
                .iter()
                .map(|b| b.branch.clone())
                .collect::<Vec<_>>()
        } else {
            scope
                .iter()
                .filter(|name| !outcome.ignored.contains(*name))
                .cloned()
                .collect::<Vec<_>>()
        };
        for name in &targets {
            publish(&repo, name)?;
            outcome.published.push(name.clone());
        }
    } else if args.discard && !scope.is_empty() {
        // A scoped branch with nothing unpublished is left alone: deleting a
        // published (or absent) branch is never part of a discard.
        let targets: Vec<String> = scope
            .iter()
            .filter(|name| outcome.branches.iter().any(|b| name.as_str() == b.branch))
            .cloned()
            .collect();
        for name in &scope {
            if !targets.contains(name) && !outcome.ignored.contains(name) {
                outcome.ignored.push(name.clone());
            }
        }
        // Validate every holding checkout and resolve the discard base
        // before mutating any of them, so a dirty worktree or an
        // unresolvable base cannot leave a half-done discard behind.
        let holds = holds_of(&repo, &targets)?;
        let main_target = if holds.main.is_empty() {
            None
        } else {
            Some(discard_target(&repo, &args.base)?)
        };
        for name in &targets {
            discard_branch(&repo, &holds, name, main_target.as_deref())?;
            outcome.discarded.push(name.clone());
        }
    }
    outcome.branches = unpublished_branches(&repo)?;
    outcome.detached = stranded_heads(&repo)?;
    outcome.dirty_worktrees = dirty_worktrees(root, &repo)?;
    Ok(outcome)
}

fn clone_state(path: &Path) -> Result<CloneState> {
    if !path.join(".git").is_dir() {
        return Ok(CloneState {
            path: path.to_string_lossy().into_owned(),
            ..CloneState::default()
        });
    }
    Ok(CloneState {
        path: path.to_string_lossy().into_owned(),
        populated: true,
        branches: unpublished_branches(path)?,
        dirty: is_dirty(path)?,
        detached: stranded_heads(path)?.len(),
    })
}

fn is_dirty(path: &Path) -> Result<bool> {
    Ok(!git(path, &["status", "--porcelain=v1", "--untracked-files=all"])?.is_empty())
}

/// The local work a move must leave exactly as it found it: every branch tip,
/// the checked-out revision and the uncommitted files. Unlike [`CloneState`]
/// this holds nothing derived from remote-tracking refs, so a branch published
/// (or a remote ref pruned) while the move runs cannot change it.
#[derive(Debug, PartialEq)]
struct LocalWork {
    tips: String,
    head: String,
    dirty: bool,
}

fn local_work(path: &Path) -> Result<LocalWork> {
    Ok(LocalWork {
        tips: git(
            path,
            &[
                "for-each-ref",
                "--format=%(objectname) %(refname)",
                "refs/heads",
            ],
        )?,
        // An unborn HEAD has no revision to compare: both sides read empty.
        head: git(path, &["rev-parse", "HEAD"]).unwrap_or_default(),
        dirty: is_dirty(path)?,
    })
}

fn read_identity(record: &Path, account: &str) -> Result<Option<Identity>> {
    if !record.exists() {
        return Ok(None);
    }
    let identity: Identity = serde_json::from_slice(&std::fs::read(record)?)?;
    if identity.protocol != PROTOCOL || identity.account != account {
        bail!("private clone identity mismatch; do not reset or remove unknown work");
    }
    Ok(Some(identity))
}

/// A clone location must be absent or a real clone: anything else is not ours.
fn require_clone_or_absent(path: &Path) -> Result<()> {
    if path.symlink_metadata().is_ok() && !path.join(".git").is_dir() {
        bail!("unrecognized directory at {}; retain it for recovery", path.display());
    }
    Ok(())
}

/// The canonical repository must be the transferred one: its base branch
/// shares history with the clone's own `origin/<base>`. Reads the remote into
/// `FETCH_HEAD` only; no ref or branch of the clone moves.
fn verify_canonical(repo: &Path, to: &str, base: &str) -> Result<()> {
    git(
        repo,
        &[
            "rev-parse",
            "--verify",
            &format!("refs/remotes/origin/{base}^{{commit}}"),
        ],
    )
    .context("the clone has no origin/<base> to compare the canonical repository with")?;
    git(repo, &["fetch", "--no-tags", "--", to, base])
        .context("cannot read the base branch of the canonical repository")?;
    git(
        repo,
        &[
            "merge-base",
            &format!("refs/remotes/origin/{base}"),
            "FETCH_HEAD",
        ],
    )
    .context(
        "wrong repository: the canonical URL's base branch shares no history with this clone",
    )?;
    Ok(())
}

/// Move an additional clone to the slug of its canonical (post-transfer)
/// repository and re-point it (#14357). Nothing is deleted: the directory is
/// renamed on the same volume, the identity record is rewritten, and `origin`
/// is set. Both locations populated is reported and left intact. Every step is
/// ordered so an interrupted run is finished by repeating it.
pub(super) fn rebind(root: &Path, args: &RebindArgs) -> Result<RebindOutcome> {
    for slug in [&args.from_slug, &args.to_slug] {
        if !safe_segment(slug) || slug.contains("..") {
            bail!("additional repository directory name is unsafe");
        }
    }
    if args.from_slug == args.to_slug {
        bail!("old and canonical repositories share one directory; this is not a transfer");
    }
    let root = root.canonicalize()?;
    read_identity(&root.join("identity.json"), &args.account)?
        .context("the account's primary workspace is not prepared; start the session first")?;
    let work = root.join("work");
    let identities = work.join(".identity");
    let (old, new) = (work.join(&args.from_slug), work.join(&args.to_slug));
    let old_record = identities.join(format!("{}.json", args.from_slug));
    let new_record = identities.join(format!("{}.json", args.to_slug));
    require_clone_or_absent(&old)?;
    require_clone_or_absent(&new)?;
    let (old_pop, new_pop) = (old.join(".git").is_dir(), new.join(".git").is_dir());
    let outcome = |status, from: CloneState, to: CloneState| Ok(RebindOutcome { status, from, to });
    if !old_pop && !new_pop {
        return outcome(RebindStatus::NotCloned, clone_state(&old)?, clone_state(&new)?);
    }
    if old_pop && new_pop {
        return outcome(RebindStatus::BothPopulated, clone_state(&old)?, clone_state(&new)?);
    }
    let repo = if old_pop { &old } else { &new };
    if repo.canonicalize()? != *repo || !repo.join(".git").canonicalize()?.starts_with(repo) {
        bail!("workspace clone must be a real directory without symlinks");
    }
    let before = clone_state(repo)?;
    let local_before = local_work(repo)?;
    // A linked worktree records absolute paths into the old location.
    if worktrees(repo)?.len() > 1 {
        bail!(
            "the clone has linked worktrees whose paths a move would break; \
             finish or remove them first (nothing was changed)"
        );
    }
    let origin = git(repo, &["remote", "get-url", "origin"])?;
    let origin = origin.trim();
    // A leftover record at the canonical slug from an earlier preparation that
    // never cloned must not shadow the record of the clone that exists.
    let record = if old_pop {
        read_identity(&old_record, &args.account)?
    } else {
        match read_identity(&new_record, &args.account)? {
            Some(found) => Some(found),
            None => read_identity(&old_record, &args.account)?,
        }
    };
    let record = record.context("the clone has no identity record; retain it for recovery")?;
    let known = |url: &str| same_repository(url, &args.from) || same_repository(url, &args.to);
    if !known(&record.repository) || !known(origin) {
        bail!("private clone identity mismatch; do not reset or remove unknown work");
    }
    let settled =
        !old_pop && record.repository == args.to && origin == args.to && !old_record.exists();
    if !settled {
        verify_canonical(repo, &args.to, &args.base)?;
    }
    if args.dry_run {
        let status = if settled {
            RebindStatus::AlreadyBound
        } else {
            RebindStatus::DryRun
        };
        let (from, to) = if old_pop {
            (before, clone_state(&new)?)
        } else {
            (clone_state(&old)?, before)
        };
        return outcome(status, from, to);
    }
    if old_pop {
        std::fs::rename(&old, &new)?;
    }
    // Record before origin before retiring the old record: each step is
    // idempotent, and a crash leaves the clone at its new directory with a
    // record or origin the repeat recognizes (either spelling) and finishes.
    let mut identity = record;
    identity.repository.clone_from(&args.to);
    save(&new_record, &identity)?;
    git(&new, &["remote", "set-url", "origin", &args.to])?;
    if old_record.exists() {
        std::fs::remove_file(&old_record)?;
    }
    git(&new, &["fetch", "--prune", "origin"])?;
    // Verify what the move could have lost (branch tips, HEAD, dirty files),
    // not the unpublished counts: `fetch --prune` moves the remote-tracking
    // refs those are derived from, so an upstream publish or prune is not a
    // bad move (the report below shows the post-fetch counts).
    if local_work(&new)? != local_before {
        bail!(
            "the clone's branches or files changed during the move; inspect {} before retrying",
            new.display()
        );
    }
    let after = clone_state(&new)?;
    outcome(
        if old_pop {
            RebindStatus::Moved
        } else {
            RebindStatus::AlreadyBound
        },
        if old_pop { before } else { clone_state(&old)? },
        after,
    )
}

/// Push `branch` to `origin` without ever forcing: a diverged remote refuses.
fn publish(repo: &Path, branch: &str) -> Result<()> {
    git(
        repo,
        &[
            "push",
            "origin",
            &format!("refs/heads/{branch}:refs/heads/{branch}"),
        ],
    )?;
    Ok(())
}

/// The worktrees (linked ones and the main checkout) that hold each branch.
struct Holds {
    /// Linked worktree to remove, per branch.
    worktrees: Vec<(String, PathBuf)>,
    /// The main checkout holds this branch and must be moved off it.
    main: Vec<String>,
}

fn holds_of(repo: &Path, branches: &[String]) -> Result<Holds> {
    let main = repo.canonicalize()?;
    let mut holds = Holds {
        worktrees: Vec::new(),
        main: Vec::new(),
    };
    for name in branches {
        for worktree in worktrees(repo)? {
            if git(&worktree, &["rev-parse", "--abbrev-ref", "HEAD"])?.trim() != name {
                continue;
            }
            let is_main = worktree.canonicalize()? == main;
            // Every holding checkout is dirty-checked — the main checkout
            // included, since `prepare` leaves it on the requested branch:
            // a dirty holder of any kind refuses the whole discard.
            if !git(&worktree, &["status", "--porcelain=v1", "--untracked-files=all"])?.is_empty() {
                bail!(
                    "{} {} on discarded branch {} has uncommitted changes; resolve them by hand first",
                    if is_main { "main checkout" } else { "worktree" },
                    worktree.display(),
                    name
                );
            }
            if is_main {
                holds.main.push(name.clone());
            } else {
                holds.worktrees.push((name.clone(), worktree));
            }
        }
    }
    Ok(holds)
}

fn discard_branch(
    repo: &Path,
    holds: &Holds,
    branch: &str,
    main_target: Option<&str>,
) -> Result<()> {
    for (name, worktree) in &holds.worktrees {
        if name == branch {
            git(repo, &["worktree", "remove", &worktree.display().to_string()])?;
        }
    }
    // Move the checkout off the branch without touching any branch ref:
    // detach it where preparation itself would (the base on origin, or a
    // bare base revision, both resolved before any mutation). Detaching at
    // HEAD would leave the discarded commits stranded and refused all over
    // again.
    if let Some(target) = main_target.filter(|_| holds.main.iter().any(|name| name == branch)) {
        git(repo, &["switch", "--detach", target])?;
    }
    git(repo, &["branch", "-D", branch])?;
    Ok(())
}

/// The detached target a discard moves a holding main checkout to: the base
/// on `origin`, or a bare base revision. Resolved once, before the first
/// mutation, so an unresolvable base refuses the whole discard instead of
/// landing mid-way through a multi-branch scope.
fn discard_target(repo: &Path, base: &str) -> Result<String> {
    let target = format!("refs/remotes/origin/{base}");
    if git(repo, &["rev-parse", "--verify", &target]).is_ok() {
        Ok(target)
    } else if base.len() == 40 && base.chars().all(|c| c.is_ascii_hexdigit()) {
        Ok(base.to_owned())
    } else {
        bail!(
            "cannot move the checkout off a discarded branch: base {base} is not on origin; name the work with a branch by hand"
        )
    }
}

/// Paths of an additional repository: `(clone, identity record, staging)`.
/// Refuses unless the account's primary workspace was established first and
/// every directory on the way is a real directory of this volume.
fn additional_paths(
    root: &Path,
    slug: &str,
    account: &str,
) -> Result<(PathBuf, PathBuf, Option<PathBuf>)> {
    if !safe_segment(slug) || slug.contains("..") {
        bail!("additional repository directory name is unsafe");
    }
    let primary: Identity = serde_json::from_slice(
        &std::fs::read(root.join("identity.json"))
            .context("the account's primary workspace is not prepared; start the session first")?,
    )?;
    if primary.protocol != PROTOCOL || primary.account != account {
        bail!("private workspace account identity mismatch; do not reset or remove unknown work");
    }
    let work = root.join("work");
    let identities = work.join(".identity");
    for dir in [&work, &identities] {
        match std::fs::symlink_metadata(dir) {
            Ok(m) if m.is_dir() => {}
            Ok(_) => bail!("workspace {} must be a real directory, not a link", dir.display()),
            Err(_) => std::fs::create_dir(dir)?,
        }
    }
    Ok((
        work.join(slug),
        identities.join(format!("{slug}.json")),
        Some(work.join(format!(".cloning-{slug}"))),
    ))
}

fn discard_staging(path: &Path) -> Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(m) if m.is_dir() => std::fs::remove_dir_all(path)?,
        Ok(_) => std::fs::remove_file(path)?,
        Err(_) => {}
    }
    Ok(())
}

/// Forge identity (`host/owner/name`, lowercase) of a roster remote in any of
/// its spellings: `https://h/o/n(.git)`, `ssh://git@h/o/n`, `git@h:o/n(.git)`.
fn remote_key(remote: &str) -> Option<String> {
    let remote = remote.trim();
    let (host, path) = if let Some((_, rest)) = remote.split_once("://") {
        let rest = rest.rsplit_once('@').map_or(rest, |(_, r)| r);
        rest.split_once('/')?
    } else {
        let (host, path) = remote
            .rsplit_once('@')
            .map_or(remote, |(_, r)| r)
            .split_once(':')?;
        (host, path)
    };
    let path = path.trim_matches('/');
    let path = path.strip_suffix(".git").unwrap_or(path);
    Some(format!("{}/{}", host.split(':').next()?, path).to_ascii_lowercase())
}

/// Whether `id` is a repository the fleet roster marks `firewall: true`
/// (notebook/corporate). Matches the remote's forge identity, and — failing
/// closed for a record with no usable remote — the record's own name against
/// the repository name. `records` are `(name, remote)` pairs.
pub(super) fn firewalled(id: &RepoId, records: &[(String, Option<String>)]) -> bool {
    let (authority, rest) = id.key.split_once('/').unwrap_or((id.key.as_str(), ""));
    let without_port = format!("{}/{rest}", authority.split(':').next().unwrap_or(authority));
    let name = id.key.rsplit('/').next().unwrap_or("");
    records
        .iter()
        .any(|(record, remote)| match remote.as_deref().and_then(remote_key) {
            Some(key) => key == without_port,
            None => record.to_ascii_lowercase() == name,
        })
}

fn worktrees(repo: &Path) -> Result<Vec<PathBuf>> {
    Ok(worktree_entries(repo)?
        .into_iter()
        .map(|entry| entry.path)
        .collect())
}

struct WorktreeEntry {
    path: PathBuf,
    locked: bool,
}

fn worktree_entries(repo: &Path) -> Result<Vec<WorktreeEntry>> {
    Ok(git(repo, &["worktree", "list", "--porcelain", "-z"])?
        .split("\0\0")
        .filter_map(|record| {
            let mut fields = record.split('\0');
            let path = PathBuf::from(fields.next()?.strip_prefix("worktree ")?);
            let locked = fields.any(|field| field == "locked" || field.starts_with("locked "));
            Some(WorktreeEntry { path, locked })
        })
        .collect())
}

/// The dirty refusal names at most this many paths, so an operator can tell
/// stranded work from residue without exhausting the message (#14160).
const DIRTY_PATH_LIMIT: usize = 10;

/// Ignore only untracked Loom runtime residue (#14160) and tool caches
/// (#14458). Porcelain v1 C-quotes special path bytes, including newlines,
/// keeping each changed path on a single line. Both prefixes are literal in
/// a quoted path, so status of any tracked edit is preserved verbatim.
fn worktree_status(path: &Path) -> Result<Vec<String>> {
    let status = git(
        path,
        &[
            "-c",
            "core.quotePath=true",
            "status",
            "--porcelain=v1",
            "--untracked-files=all",
        ],
    )?;
    Ok(status
        .lines()
        .filter(|line| {
            !line.strip_prefix("?? ").is_some_and(|path| {
                let path = path.strip_prefix('"').unwrap_or(path);
                path.starts_with(".loom/lease-renew/") || path.starts_with(".cache/")
            })
        })
        .map(str::to_owned)
        .collect())
}

fn clean_worktrees(root: &Path, repo: &Path, account: &str) -> Result<()> {
    for WorktreeEntry { path, locked } in worktree_entries(repo)? {
        let canonical = path.canonicalize()?;
        if !canonical.starts_with(root) {
            bail!("worktree points outside the private volume; refuse reuse");
        }
        let status = worktree_status(&path)?;
        if status.is_empty() {
            continue;
        }
        let mut detail = status
            .iter()
            .take(DIRTY_PATH_LIMIT)
            .cloned()
            .collect::<Vec<_>>()
            .join("\n  ");
        if status.len() > DIRTY_PATH_LIMIT {
            detail.push_str(&format!("\n  … and {} more", status.len() - DIRTY_PATH_LIMIT));
        }
        let guidance = recovery_guidance(
            account,
            &path.display().to_string(),
            canonical != repo && !locked && ahead_of_origin(&path, "HEAD")? == 0,
        );
        let account = crate::shell_quote::shell_quote(account);
        bail!(
            "{SEAT_DIRTY_REASON}: private clone/worktree is dirty; retain and recover its work before reuse ({}):\n  {detail}\n\
             list every dirty worktree with `loom-daemon accounts session recover {account}`; {guidance}",
            path.display()
        );
    }
    Ok(())
}

/// Every worktree of the clone (the main checkout included) that
/// [`worktree_status`] calls dirty, with its counts (#14458).
fn dirty_worktrees(root: &Path, repo: &Path) -> Result<Vec<DirtyWorktree>> {
    let root = root.canonicalize()?;
    let main = repo.canonicalize()?;
    let mut dirty = Vec::new();
    for WorktreeEntry { path, locked } in worktree_entries(repo)? {
        let status = worktree_status(&path)?;
        if status.is_empty() {
            continue;
        }
        let untracked = status.iter().filter(|line| line.starts_with("?? ")).count();
        let branch = git(&path, &["rev-parse", "--abbrev-ref", "HEAD"])?
            .trim()
            .to_owned();
        let canonical = path.canonicalize()?;
        let unpublished = ahead_of_origin(&path, "HEAD")?;
        dirty.push(DirtyWorktree {
            path: path.display().to_string(),
            branch: (branch != "HEAD").then_some(branch),
            tracked: status.len() - untracked,
            untracked,
            unpublished,
            discardable: canonical != main
                && canonical.starts_with(&root)
                && !locked
                && unpublished == 0,
        });
    }
    Ok(dirty)
}

/// Resolve the entire requested scope before removal. Unknown paths may be
/// owned by a sibling clone; the host must account for every one before it
/// asks any clone to remove its own. Aliases target one canonical worktree.
fn named_worktrees(
    root: &Path,
    repo: &Path,
    named: &[PathBuf],
    ignored: &mut Vec<String>,
) -> Result<Vec<PathBuf>> {
    let root = root.canonicalize()?;
    let main = repo.canonicalize()?;
    let listed: Vec<_> = worktree_entries(repo)?
        .into_iter()
        .filter_map(|entry| Some((entry.path.canonicalize().ok()?, entry)))
        .collect();
    let mut seen = std::collections::HashSet::new();
    let mut targets = Vec::new();
    for want in named {
        let wanted = want.canonicalize().ok();
        let Some((canonical, entry)) = listed
            .iter()
            .find(|(path, _)| Some(path) == wanted.as_ref())
        else {
            ignored.push(want.display().to_string());
            continue;
        };
        if !seen.insert(canonical) {
            continue;
        }
        let path = &entry.path;
        if *canonical == main {
            bail!(
                "{} is the clone's main checkout; save or resolve its changes in place",
                path.display()
            );
        }
        if entry.locked {
            bail!("worktree {} is locked; save or resolve its changes in place", path.display());
        }
        if !canonical.starts_with(&root) {
            bail!("worktree points outside the private volume; refuse removal");
        }
        let unpublished = ahead_of_origin(path, "HEAD")?;
        if unpublished > 0 {
            bail!(
                "worktree {} carries {unpublished} unpublished commit(s); save or publish them before removing the worktree",
                path.display()
            );
        }
        targets.push(path.clone());
    }
    Ok(targets)
}

fn finished_pr(repo: &Path, repository: &str, branch: &str, tip: &str) -> bool {
    let host = std::env::var("GH_HOST").unwrap_or_else(|_| "github.com".into());
    let Some(slug) = super::forge_auth::github_slug(repository, &host) else {
        return false;
    };
    crate::worktree_ops::clean_owner::finished_pr_at_head(repo, &host, &slug, branch, tip)
}

/// Reclaim only an in-volume linked checkout whose tip is published and
/// whose own repository and branch have a finished PR at that exact tip.
/// Keep the branch ref and every commit. A missing, open or unreadable PR
/// leaves the checkout for the usual recovery flow, even if Git ancestry
/// alone would make its tip look merged.
fn reap_finished_worktrees(
    root: &Path,
    repo: &Path,
    repository: &str,
    has_finished_pr: &dyn Fn(&Path, &str, &str, &str) -> bool,
) -> Result<Vec<String>> {
    let mut reaped = Vec::new();
    for WorktreeEntry { path, locked } in worktree_entries(repo)? {
        let canonical = path.canonicalize()?;
        if locked || canonical == repo || !canonical.starts_with(root) {
            continue;
        }
        let branch = git(&path, &["rev-parse", "--abbrev-ref", "HEAD"])?;
        let branch = branch.trim();
        if branch == "HEAD" || ahead_of_origin(&path, "HEAD")? > 0 {
            continue;
        }
        let tip = git(&path, &["rev-parse", "HEAD"])?;
        if has_finished_pr(repo, repository, branch, tip.trim()) {
            git(repo, &["worktree", "remove", "--force", &path.display().to_string()])?;
            reaped.push(path.display().to_string());
        }
    }
    Ok(reaped)
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
    use crate::gh_invocation::{AccessIntent, GhCompletion, GhInvocation, GhTarget, Operation};
    use crate::proc_exec::ExecError;
    // Through the one `gh` choke point (#9985): stdin is the request, stdout
    // git's answer, stderr discarded — never captured.
    let helper = GhInvocation::new(
        Operation::new("auth.git_credential"),
        AccessIntent::Read,
        GhTarget::None,
        std::time::Duration::ZERO,
    )
    .args(["auth", "git-credential", "get"])
    .credential_helper(request)
    .execute();
    match helper {
        Ok(GhCompletion::Passthrough(status)) if status.success() => Ok(()),
        Err(ExecError::Spawn(e)) => Err(anyhow::Error::new(e).context(
            "forge authentication unavailable; provide an external gh profile or supported token environment",
        )),
        Err(ExecError::Collect(e)) => Err(e.into()),
        Ok(_) => bail!("forge authentication helper failed"),
    }
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

#[cfg(test)]
mod helper_cause_tests {
    use super::*;

    /// #14433: only this binary's own helper diagnostics are surfaced from
    /// Git's stderr; anything else (URLs, a secret a helper printed) is not.
    #[test]
    fn only_the_credential_helpers_own_diagnostics_name_a_push_cause() {
        let stderr = b"Error: forge authentication helper failed\nfatal: could not read Username for 'https://github.com'\n";
        assert_eq!(helper_cause(stderr), Some("forge authentication helper failed"));
        let missing = b"Error: forge authentication unavailable; provide an external gh profile";
        assert_eq!(helper_cause(missing), Some("forge authentication unavailable"));
        assert_eq!(
            helper_cause(b"fatal: unable to access 'https://x:ghs_SECRET@github.com/a/b/'"),
            None
        );
    }
}
