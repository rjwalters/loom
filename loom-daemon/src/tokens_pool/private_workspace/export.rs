//! Bounded read-only control channel. Worker data never chooses a host path.
use super::*;
use crate::worktree_root::worktree_root;
use serde_json::Value;
use std::io::Read;
use std::process::Command;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Record {
    pub runtime: String,
    pub provider: String,
    pub account: String,
    pub repository: String,
    pub logical_root: PathBuf,
    pub owner: String,
    pub issue: Option<u64>,
    pub container_id: String,
    pub volume: String,
    pub outcome: String,
    pub snapshot: Option<Snapshot>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Snapshot {
    pub issue: Option<u64>,
    pub branch: Option<String>,
    pub revision: Option<String>,
    pub published: bool,
    pub dirty: bool,
    pub checkpoint: Option<Checkpoint>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Checkpoint {
    pub phase: String,
    pub task_id: String,
    pub timestamp: String,
    pub model: Option<String>,
    pub pr_number: Option<u64>,
    pub attempt: Option<u64>,
}

fn record_path(root: &Path, issue: Option<u64>, owner: &str) -> Result<PathBuf> {
    let key = issue.map(|n| format!("issue-{n}")).unwrap_or_else(|| {
        // The job owner is host-generated, but avoid using even that as a path.
        {
            use sha2::{Digest, Sha256};
            format!("role-{}", hex::encode(Sha256::digest(owner.as_bytes())))
        }
    });
    let dir = root.join(".loom/private-jobs");
    safe_parent(root, &dir)?;
    Ok(dir.join(format!("{key}.json")))
}

/// Rejects a traversal or a symlink among the path components *below* `root`.
///
/// Those components are the only ones worker-side data can create, and they are
/// what this hardening defends: a worker that plants `.loom -> /somewhere/host`
/// inside its clone must not be able to redirect a host-side control write out
/// of that clone.
///
/// The prefix up to and including `root` is host-chosen and is trusted. It must
/// be: on macOS a default `TMPDIR` sits under `/var/folders/...`, and both
/// `/var -> /private/var` and `/tmp -> /private/tmp` are platform symlinks no
/// worker could have planted. Walking *all* ancestors therefore rejected every
/// private-workspace call on a macOS host (#8870), which failed the build gate
/// for every builder on every issue. Scoping the walk to `root`'s descendants
/// restores that host without granting a blanket symlink allowance: any symlink
/// component the worker can actually reach is still refused.
fn safe_parent(root: &Path, path: &Path) -> Result<()> {
    let Ok(relative) = path.strip_prefix(root) else {
        bail!("private export destination escapes its root");
    };
    // `strip_prefix` is lexical, so a `..` component could otherwise climb back
    // out of a path that still starts with `root`.
    if relative
        .components()
        .any(|c| !matches!(c, std::path::Component::Normal(_)))
    {
        bail!("private export destination contains a traversal component");
    }
    let mut cursor = root.to_path_buf();
    for component in relative.components() {
        cursor.push(component);
        if std::fs::symlink_metadata(&cursor).is_ok_and(|m| m.file_type().is_symlink()) {
            bail!("private export destination contains a symlink");
        }
    }
    Ok(())
}

pub fn has_issue(root: &Path, issue: u32) -> bool {
    root.join(format!(".loom/private-jobs/issue-{issue}.json"))
        .exists()
}

pub(super) fn check_runtime(root: &Path, issue: Option<u64>, runtime: &str) -> Result<()> {
    if runtime != "codex" {
        check_failover(root, issue, "non-codex-runtime")?;
    }
    Ok(())
}

pub(super) fn check_failover(root: &Path, issue: Option<u64>, account: &str) -> Result<()> {
    let Some(issue) = issue else { return Ok(()) };
    let path = record_path(root, Some(issue), "")?;
    if path.exists() {
        let record: Record = serde_json::from_slice(&read_small(root, &path)?)?;
        if record.account != account {
            bail!("private job requires recovery on account {}; automatic account/runtime failover cannot transfer its account-owned checkpoint", record.account);
        }
    }
    Ok(())
}

pub(super) struct PreparedRecord {
    /// The trusted host root this record lives under; `safe_parent` is scoped
    /// to it so a worker-planted symlink still cannot redirect a rollback.
    root: PathBuf,
    path: PathBuf,
    previous: Option<Vec<u8>>,
    owner: std::cell::RefCell<String>,
}
impl PreparedRecord {
    pub(super) fn rebind(&self, owner: &str) -> Result<()> {
        let mut record: Record = serde_json::from_slice(&read_small(&self.root, &self.path)?)?;
        if record.owner != *self.owner.borrow() || record.outcome != "prepared" {
            bail!("prepared export owner changed");
        }
        record.owner = owner.into();
        save(&self.path, &record)?;
        *self.owner.borrow_mut() = owner.into();
        Ok(())
    }
    pub(super) fn rollback(&self) -> Result<()> {
        let record: Record = serde_json::from_slice(&read_small(&self.root, &self.path)?)?;
        if record.owner != *self.owner.borrow() || record.outcome != "prepared" {
            return Ok(());
        }
        if let Some(previous) = &self.previous {
            let record: Record = serde_json::from_slice(previous)?;
            save(&self.path, &record)?;
        } else {
            std::fs::remove_file(&self.path)?;
        }
        Ok(())
    }
}

pub(super) fn record_prepared(
    root: &Path,
    config: &Config,
    job: &lease::Job,
) -> Result<PreparedRecord> {
    let prior_path = record_path(root, job.issue, &job.owner)?;
    std::fs::create_dir_all(prior_path.parent().unwrap())?;
    let previous = if prior_path.exists() {
        Some(read_small(root, &prior_path)?)
    } else {
        None
    };
    let prior_snapshot = previous
        .as_ref()
        .and_then(|bytes| serde_json::from_slice::<Record>(bytes).ok())
        .and_then(|r| r.snapshot);
    let record = Record {
        runtime: "codex".into(),
        provider: "codex".into(),
        account: config.account.clone(),
        // The clone this job acts on (#10796): an additional clone's recovery
        // record must name it, not the account's primary.
        repository: if job.repository.is_empty() {
            config.repository.clone()
        } else {
            job.repository.clone()
        },
        logical_root: root.canonicalize()?,
        owner: job.owner.clone(),
        issue: job.issue,
        container_id: job.container_id.clone(),
        volume: config.volume.clone(),
        outcome: "prepared".into(),
        snapshot: prior_snapshot,
    };
    save(&prior_path, &record)?;
    Ok(PreparedRecord {
        root: root.to_path_buf(),
        path: prior_path,
        previous,
        owner: std::cell::RefCell::new(job.owner.clone()),
    })
}

pub(super) fn update(root: &Path, config: &Config, job: &lease::Job, outcome: &str) -> Result<()> {
    let path = record_path(root, job.issue, &job.owner)?;
    let mut record: Record = serde_json::from_slice(&read_small(root, &path)?)?;
    if record.account != config.account
        || record.container_id != job.container_id
        || record.owner != job.owner
    {
        bail!("private export owner changed; refusing to overwrite another job");
    }
    record.outcome = outcome.into();
    let mut fresh = false;
    let command = snapshot_command(config, job)?;
    if let Ok(crate::proc_exec::Completion::Exited(output)) =
        crate::proc_exec::run_bounded(command, std::time::Duration::from_secs(2))
    {
        if output.status.success() && output.stdout.len() <= 65536 {
            let snapshot: Snapshot = serde_json::from_slice(&output.stdout)?;
            validate(&snapshot, job.issue)?;
            if let (Some(issue), Some(checkpoint)) = (job.issue, &snapshot.checkpoint) {
                let dir = root.join(".loom/sweep-checkpoint");
                safe_parent(root, &dir)?;
                std::fs::create_dir_all(&dir)?;
                let destination = dir.join(format!("issue-{issue}.json"));
                let previous = read_small(root, &destination)
                    .ok()
                    .and_then(|bytes| serde_json::from_slice::<Checkpoint>(&bytes).ok());
                if previous.as_ref() != Some(checkpoint) {
                    save(&destination, checkpoint)?;
                    let timestamp = chrono::DateTime::parse_from_rfc3339(&checkpoint.timestamp)?;
                    let file = std::fs::OpenOptions::new().write(true).open(&destination)?;
                    file.set_times(
                        std::fs::FileTimes::new()
                            .set_modified(std::time::SystemTime::from(timestamp)),
                    )?;
                }
            }
            record.snapshot = Some(snapshot);
            fresh = true;
        }
    }
    // A previous run's snapshot cannot prove this run finished safely, and a
    // fresh one only certifies completion for an issue job when the issue
    // branch is published and its worktree clean: work the step left behind —
    // unpushed commits, an unclean tree, a worktree that never came to exist —
    // needs a recovery pass even though the step exited cleanly (#14031).
    if outcome == "completed" && !completed_certified(fresh, record.snapshot.as_ref(), job.issue) {
        record.outcome = "recovery-required".into();
    }
    // Failed read preserves the last snapshot and records uncertainty.
    save(&path, &record)
}

/// Whether this run's own snapshot certifies a `completed` outcome. A fresh
/// snapshot of an issue-less (role) job certifies as before; an issue job's
/// must additionally show the issue branch published and the worktree clean.
fn completed_certified(fresh: bool, snapshot: Option<&Snapshot>, issue: Option<u64>) -> bool {
    if !fresh {
        return false;
    }
    snapshot.is_some_and(|s| issue.is_none() || (s.published && !s.dirty))
}

/// The in-container snapshot of the clone `job` ran in. The exec must name that
/// clone itself (#10796): the session container's own environment knows only
/// the account's primary, so a bare `docker exec … snapshot` would describe
/// `/workspace/repo` for a job that ran in an additional clone — and a clean,
/// published primary would then certify an additional clone's unfinished work
/// as safe. A job whose repository this account no longer registers gets no
/// snapshot at all rather than the wrong one.
fn snapshot_command(config: &Config, job: &lease::Job) -> Result<Command> {
    let target = config
        .target((!job.repository.is_empty()).then_some(job.repository.as_str()))
        .context(containment::WRONG_CLONE)?;
    let mut command = Command::new("docker");
    command.args([
        "exec",
        "--env",
        &format!("LOOM_PRIVATE_CLONE={}", target.path),
        // The step's worktrees live under the override its launch env carried
        // (transport.rs `child_env` sets LOOM_WORKTREE_ROOT to this value);
        // the session container itself is created without it (docker.rs
        // `create`), so the snapshot exec must name it too or the worktree
        // base resolves without the clone-basename namespace and the snapshot
        // misses the worktree (#14031).
        "--env",
        &format!("LOOM_WORKTREE_ROOT={}/.loom/worktrees", target.path),
        &job.container_id,
        "loom-daemon",
        "private-workspace",
        "snapshot",
    ]);
    if let Some(issue) = job.issue {
        command.args(["--issue", &issue.to_string()]);
    }
    Ok(command)
}

fn validate(snapshot: &Snapshot, issue: Option<u64>) -> Result<()> {
    if snapshot.issue != issue
        || snapshot
            .branch
            .as_ref()
            .is_some_and(|b| Some(b.clone()) != issue.map(|n| format!("feature/issue-{n}")))
    {
        bail!("private export issue/branch does not match its dispatch");
    }
    if snapshot
        .revision
        .as_ref()
        .is_some_and(|r| r.len() != 40 || !r.bytes().all(|b| b.is_ascii_hexdigit()))
    {
        bail!("invalid exported revision");
    }
    if let Some(c) = &snapshot.checkpoint {
        if c.task_id.len() > 256
            || c.task_id.chars().any(char::is_control)
            || chrono::DateTime::parse_from_rfc3339(&c.timestamp).is_err()
            || c.model.as_ref().is_some_and(|m| {
                m.len() > 128
                    || !m
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
            })
        {
            bail!("invalid checkpoint identity");
        }
        if ![
            "curator-done",
            "builder-done",
            "judge-rejected",
            "judge-done",
            "doctor-done",
            "merge-done",
        ]
        .contains(&c.phase.as_str())
            || c.pr_number == Some(0)
            || c.attempt.is_some_and(|n| n > 10000)
        {
            bail!("invalid exported checkpoint");
        }
    }
    Ok(())
}

/// Reads a bounded control record. `root` is the trusted host directory the
/// record must live under (see [`safe_parent`]); `O_NOFOLLOW` additionally
/// refuses the final component even if it raced into a symlink after the walk.
fn read_small(root: &Path, path: &Path) -> Result<Vec<u8>> {
    use std::os::unix::fs::OpenOptionsExt;
    safe_parent(root, path)?;
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    if !file.metadata()?.is_file() {
        bail!("private control record must be a regular file");
    }
    let mut bytes = Vec::new();
    file.take(65537).read_to_end(&mut bytes)?;
    if bytes.len() > 65536 {
        bail!("private control record exceeds 64 KiB");
    }
    Ok(bytes)
}

pub(super) fn snapshot(issue: Option<u64>) -> Result<Snapshot> {
    inspect(&worker_setup::selected_clone()?, issue)
}

/// The recovery state of the clone at `repo` — of its issue worktree when the
/// job had one. Split out of [`snapshot`] (which only resolves the selected
/// clone from the launch environment) so tests can drive a fixture clone.
fn inspect(repo: &Path, issue: Option<u64>) -> Result<Snapshot> {
    let branch = issue.map(|n| format!("feature/issue-{n}"));
    // The step created its worktree through worktree-root resolution: the
    // transport exports LOOM_WORKTREE_ROOT=<clone>/.loom/worktrees and the
    // helper namespaces that base by the clone's basename, so the worktree
    // lives at <clone>/.loom/worktrees/<basename>/issue-N. A literal
    // .loom/worktrees/issue-N never exists there, and the old fallback to the
    // root clone recorded the detached base revision — clean, unpublished and
    // indistinguishable between a merged job and a wedged one (#14031).
    let worktree = issue.map(|n| worktree_root(repo).join(format!("issue-{n}")));
    let inspected = match &worktree {
        // A role job with no issue describes the root clone itself.
        None => Some(repo.canonicalize()?),
        Some(path) if path.exists() => Some(path.canonicalize()?),
        // An issue job whose worktree never came to exist has no state that
        // can be attested published or clean. Report unknown rather than
        // silently describing the detached root clone's base revision.
        Some(_) => None,
    };
    if let Some(canonical) = &inspected {
        if !canonical.starts_with(repo) {
            bail!("private worktree escaped its clone");
        }
    }
    let git = |dir: &Path, args: &[&str]| -> Result<String> {
        let output = Command::new("git").arg("-C").arg(dir).args(args).output()?;
        if !output.status.success() {
            bail!("private snapshot git query failed");
        }
        Ok(String::from_utf8(output.stdout)?.trim().to_owned())
    };
    let (revision, published, dirty) = match &inspected {
        Some(dir) => {
            let actual_branch = git(dir, &["branch", "--show-current"])?;
            let revision = git(dir, &["rev-parse", "HEAD"])?;
            let published = branch
                .as_ref()
                .is_some_and(|expected| expected == &actual_branch)
                && branch.as_ref().is_some_and(|expected| {
                    git(dir, &["rev-parse", &format!("refs/remotes/origin/{expected}")])
                        .is_ok_and(|remote| remote == revision)
                });
            let dirty = !git(dir, &["status", "--porcelain", "--untracked-files=all"])?.is_empty();
            (Some(revision), published, dirty)
        }
        // Unknown, encoded so it cannot read as clean: nothing certifies it
        // published, and the missing worktree itself needs a recovery look.
        None => (None, false, true),
    };
    let checkpoint = issue
        .map(|n| repo.join(format!(".loom/sweep-checkpoint/issue-{n}.json")))
        .filter(|p| p.exists())
        .map(|p| -> Result<Checkpoint> {
            let value: Value = serde_json::from_slice(&read_small(repo, &p)?)?;
            Ok(Checkpoint {
                task_id: value["task_id"].as_str().unwrap_or_default().into(),
                timestamp: value["timestamp"]
                    .as_str()
                    .context("checkpoint timestamp missing")?
                    .into(),
                model: value["model"].as_str().map(str::to_owned),
                phase: value["phase"]
                    .as_str()
                    .context("checkpoint phase missing")?
                    .into(),
                pr_number: value["pr_number"].as_u64(),
                attempt: value["attempt"].as_u64(),
            })
        })
        .transpose()?;
    let result = Snapshot {
        issue,
        branch,
        revision,
        published,
        dirty,
        checkpoint,
    };
    validate(&result, issue)?;
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_git_helpers::{configure, run};
    use serial_test::serial;

    /// The snapshot exec names the clone the job ran in (#10796): an
    /// additional clone's job is snapshotted there, never at the primary the
    /// container's own environment defaults to, and a job on a repository the
    /// account does not register gets no snapshot at all.
    #[test]
    fn snapshot_exec_names_the_selected_clone() {
        let mut config = Config {
            schema_version: 1,
            account: "seat".into(),
            container: "c".into(),
            engine: "e".into(),
            docker_desktop: false,
            repository: "https://example.invalid/o/primary".into(),
            base: "main".into(),
            repositories: vec![],
            volume: "v".into(),
            profile: PathBuf::from("/nonexistent"),
            gh_config: None,
        };
        let second = identify("https://example.invalid/o/second").unwrap();
        config.repositories.push(Registered {
            repository: second.url.clone(),
            base: "main".into(),
            slug: second.slug.clone(),
        });
        let job = |repository: &str| lease::Job {
            owner: "fixture".into(),
            kind: JobKind::Sweep,
            issue: Some(7),
            branch: None,
            container_id: "a".repeat(64),
            base_revision: "b".repeat(40),
            host_pid: 1,
            control: "c".repeat(64),
            repository: repository.into(),
        };
        let argv = |command: &Command| -> Vec<String> {
            command
                .get_args()
                .map(|arg| arg.to_string_lossy().into_owned())
                .collect()
        };

        let primary = argv(&snapshot_command(&config, &job("")).unwrap());
        assert!(primary.contains(&format!("LOOM_PRIVATE_CLONE={REPO}")), "{primary:?}");
        // The snapshot exec also carries the worktree root the step's launch
        // env set — the container itself is created without it — so the
        // in-container worktree base resolves with the clone-basename
        // namespace (#14031).
        assert!(
            primary.contains(&format!("LOOM_WORKTREE_ROOT={REPO}/.loom/worktrees")),
            "{primary:?}"
        );

        let other = argv(&snapshot_command(&config, &job(&second.url)).unwrap());
        let clone = format!("{WORK_ROOT}/{}", second.slug);
        assert!(other.contains(&format!("LOOM_PRIVATE_CLONE={clone}")), "{other:?}");
        assert!(
            other.contains(&format!("LOOM_WORKTREE_ROOT={clone}/.loom/worktrees")),
            "{other:?}"
        );
        assert!(!other.iter().any(|arg| arg.contains(REPO)), "{other:?}");
        // `docker exec [OPTIONS] CONTAINER COMMAND`: the env option must precede
        // the container, and the snapshot still carries the job's issue.
        let env_at = other.iter().position(|arg| arg == "--env").unwrap();
        let container_at = other.iter().position(|arg| *arg == "a".repeat(64)).unwrap();
        assert!(env_at < container_at, "{other:?}");
        assert_eq!(
            other[container_at + 1..],
            [
                "loom-daemon",
                "private-workspace",
                "snapshot",
                "--issue",
                "7"
            ]
        );

        assert!(snapshot_command(&config, &job("https://example.invalid/o/third")).is_err());
    }

    /// A private-clone-shaped fixture: a detached root clone at `host/repo`
    /// with a bare `origin` remote, mirroring the layout the in-container
    /// snapshot runs against.
    fn clone_with_origin(host: &Path) -> PathBuf {
        let clone = host.join("repo");
        std::fs::create_dir_all(&clone).unwrap();
        run(&clone, &["init", "-b", "main"]);
        configure(&clone);
        std::fs::write(clone.join("base"), "base\n").unwrap();
        run(&clone, &["add", "-A"]);
        run(&clone, &["commit", "-q", "-m", "base"]);
        let origin = host.join("origin.git");
        run(host, &["init", "--bare", "-b", "main", origin.to_str().unwrap()]);
        run(&clone, &["remote", "add", "origin", origin.to_str().unwrap()]);
        clone
    }

    /// Run `f` with `LOOM_WORKTREE_ROOT` set the way the private transport
    /// sets it for the step (`<clone>/.loom/worktrees`), restoring the prior
    /// value. Every caller carries `#[serial]` (the crate-default group,
    /// shared with `worktree_root`'s and `worktree_reaper`'s env tests) so the
    /// process-global mutation cannot race them (#5164).
    fn with_step_worktree_root<T>(clone: &Path, f: impl FnOnce() -> T) -> T {
        let prev = std::env::var("LOOM_WORKTREE_ROOT").ok();
        std::env::set_var("LOOM_WORKTREE_ROOT", clone.join(".loom/worktrees").to_str().unwrap());
        let result = f();
        match prev {
            Some(p) => std::env::set_var("LOOM_WORKTREE_ROOT", p),
            None => std::env::remove_var("LOOM_WORKTREE_ROOT"),
        }
        result
    }

    /// The snapshot must read the issue worktree the step used — under the
    /// basename-namespaced base its `LOOM_WORKTREE_ROOT` resolves to — and
    /// distinguish a job that pushed from one that left an unpushed commit
    /// behind (#14031).
    #[test]
    #[serial]
    fn issue_snapshot_reads_the_real_worktree_and_distinguishes_published() {
        let host = tempfile::tempdir().unwrap();
        let clone = clone_with_origin(host.path());
        // The base the step's worktree.sh resolves: LOOM_WORKTREE_ROOT
        // namespaced by the clone's basename.
        let base = clone.join(".loom/worktrees/repo");
        std::fs::create_dir_all(&base).unwrap();
        let worktree = base.join("issue-7");
        run(
            &clone,
            &[
                "worktree",
                "add",
                "-b",
                "feature/issue-7",
                worktree.to_str().unwrap(),
                "main",
            ],
        );
        let base_revision = run(&clone, &["rev-parse", "main"]);
        std::fs::write(worktree.join("work"), "done\n").unwrap();
        run(&worktree, &["add", "-A"]);
        run(&worktree, &["commit", "-q", "-m", "work"]);
        let worktree_revision = run(&worktree, &["rev-parse", "HEAD"]);

        let unpushed = with_step_worktree_root(&clone, || inspect(&clone, Some(7)).unwrap());
        assert_eq!(
            unpushed.revision.as_deref(),
            Some(worktree_revision.as_str()),
            "the record must name the issue branch's revision"
        );
        assert_ne!(
            unpushed.revision.as_deref(),
            Some(base_revision.as_str()),
            "the record must not describe the detached root clone"
        );
        assert!(!unpushed.published);
        assert!(!unpushed.dirty);

        run(&clone, &["push", "origin", "feature/issue-7"]);
        let pushed = with_step_worktree_root(&clone, || inspect(&clone, Some(7)).unwrap());
        assert!(pushed.published, "a pushed and an unpushed job must differ in published");
        assert!(!pushed.dirty);
        assert_eq!(pushed.revision, unpushed.revision);
    }

    /// An issue job whose worktree is absent must not be described by the
    /// detached root clone's state: unknown, unpublished, not clean (#14031).
    #[test]
    #[serial]
    fn issue_snapshot_without_worktree_is_unknown_not_the_root_clone() {
        let host = tempfile::tempdir().unwrap();
        let clone = clone_with_origin(host.path());
        let snapshot = with_step_worktree_root(&clone, || inspect(&clone, Some(7)).unwrap());
        assert_eq!(snapshot.issue, Some(7));
        assert_eq!(snapshot.branch.as_deref(), Some("feature/issue-7"));
        assert_eq!(snapshot.revision, None, "a missing worktree has no revision to certify");
        assert!(!snapshot.published);
        assert!(snapshot.dirty);
    }

    /// A role job with no issue still describes the root clone itself.
    #[test]
    fn issue_less_snapshot_still_reads_the_root_clone() {
        let host = tempfile::tempdir().unwrap();
        let clone = clone_with_origin(host.path());
        let head = run(&clone, &["rev-parse", "HEAD"]);
        let snapshot = inspect(&clone, None).unwrap();
        assert_eq!(snapshot.issue, None);
        assert_eq!(snapshot.branch, None);
        assert_eq!(snapshot.revision.as_deref(), Some(head.as_str()));
        assert!(!snapshot.published);
        assert!(!snapshot.dirty);
    }

    #[test]
    fn export_rejects_foreign_issue_branch_and_checkpoint() {
        let mut s = Snapshot {
            issue: Some(7),
            branch: Some("feature/issue-7".into()),
            revision: Some("a".repeat(40)),
            published: false,
            dirty: true,
            checkpoint: None,
        };
        assert!(validate(&s, Some(8)).is_err());
        s.branch = Some("../../host".into());
        assert!(validate(&s, Some(7)).is_err());
        s.branch = None;
        s.checkpoint = Some(Checkpoint {
            task_id: "t".into(),
            timestamp: "2026-09-24T00:00:00Z".into(),
            model: None,
            phase: "/host/path".into(),
            pr_number: None,
            attempt: None,
        });
        assert!(validate(&s, Some(7)).is_err());
    }
    /// `completed` survives only a fresh snapshot that certifies an issue
    /// job's branch published and its worktree clean; everything else reads
    /// recovery-required (#14031).
    #[test]
    fn completion_requires_a_fresh_clean_published_issue_snapshot() {
        let snap = |published: bool, dirty: bool| Snapshot {
            issue: Some(7),
            branch: Some("feature/issue-7".into()),
            revision: Some("a".repeat(40)),
            published,
            dirty,
            checkpoint: None,
        };
        assert!(completed_certified(true, Some(&snap(true, false)), Some(7)));
        assert!(!completed_certified(false, Some(&snap(true, false)), Some(7)));
        assert!(!completed_certified(true, Some(&snap(false, false)), Some(7)));
        assert!(!completed_certified(true, Some(&snap(true, true)), Some(7)));
        assert!(!completed_certified(true, None, Some(7)));
        // A role job (no issue) keeps the fresh-snapshot-only contract.
        assert!(completed_certified(true, Some(&snap(false, true)), None));
        assert!(!completed_certified(false, Some(&snap(false, true)), None));
    }

    /// `update` stamps recovery-required when completion cannot be certified.
    /// The job's container is unreachable from a test, so the snapshot is not
    /// fresh — the pre-existing leg of the rule whose fresh leg now lives in
    /// `completed_certified` (#14031).
    #[test]
    fn update_downgrades_uncertified_completion() {
        let root = tempfile::tempdir().unwrap();
        let config = Config {
            schema_version: 1,
            account: "seat".into(),
            container: "c".into(),
            engine: "e".into(),
            docker_desktop: false,
            repository: "https://example.invalid/o/primary".into(),
            base: "main".into(),
            repositories: vec![],
            volume: "v".into(),
            profile: PathBuf::from("/nonexistent"),
            gh_config: None,
        };
        let job = lease::Job {
            owner: "fixture".into(),
            kind: JobKind::Sweep,
            issue: Some(7),
            branch: None,
            container_id: "a".repeat(64),
            base_revision: "b".repeat(40),
            host_pid: 1,
            control: "c".repeat(64),
            repository: String::new(),
        };
        let record = Record {
            runtime: "codex".into(),
            provider: "codex".into(),
            account: "seat".into(),
            repository: "https://example.invalid/o/primary".into(),
            logical_root: root.path().into(),
            owner: "fixture".into(),
            issue: Some(7),
            container_id: "a".repeat(64),
            volume: "v".into(),
            outcome: "prepared".into(),
            snapshot: None,
        };
        std::fs::create_dir_all(root.path().join(".loom/private-jobs")).unwrap();
        let path = record_path(root.path(), Some(7), "").unwrap();
        save(&path, &record).unwrap();
        update(root.path(), &config, &job, "completed").unwrap();
        let after: Record =
            serde_json::from_slice(&read_small(root.path(), &path).unwrap()).unwrap();
        assert_eq!(after.outcome, "recovery-required");
    }

    #[test]
    fn unfinished_record_blocks_account_failover_and_does_not_touch_host_worktree() {
        let root = tempfile::tempdir().unwrap();
        // The real layout: the worktree base is namespaced by the clone's
        // basename (LOOM_WORKTREE_ROOT + worktree-root resolution), not a bare
        // .loom/worktrees/issue-N (#14031).
        let worktree = root.path().join(format!(
            ".loom/worktrees/{}/issue-7",
            root.path().file_name().unwrap().to_string_lossy()
        ));
        std::fs::create_dir_all(&worktree).unwrap();
        std::fs::write(worktree.join("keep"), "host").unwrap();
        let record = Record {
            runtime: "codex".into(),
            provider: "codex".into(),
            account: "first".into(),
            repository: "https://example.invalid/repo".into(),
            logical_root: root.path().into(),
            owner: "sweep-7".into(),
            issue: Some(7),
            container_id: "a".repeat(64),
            volume: "private".into(),
            outcome: "prepared".into(),
            snapshot: None,
        };
        std::fs::create_dir_all(root.path().join(".loom/private-jobs")).unwrap();
        save(&record_path(root.path(), Some(7), "").unwrap(), &record).unwrap();
        assert!(check_failover(root.path(), Some(7), "second").is_err());
        assert!(check_runtime(root.path(), Some(7), "claude").is_err());
        assert!(check_runtime(root.path(), Some(7), "pi").is_err());
        assert!(check_failover(root.path(), Some(7), "first").is_ok());
        assert_eq!(std::fs::read_to_string(worktree.join("keep")).unwrap(), "host");
    }
    #[test]
    fn export_paths_reject_symlinks_and_oversized_records() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path(), root.path().join(".loom")).unwrap();
        assert!(record_path(root.path(), Some(7), "owner").is_err());
        std::fs::write(outside.path().join("large"), vec![b'x'; 65537]).unwrap();
        assert!(read_small(outside.path(), &outside.path().join("large")).is_err());
        std::fs::write(outside.path().join("keep"), "precious").unwrap();
        std::os::unix::fs::symlink(outside.path().join("keep"), root.path().join("link")).unwrap();
        assert!(read_small(root.path(), &root.path().join("link")).is_err());
        assert_eq!(std::fs::read_to_string(outside.path().join("keep")).unwrap(), "precious");
    }
    /// A symlinked *prefix* is the host's own business — macOS resolves a
    /// default `TMPDIR` through `/var -> /private/var`, and rejecting that
    /// failed every private-workspace call (and so the whole build gate) on
    /// every macOS host (#8870). Only components below the trusted root, which
    /// are the ones worker data can create, stay refused.
    #[test]
    fn platform_symlinked_root_prefix_is_allowed_but_worker_symlinks_are_not() {
        let base = tempfile::tempdir().unwrap();
        let real = base.path().join("private");
        std::fs::create_dir_all(real.join("clone")).unwrap();
        // Stand in for macOS's `/var -> /private/var`: reach the same clone
        // through a symlinked ancestor the worker never controls.
        std::os::unix::fs::symlink(&real, base.path().join("link")).unwrap();
        let root = base.path().join("link/clone");

        let path =
            record_path(&root, Some(7), "owner").expect("a symlinked host prefix is trusted");
        assert!(path.ends_with(".loom/private-jobs/issue-7.json"));
        assert!(check_failover(&root, Some(7), "any").is_ok());

        // The allowance must not extend below the root: the same clone, reached
        // through the same benign prefix, still refuses a planted symlink.
        let outside = base.path().join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, real.join("clone/.loom")).unwrap();
        assert!(record_path(&root, Some(7), "owner").is_err());
        std::fs::write(outside.join("secret"), "precious").unwrap();
        assert!(read_small(&root, &root.join(".loom/secret")).is_err());

        // A planted symlink deeper than the first component below the root is
        // refused too: the walk covers every descendant component, it does not
        // spot-check one level.
        std::fs::remove_file(real.join("clone/.loom")).unwrap();
        std::fs::create_dir_all(real.join("clone/.loom")).unwrap();
        std::os::unix::fs::symlink(&outside, real.join("clone/.loom/private-jobs")).unwrap();
        assert!(record_path(&root, Some(7), "owner").is_err());
        assert_eq!(std::fs::read_to_string(outside.join("secret")).unwrap(), "precious");
        assert!(!outside.join("issue-7.json").exists());
    }
    #[test]
    fn export_paths_reject_traversal_and_foreign_roots() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("clone")).unwrap();
        let clone = root.path().join("clone");
        assert!(safe_parent(&clone, &clone.join(".loom")).is_ok());
        // Lexically still under `clone`, but it climbs back out.
        assert!(safe_parent(&clone, &clone.join("../escape")).is_err());
        // Not under the trusted root at all: refused rather than trivially
        // accepted for having no components to walk.
        assert!(safe_parent(&clone, root.path()).is_err());
        assert!(safe_parent(&clone, Path::new("/etc/passwd")).is_err());
    }
    #[test]
    fn abandoned_preparation_rolls_back_only_its_matching_host_record() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("record.json");
        let record = Record {
            runtime: "codex".into(),
            provider: "codex".into(),
            account: "account".into(),
            repository: "https://example.invalid/repo".into(),
            logical_root: root.path().into(),
            owner: "prepared-owner".into(),
            issue: Some(7),
            container_id: "a".repeat(64),
            volume: "volume".into(),
            outcome: "prepared".into(),
            snapshot: None,
        };
        save(&path, &record).unwrap();
        let token = PreparedRecord {
            root: root.path().to_path_buf(),
            path: path.clone(),
            previous: None,
            owner: std::cell::RefCell::new(record.owner.clone()),
        };
        token.rollback().unwrap();
        assert!(!path.exists());
        let prior = Record {
            owner: "prior-owner".into(),
            outcome: "failed".into(),
            ..record.clone()
        };
        save(&path, &record).unwrap();
        let token = PreparedRecord {
            root: root.path().to_path_buf(),
            path: path.clone(),
            previous: Some(serde_json::to_vec(&prior).unwrap()),
            owner: std::cell::RefCell::new(record.owner.clone()),
        };
        token.rollback().unwrap();
        let restored: Record =
            serde_json::from_slice(&read_small(root.path(), &path).unwrap()).unwrap();
        assert_eq!(restored.owner, "prior-owner");
        assert_eq!(restored.outcome, "failed");
        // Another attempt replaced the prepared record; an older Drop cannot
        // erase its recovery association.
        token.rollback().unwrap();
        assert_eq!(
            serde_json::from_slice::<Record>(&read_small(root.path(), &path).unwrap())
                .unwrap()
                .owner,
            "prior-owner"
        );
    }
}
