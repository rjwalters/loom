//! Bounded read-only control channel. Worker data never chooses a host path.
use super::*;
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
        repository: config.repository.clone(),
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
    let mut command = Command::new("docker");
    command.args([
        "exec",
        &job.container_id,
        "loom-daemon",
        "private-workspace",
        "snapshot",
    ]);
    if let Some(issue) = job.issue {
        command.args(["--issue", &issue.to_string()]);
    }
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
    // A previous run's snapshot cannot prove this run finished safely.
    if outcome == "completed" && !fresh {
        record.outcome = "recovery-required".into();
    }
    // Failed read preserves the last snapshot and records uncertainty.
    save(&path, &record)
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
    let repo = Path::new(REPO);
    let branch = issue.map(|n| format!("feature/issue-{n}"));
    let worktree = issue
        .map(|n| repo.join(format!(".loom/worktrees/issue-{n}")))
        .filter(|p| p.exists())
        .unwrap_or_else(|| repo.to_owned());
    let canonical = worktree.canonicalize()?;
    if !canonical.starts_with(repo) {
        bail!("private worktree escaped its clone");
    }
    let git = |args: &[&str]| -> Result<String> {
        let output = Command::new("git")
            .arg("-C")
            .arg(&canonical)
            .args(args)
            .output()?;
        if !output.status.success() {
            bail!("private snapshot git query failed");
        }
        Ok(String::from_utf8(output.stdout)?.trim().to_owned())
    };
    let actual_branch = git(&["branch", "--show-current"])?;
    let revision = git(&["rev-parse", "HEAD"])?;
    let published = branch
        .as_ref()
        .is_some_and(|expected| expected == &actual_branch)
        && branch.as_ref().is_some_and(|expected| {
            git(&["rev-parse", &format!("refs/remotes/origin/{expected}")])
                .is_ok_and(|remote| remote == revision)
        });
    let dirty = !git(&["status", "--porcelain", "--untracked-files=all"])?.is_empty();
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
        revision: Some(revision),
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
    #[test]
    fn unfinished_record_blocks_account_failover_and_does_not_touch_host_worktree() {
        let root = tempfile::tempdir().unwrap();
        let worktree = root.path().join(".loom/worktrees/issue-7");
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
