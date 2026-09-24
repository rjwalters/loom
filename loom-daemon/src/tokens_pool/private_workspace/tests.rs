use super::*;

#[test]
fn forge_credential_username_preserves_basic_auth_and_rejects_protocol_injection() {
    use repository::credential_username;
    assert_eq!(credential_username(false, Some("fixture-user")).unwrap(), "fixture-user");
    assert_eq!(credential_username(false, None).unwrap(), "x-access-token");
    assert_eq!(credential_username(false, Some("")).unwrap(), "x-access-token");
    assert_eq!(credential_username(true, Some("unused-user")).unwrap(), "x-access-token");
    for invalid in [
        "bad:user",
        "bad\npassword=injected",
        "bad\ruser",
        "bad\0user",
    ] {
        assert!(credential_username(false, Some(invalid)).is_err());
    }
}
use std::process::Command;

fn git(path: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(path)
        .args([
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "Git fixture failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap().trim().to_owned()
}

struct RepoFixture {
    _temp: tempfile::TempDir,
    remote: PathBuf,
    root: PathBuf,
    args: repository::PrepareArgs,
}

impl RepoFixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let remote = temp.path().join("remote");
        let root = temp.path().join("volume");
        std::fs::create_dir(&remote).unwrap();
        std::fs::create_dir(&root).unwrap();
        git(&remote, &["init", "-b", "main"]);
        std::fs::write(remote.join("fixture"), "base").unwrap();
        git(&remote, &["add", "."]);
        git(&remote, &["commit", "-m", "fixture"]);
        let args = repository::PrepareArgs {
            account: "fixture".into(),
            repository: remote.to_string_lossy().into_owned(),
            base: "main".into(),
            container_id: "test-container".into(),
            branch: None,
        };
        Self {
            _temp: temp,
            remote,
            root,
            args,
        }
    }

    fn prepare(&self) -> Result<String> {
        repository::prepare(&self.root, &self.args)
    }
}

#[test]
fn independent_clone_reuses_without_alternates_and_keeps_private_worktrees() {
    let f = RepoFixture::new();
    let revision = f.prepare().unwrap();
    let repo = f.root.join("repo");
    assert_eq!(revision, git(&f.remote, &["rev-parse", "HEAD"]));
    assert!(repo.join(".git").is_dir());
    assert!(!repo.join(".git/objects/info/alternates").exists());
    let worktree = f.root.join("issue-1");
    git(
        &repo,
        &[
            "worktree",
            "add",
            "-b",
            "issue-1",
            worktree.to_str().unwrap(),
        ],
    );
    assert_eq!(f.prepare().unwrap(), revision);
    assert!(worktree.join("fixture").exists());
    assert!(git(&repo, &["rev-parse", "--git-common-dir"]).starts_with(".git"));
}

#[test]
fn dirty_root_and_dirty_worktree_are_preserved() {
    let f = RepoFixture::new();
    f.prepare().unwrap();
    let repo = f.root.join("repo");
    std::fs::write(repo.join("fixture"), "unfinished").unwrap();
    assert!(f.prepare().unwrap_err().to_string().contains("dirty"));
    assert_eq!(std::fs::read_to_string(repo.join("fixture")).unwrap(), "unfinished");
    git(&repo, &["restore", "fixture"]);
    let worktree = f.root.join("issue-2");
    git(
        &repo,
        &[
            "worktree",
            "add",
            "-b",
            "issue-2",
            worktree.to_str().unwrap(),
        ],
    );
    std::fs::write(worktree.join("untracked"), "retain").unwrap();
    assert!(f.prepare().unwrap_err().to_string().contains("dirty"));
    assert!(worktree.join("untracked").exists());
}

#[test]
fn unpushed_and_detached_commits_survive_refused_reuse() {
    let f = RepoFixture::new();
    f.prepare().unwrap();
    let repo = f.root.join("repo");
    git(&repo, &["switch", "-c", "issue-3"]);
    std::fs::write(repo.join("fixture"), "unpushed").unwrap();
    git(&repo, &["commit", "-am", "unpublished"]);
    let tip = git(&repo, &["rev-parse", "HEAD"]);
    assert!(f.prepare().unwrap_err().to_string().contains("unpushed"));
    assert_eq!(git(&repo, &["rev-parse", "HEAD"]), tip);
    git(&repo, &["switch", "--detach"]);
    git(&repo, &["branch", "-D", "issue-3"]);
    assert!(f.prepare().unwrap_err().to_string().contains("detached"));
    assert_eq!(git(&repo, &["rev-parse", "HEAD"]), tip);
}

