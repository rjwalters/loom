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
    /// Collision-safe directory name of an additional repository (#10795).
    /// Absent: the legacy `<volume>/repo`.
    #[arg(long)]
    pub slug: Option<String>,
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
                    Ok(revision) => {
                        println!("{}", json!({"protocol": PROTOCOL, "revision": revision}))
                    }
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
    clean_worktrees(&root, &repo)?;
    git(&repo, &["fetch", "--prune", "origin"])?;
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
    }
}

/// List, publish or discard the clone's unpublished branches (#14030).
/// Publish only ever fast-forwards `origin`; discard requires an explicit
/// branch scope, deletes only branches it was named for, and never removes
/// the volume or moves a branch ref other than the ones being discarded.
pub(super) fn recover(root: &Path, args: &RecoverArgs) -> Result<RecoverOutcome> {
    let (repo, record) = recover_paths(root, args)?;
    if !repo.join(".git").is_dir() {
        return Ok(empty_outcome(&args.repository));
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
    };
    let mut scope = args.branch.clone();
    if let Some(issue) = args.issue {
        let name = format!("feature/issue-{issue}");
        git(&repo, &["check-ref-format", "--branch", &name])?;
        scope.push(name);
    }
    scope.sort();
    scope.dedup();
    if args.discard && scope.is_empty() {
        bail!("discard requires an explicit branch scope (--branch BRANCH or --issue N)");
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
    } else if args.discard {
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
    Ok(outcome)
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
    Ok(git(repo, &["worktree", "list", "--porcelain", "-z"])?
        .split('\0')
        .filter_map(|field| field.strip_prefix("worktree "))
        .map(PathBuf::from)
        .collect())
}

/// Loom's lease-renewal store (`Store::from_env`, `$HOME/.loom/lease-renew`
/// unless overridden) is written inside a seat clone when the container's
/// `$HOME` resolves to the clone root (#14160): an untracked
/// `.loom/lease-renew/.lock` there is runtime residue, not stranded work,
/// and must never refuse preparation. The seat env sets
/// `LOOM_LEASE_RENEW_STATE_DIR` so new writes land on the cache volume; the
/// exclude also forgives residue left by an older container.
const LEASE_RENEW_EXCLUDE: &str = ":(exclude).loom/lease-renew";

/// The dirty refusal names at most this many paths, so an operator can tell
/// stranded work from residue without exhausting the message (#14160).
const DIRTY_PATH_LIMIT: usize = 10;

fn clean_worktrees(root: &Path, repo: &Path) -> Result<()> {
    for path in worktrees(repo)? {
        if !path.canonicalize()?.starts_with(root) {
            bail!("worktree points outside the private volume; refuse reuse");
        }
        let status = git(
            &path,
            &[
                "status",
                "--porcelain=v1",
                "--untracked-files=all",
                "--",
                ".",
                LEASE_RENEW_EXCLUDE,
            ],
        )?;
        if status.is_empty() {
            continue;
        }
        let total = status.lines().count();
        let mut detail = status
            .lines()
            .take(DIRTY_PATH_LIMIT)
            .collect::<Vec<_>>()
            .join("\n  ");
        if total > DIRTY_PATH_LIMIT {
            detail.push_str(&format!("\n  … and {} more", total - DIRTY_PATH_LIMIT));
        }
        bail!(
            "private clone/worktree is dirty; retain and recover its work before reuse ({}):\n  {detail}",
            path.display()
        );
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