#[test]
fn mismatched_identity_unknown_work_and_alternates_are_refused() {
    let mut f = RepoFixture::new();
    f.prepare().unwrap();
    f.args.repository.push_str("-other");
    assert!(f
        .prepare()
        .unwrap_err()
        .to_string()
        .contains("identity mismatch"));
    f.args.repository.truncate(f.args.repository.len() - 6);
    std::fs::write(f.root.join("repo/.git/objects/info/alternates"), "/host/objects").unwrap();
    assert!(f
        .prepare()
        .unwrap_err()
        .to_string()
        .contains("private Git objects"));
    let unknown = tempfile::tempdir().unwrap();
    std::fs::write(unknown.path().join("precious"), "keep").unwrap();
    assert!(repository::prepare(unknown.path(), &f.args)
        .unwrap_err()
        .to_string()
        .contains("nonempty"));
    assert!(unknown.path().join("precious").exists());
}

#[test]
fn https_remote_rejects_credentials_and_unsupported_transports() {
    assert!(repository_url("https://github.com/rjwalters/loom.git").is_ok());
    for url in [
        "https://token@github.com/a/b",
        "https://github.com/a/b?token=x",
        "https://github.com/a/b#x",
        "file:///tmp/repo",
        "ssh://git@github.com/a/b",
        "/tmp/repo",
        "https://github.com/",
    ] {
        assert!(repository_url(url).is_err(), "accepted {url}");
    }
}

fn config() -> Config {
    Config {
        schema_version: 1,
        account: "account".into(),
        container: "session".into(),
        engine: "engine".into(),
        docker_desktop: false,
        repository: "https://forge.example/a/b.git".into(),
        base: "main".into(),
        volume: "private-volume".into(),
        profile: "/external/account".into(),
        gh_config: None,
    }
}

#[test]
fn actual_mounts_privilege_and_volume_driver_options_are_enforced() {
    let config = config();
    let good = serde_json::json!({
        "Config": {"Labels": {"loom.workspace-mode": MODE, "loom.account": config.account, "loom.repository": config.repository, "loom.workspace": REPO}, "User":"1000:1000", "Entrypoint":["/usr/bin/tini"], "Cmd":["--","/bin/sleep","infinity"]},
        "HostConfig": {"Privileged":false,"ReadonlyRootfs":true,"CapDrop":["ALL"],"SecurityOpt":["no-new-privileges"]},
        "Mounts":[{"Destination":ROOT,"Type":"volume","Name":config.volume,"RW":true}, {"Destination":PROFILE,"Type":"bind","Source":config.profile,"RW":true}]
    });
    docker::validate_settings(&config, &good).unwrap();
    let mut bad = good.clone();
    bad["HostConfig"]["Privileged"] = true.into();
    assert!(docker::validate_settings(&config, &bad).is_err());
    let mut bad = good.clone();
    bad["HostConfig"]["CapAdd"] = serde_json::json!(["SYS_ADMIN"]);
    assert!(docker::validate_settings(&config, &bad).is_err());
    let mut bad = good.clone();
    bad["HostConfig"]["PidMode"] = "host".into();
    assert!(docker::validate_settings(&config, &bad).is_err());
    let mut bad = good.clone();
    bad["Mounts"][0]["Name"] = "peer-volume".into();
    assert!(docker::validate_settings(&config, &bad).is_err());
    let mut bad = good.clone();
    bad["Mounts"]
        .as_array_mut()
        .unwrap()
        .push(serde_json::json!({"Destination":"/host","Type":"bind","Source":"/repo","RW":true}));
    assert!(docker::validate_settings(&config, &bad).is_err());
    let mut translated = good.clone();
    translated["Mounts"][1]["Source"] = format!("/host_mnt{}", config.profile.display()).into();
    assert!(docker::validate_settings(&config, &translated).is_err());
    let mut desktop = config.clone();
    desktop.docker_desktop = true;
    assert_eq!(
        docker::validate_settings(&desktop, &translated).is_ok(),
        cfg!(target_os = "macos")
    );
    translated["Mounts"][1]["Source"] = "/host_mnt/unrelated/account".into();
    assert!(docker::validate_settings(&desktop, &translated).is_err());
    let volume = serde_json::json!({"Driver":"local", "Options":null, "Labels":{"loom.account":config.account,"loom.repository":config.repository,"loom.profile":config.profile}});
    docker::validate_volume_settings(&config, &volume).unwrap();
    let mut bad = volume.clone();
    bad["Options"] = serde_json::json!({"type":"none","o":"bind","device":"/host/repo"});
    assert!(docker::validate_volume_settings(&config, &bad).is_err());
    let mut bad = volume;
    bad["Driver"] = "network-plugin".into();
    assert!(docker::validate_volume_settings(&config, &bad).is_err());
}

#[test]
fn symlinked_git_objects_and_refs_are_refused_without_modification() {
    use std::os::unix::fs::symlink;
    for component in ["objects", "refs"] {
        let f = RepoFixture::new();
        f.prepare().unwrap();
        let source = f.root.join("repo/.git").join(component);
        let moved = f._temp.path().join(format!("outside-{component}"));
        std::fs::rename(&source, &moved).unwrap();
        symlink(&moved, &source).unwrap();
        assert!(f.prepare().unwrap_err().to_string().contains("symlinks"));
        assert!(moved.is_dir());
        assert!(std::fs::symlink_metadata(source)
            .unwrap()
            .file_type()
            .is_symlink());
    }
}

#[test]
fn account_lease_is_exclusive_across_role_sweep_interactive_and_repo_claims() {
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("account-a");
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(4));
    let winners = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    std::thread::scope(|scope| {
        for _ in 0..3 {
            let barrier = barrier.clone();
            let winners = winners.clone();
            let dir = &dir;
            scope.spawn(move || {
                barrier.wait();
                let guard = lease::Lease::acquire(dir);
                if guard.is_ok() {
                    winners.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                }
                barrier.wait();
                drop(guard);
            });
        }
        barrier.wait();
        barrier.wait();
    });
    assert_eq!(winners.load(std::sync::atomic::Ordering::SeqCst), 1);
    let _a = lease::Lease::acquire(&dir).unwrap();
    let _b = lease::Lease::acquire(&root.path().join("account-b")).unwrap();
    assert!(lease::Lease::acquire(&dir).is_err());
}

#[test]
fn ambiguous_lease_is_durable_and_stopped_container_recovery_retains_last_owner() {
    let root = tempfile::tempdir().unwrap();
    let lease = lease::Lease::acquire(root.path()).unwrap();
    let job = lease::Job {
        owner: "sweep-a".into(),
        kind: JobKind::Sweep,
        issue: Some(42),
        branch: Some("issue-42".into()),
        container_id: "old".into(),
        base_revision: "abc".into(),
        host_pid: 999_999,
    };
    lease.begin(&job).unwrap();
    drop(lease);
    let lease = lease::Lease::acquire(root.path()).unwrap();
    assert_eq!(lease::read(root.path()).unwrap().unwrap().owner, "sweep-a");
    let config = Config {
        schema_version: 1,
        account: "a".into(),
        container: "unused".into(),
        engine: "engine".into(),
        docker_desktop: false,
        repository: "https://example.com/a/b".into(),
        base: "main".into(),
        volume: "volume".into(),
        profile: "/external/profile".into(),
        gh_config: None,
    };
    assert!(lease
        .recover(&config, Some(&serde_json::json!({"Id":"new", "State":{"Running":false}})))
        .is_err());
    assert!(lease::read(root.path()).unwrap().is_some());
    lease
        .recover(&config, Some(&serde_json::json!({"Id":"old", "State":{"Running":false}})))
        .unwrap();
    assert!(lease::read(root.path()).unwrap().is_none());
    assert!(root.path().join("last-job.json").exists());
}
