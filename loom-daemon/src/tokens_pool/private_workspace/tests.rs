use super::*;

/// Serializes every test in this module that forks (a `git` fixture, a
/// `Command::spawn`) or holds an account lease. `fork()` copies every open
/// descriptor, so a fork racing a held lease keeps the `flock` alive in the
/// child until it execs and `Lease::acquire` reads busy (issue #10798). One
/// lock for the forkers and the lease holders, not a global `#[serial]`.
pub(super) fn fork_guard() -> std::sync::MutexGuard<'static, ()> {
    static FORK_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    FORK_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Re-acquire a lease this test just released, waiting (bounded) for the
/// release to become visible (#9409, #9394, #9497).
///
/// [`fork_guard`] only serializes the forkers in *this* module. Every other
/// test in the binary that spawns a process (`git`, `sh`, `gh` fixtures, of
/// which there are thousands) forks too, and its child holds a copy of every
/// open descriptor — including our close-on-exec lease fd — until it execs.
/// `flock` belongs to the open file description, so while that child sits
/// between fork and exec the lock outlives our `drop` and a one-shot
/// `Lease::acquire` reads busy. That window is microseconds on a quiet host
/// and stretches under load (a second concurrent `cargo test`, a busy shared
/// worker), which is why the failure only showed up there. It is not
/// cross-process contention: every lease root here is a fresh tempdir.
///
/// Production tolerates the same transient (a busy dispatch is refused and
/// retried next tick); a test that asserts "free right after release" must
/// wait for the stray copy to close, not read once.
pub(super) fn reacquire(dir: &std::path::Path) -> lease::Lease {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        match lease::Lease::acquire(dir) {
            Ok(lease) => return lease,
            Err(e) if std::time::Instant::now() >= deadline => {
                panic!("lease must be free after its holder released it: {e}")
            }
            Err(_) => std::thread::sleep(std::time::Duration::from_millis(10)),
        }
    }
}

/// The mechanism [`reacquire`] exists for, pinned deterministically: a child
/// forked while a lease is held keeps the `flock` alive after the parent drops
/// it, so a one-shot re-acquire reads busy until the child is gone.
#[test]
#[serial_test::serial(private_workspace_fork)]
fn a_forked_childs_inherited_lease_copy_outlives_the_parents_release() {
    let _fork = fork_guard();
    let root = tempfile::tempdir().unwrap();
    let held = lease::Lease::acquire(root.path()).unwrap();
    // The child blocks on this pipe instead of sleeping, so it is alive for
    // exactly as long as the assertion needs and no longer (it holds a copy
    // of every descriptor in the process, other tests' locks included).
    let mut fds = [0; 2];
    assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
    let [read_end, write_end] = fds;
    // SAFETY: the child only calls async-signal-safe functions (`read`,
    // `_exit`), as POSIX requires after `fork` in a multi-threaded process.
    let pid = unsafe { libc::fork() };
    assert!(pid >= 0, "fork failed");
    if pid == 0 {
        unsafe {
            let mut byte = 0u8;
            libc::read(read_end, std::ptr::addr_of_mut!(byte).cast(), 1);
            libc::_exit(0);
        }
    }
    drop(held);
    let busy = lease::Lease::acquire(root.path()).is_err();
    unsafe {
        libc::write(write_end, b"x".as_ptr().cast(), 1);
        libc::close(write_end);
        libc::close(read_end);
    }
    let mut status = 0;
    assert_eq!(unsafe { libc::waitpid(pid, &mut status, 0) }, pid);
    assert!(busy, "the forked child's descriptor copy must still hold the flock");
    drop(reacquire(root.path()));
}

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

use crate::test_git_helpers::{configure, run as git};

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
        configure(&remote);
        std::fs::write(remote.join("fixture"), "base").unwrap();
        git(&remote, &["add", "."]);
        git(&remote, &["commit", "-m", "fixture"]);
        let args = repository::PrepareArgs {
            account: "fixture".into(),
            repository: remote.to_string_lossy().into_owned(),
            base: "main".into(),
            container_id: "test-container".into(),
            branch: None,
            slug: None,
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
    let _fork = fork_guard();
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
    let _fork = fork_guard();
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
    let _fork = fork_guard();
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
    let _fork = fork_guard();
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
        repositories: vec![],
        volume: "private-volume".into(),
        profile: "/external/account".into(),
        gh_config: None,
    }
}

#[test]
fn actual_mounts_privilege_and_volume_driver_options_are_enforced() {
    let config = config();
    let mut good = serde_json::json!({
        "Config": {"Labels": {"loom.workspace-mode": MODE, "loom.account": config.account, "loom.repository": config.repository, "loom.workspace": REPO}, "User":"1000:1000", "Entrypoint":["/usr/bin/tini"], "Cmd":["--","/bin/sleep","infinity"]},
        "HostConfig": {"Privileged":false,"ReadonlyRootfs":true,"CapDrop":["ALL"],"SecurityOpt":["no-new-privileges"]},
        "Mounts":[{"Destination":ROOT,"Type":"volume","Name":config.volume,"RW":true}, {"Destination":PROFILE,"Type":"bind","Source":config.profile,"RW":true}]
    });
    // Every profile control file is bound read-only over its own canonical
    // path (issue #8839): that mount point is what makes the hook registration
    // and Codex's trust state unwritable, unremovable and unrenameable from
    // inside the session, while `auth.json` beside them stays writable.
    for name in bundle::PROFILE_CONTROLS {
        good["Mounts"]
            .as_array_mut()
            .unwrap()
            .push(serde_json::json!({
                "Destination": bundle::control_destination(name),
                "Type": "bind",
                "Source": config.profile.join(name),
                "RW": false,
            }));
    }
    let good = good;
    docker::validate_settings(&config, &good).unwrap();
    // Dropping ANY one of them, or re-admitting it read-write, is refused:
    // either shape hands the registration back to the worker.
    for index in 2..2 + bundle::PROFILE_CONTROLS.len() {
        let mut missing = good.clone();
        missing["Mounts"].as_array_mut().unwrap().remove(index);
        assert!(
            docker::validate_settings(&config, &missing).is_err(),
            "a session missing control mount {index} was accepted"
        );
        let mut writable = good.clone();
        writable["Mounts"][index]["RW"] = true.into();
        assert!(
            docker::validate_settings(&config, &writable).is_err(),
            "a writable control mount {index} was accepted"
        );
        // A bind that lands on the right path from the WRONG source is refused
        // too, so a decoy file cannot stand in for the canonical one.
        let mut decoy = good.clone();
        decoy["Mounts"][index]["Source"] = "/tmp/decoy".into();
        assert!(docker::validate_settings(&config, &decoy).is_err());
    }
    // A session created before this protection (the profile directory bound
    // read-write and nothing else) is refused rather than silently reused.
    let mut legacy = good.clone();
    legacy["Mounts"].as_array_mut().unwrap().truncate(2);
    assert!(docker::validate_settings(&config, &legacy).is_err());
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
    let _fork = fork_guard();
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
#[serial_test::serial(private_workspace_fork)]
fn account_lease_is_exclusive_across_role_sweep_interactive_and_repo_claims() {
    let _fork = fork_guard();
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
    let _a = reacquire(&dir);
    let _b = lease::Lease::acquire(&root.path().join("account-b")).unwrap();
    assert!(lease::Lease::acquire(&dir).is_err());
}

#[test]
#[serial_test::serial(private_workspace_fork)]
fn ambiguous_lease_is_durable_and_stopped_container_recovery_retains_last_owner() {
    let _fork = fork_guard();
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
        control: "d".repeat(64),
        repository: String::new(),
    };
    lease.begin(&job).unwrap();
    drop(lease);
    let lease = reacquire(root.path());
    assert_eq!(lease::read(root.path()).unwrap().unwrap().owner, "sweep-a");
    let config = Config {
        schema_version: 1,
        account: "a".into(),
        container: "unused".into(),
        engine: "engine".into(),
        docker_desktop: false,
        repository: "https://example.com/a/b".into(),
        base: "main".into(),
        repositories: vec![],
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

// ---- Multi-repository account storage (#10795) ----------------------------

#[test]
fn same_basename_under_different_owners_or_hosts_gets_distinct_paths() {
    let a = identify("https://forge.example/alice/tool").unwrap();
    let b = identify("https://forge.example/bob/tool").unwrap();
    let c = identify("https://other.example/alice/tool").unwrap();
    let slugs = [&a.slug, &b.slug, &c.slug];
    assert_eq!(slugs.iter().collect::<std::collections::HashSet<_>>().len(), 3, "{slugs:?}");
    // One repository, several spellings: one identity.
    let same = identify("https://FORGE.example/Alice/Tool.git").unwrap();
    assert_eq!((same.key, same.slug), (a.key.clone(), a.slug.clone()));
    assert!(same_repository(
        "https://forge.example/alice/tool/",
        "https://forge.example/alice/tool.git"
    ));
    assert!(!same_repository(
        "https://forge.example/alice/tool",
        "https://forge.example/bob/tool"
    ));
    // A forge under a path prefix keeps the whole path in its identity.
    let prefixed = identify("https://forge.example/git/alice/tool").unwrap();
    assert_ne!(prefixed.slug, a.slug);
    assert!(prefixed.slug.starts_with("alice-tool-"), "{}", prefixed.slug);
    assert!(a
        .slug
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c)));
}

#[test]
fn unsafe_repository_identities_and_directory_names_are_refused() {
    for url in [
        "https://forge.example/a",
        "https://forge.example/../b",
        "https://forge.example/a/..",
        "https://forge.example/.hidden/b",
        "https://forge.example/-flag/b",
        "https://forge.example/a/b%2Fc",
        "ssh://git@forge.example/a/b",
        "https://user:secret@forge.example/a/b",
    ] {
        assert!(identify(url).is_err(), "accepted {url}");
    }
    let f = RepoFixture::new();
    f.prepare().unwrap();
    for slug in ["", "..", "../x", "a/b", ".hidden", "a..b"] {
        assert!(
            repository::prepare(&f.root, &f.additional_args("..", slug)).is_err(),
            "accepted {slug:?}"
        );
    }
}

impl RepoFixture {
    fn additional_args(&self, remote: &str, slug: &str) -> repository::PrepareArgs {
        repository::PrepareArgs {
            repository: remote.into(),
            slug: Some(slug.into()),
            ..self.args.clone_args()
        }
    }
    /// A second, independent upstream with its own content.
    fn second_remote(&self, name: &str) -> PathBuf {
        let remote = self._temp.path().join(name);
        std::fs::create_dir(&remote).unwrap();
        git(&remote, &["init", "-b", "main"]);
        configure(&remote);
        std::fs::write(remote.join("fixture"), name).unwrap();
        git(&remote, &["add", "."]);
        git(&remote, &["commit", "-m", name]);
        remote
    }
}

impl repository::PrepareArgs {
    fn clone_args(&self) -> Self {
        Self {
            account: self.account.clone(),
            repository: self.repository.clone(),
            base: self.base.clone(),
            container_id: self.container_id.clone(),
            branch: self.branch.clone(),
            slug: self.slug.clone(),
        }
    }
}

#[test]
fn two_repositories_share_one_volume_with_independent_roots_and_untouched_first_clone() {
    let _fork = fork_guard();
    let f = RepoFixture::new();
    f.prepare().unwrap();
    let (r1, r2) = (f.second_remote("one"), f.second_remote("two"));
    let (a, b) = (
        f.additional_args(&r1.to_string_lossy(), "o-one-11111111"),
        f.additional_args(&r2.to_string_lossy(), "o-two-22222222"),
    );
    let rev_a = repository::prepare(&f.root, &a).unwrap();
    // Unfinished work in the first additional clone survives preparing the second.
    let first = f.root.join("work/o-one-11111111");
    std::fs::write(first.join("scratch"), "unfinished").unwrap();
    std::fs::write(first.join("fixture"), "edited").unwrap();
    let rev_b = repository::prepare(&f.root, &b).unwrap();
    assert_ne!(rev_a, rev_b);
    assert_eq!(std::fs::read_to_string(first.join("scratch")).unwrap(), "unfinished");
    assert_eq!(std::fs::read_to_string(first.join("fixture")).unwrap(), "edited");
    let second = f.root.join("work/o-two-22222222");
    for (root, remote) in [
        (&first, &r1),
        (&second, &r2),
        (&f.root.join("repo"), &f.remote),
    ] {
        assert_eq!(
            git(root, &["rev-parse", "--show-toplevel"]),
            root.canonicalize().unwrap().to_string_lossy()
        );
        assert_eq!(git(root, &["remote", "get-url", "origin"]), remote.to_string_lossy());
        assert!(!root.join(".git/objects/info/alternates").exists());
    }
    // Re-preparing a dirty additional clone is refused, not cleaned.
    assert!(repository::prepare(&f.root, &a)
        .unwrap_err()
        .to_string()
        .contains("dirty"));
    // A repository can never be prepared under another repository's directory.
    let swapped = f.additional_args(&r2.to_string_lossy(), "o-one-11111111");
    assert!(repository::prepare(&f.root, &swapped)
        .unwrap_err()
        .to_string()
        .contains("mismatch"));
}

#[test]
fn additional_clone_requires_the_primary_and_real_directories() {
    let _fork = fork_guard();
    let f = RepoFixture::new();
    let r = f.second_remote("one");
    let args = f.additional_args(&r.to_string_lossy(), "o-one-11111111");
    assert!(repository::prepare(&f.root, &args).is_err(), "no primary prepared yet");
    assert!(!f.root.join("work/o-one-11111111").exists());
    f.prepare().unwrap();
    let outside = tempfile::tempdir().unwrap();
    std::os::unix::fs::symlink(outside.path(), f.root.join("work")).unwrap();
    assert!(repository::prepare(&f.root, &args)
        .unwrap_err()
        .to_string()
        .contains("real directory"));
    assert!(
        std::fs::read_dir(outside.path()).unwrap().next().is_none(),
        "nothing written through the link"
    );
}

#[test]
fn interrupted_clone_is_recoverable_and_never_ready() {
    let _fork = fork_guard();
    let f = RepoFixture::new();
    f.prepare().unwrap();
    let slug = "o-one-11111111";
    // A clone that fails leaves no ready path behind.
    let missing = f._temp.path().join("absent");
    let bad = f.additional_args(&missing.to_string_lossy(), slug);
    assert!(repository::prepare(&f.root, &bad).is_err());
    assert!(!f.root.join("work").join(slug).exists(), "a failed clone must not look ready");
    // A crashed earlier attempt left a half-written staging directory.
    let staging = f.root.join("work").join(format!(".cloning-{slug}"));
    std::fs::create_dir_all(staging.join("partial")).unwrap();
    std::fs::write(staging.join("partial/junk"), "x").unwrap();
    let r = f.second_remote("one");
    let good = f.additional_args(&r.to_string_lossy(), slug);
    // The failed attempt wrote its identity for ANOTHER url: refused, nothing reset.
    assert!(repository::prepare(&f.root, &good)
        .unwrap_err()
        .to_string()
        .contains("mismatch"));
    std::fs::remove_file(f.root.join("work/.identity").join(format!("{slug}.json"))).unwrap();
    let revision = repository::prepare(&f.root, &good).unwrap();
    assert!(!staging.exists());
    assert_eq!(git(&f.root.join("work").join(slug), &["rev-parse", "HEAD"]), revision);
}

#[test]
fn firewalled_repositories_are_matched_by_forge_identity_in_any_spelling() {
    let id = identify("https://forge.example/Acme/Notebook").unwrap();
    let rec = |name: &str, remote: Option<&str>| vec![(name.to_owned(), remote.map(str::to_owned))];
    for remote in [
        "https://forge.example/acme/notebook",
        "https://forge.example/Acme/Notebook.git",
        "git@forge.example:acme/notebook.git",
        "ssh://git@forge.example/acme/notebook",
    ] {
        assert!(repository::firewalled(&id, &rec("anything", Some(remote))), "{remote}");
    }
    assert!(!repository::firewalled(
        &id,
        &rec("notebook", Some("https://forge.example/other/notebook"))
    ));
    assert!(!repository::firewalled(
        &id,
        &rec("loom", Some("https://forge.example/acme/loom"))
    ));
    // No usable remote: fail closed on the repository name.
    assert!(repository::firewalled(&id, &rec("Notebook", None)));
    assert!(!repository::firewalled(&id, &rec("loom", None)));
}

#[test]
fn legacy_single_repository_state_loads_and_resolves_to_the_unchanged_primary() {
    let legacy = serde_json::json!({
        "schema_version": 1, "account": "a", "container": "c", "engine": "e",
        "repository": "https://forge.example/o/primary", "base": "main",
        "volume": "v", "profile": "/p", "gh_config": null
    });
    let mut config: Config = serde_json::from_value(legacy.clone()).unwrap();
    assert!(config.repositories.is_empty());
    // Unmigrated state serializes exactly as before.
    assert_eq!(serde_json::to_value(&config).unwrap()["repositories"], serde_json::Value::Null);
    let primary = config.target(None).unwrap();
    assert_eq!((primary.path.as_str(), primary.slug), (REPO, None));
    assert_eq!(
        config
            .target(Some("https://forge.example/o/primary.git"))
            .unwrap(),
        config.primary()
    );
    let err = config
        .target(Some("https://forge.example/o/other"))
        .unwrap_err()
        .to_string();
    assert!(err.contains("not registered") && err.contains("--private-clone"), "{err}");
    lifecycle_register(&mut config, "https://forge.example/o/other", "main").unwrap();
    let other = config
        .target(Some("https://forge.example/o/other"))
        .unwrap();
    assert!(other.path.starts_with("/workspace/work/o-other-"), "{}", other.path);
    assert_eq!(
        config.repository, "https://forge.example/o/primary",
        "primary identity is never rewritten"
    );
    // A base change for a configured repository is refused with recovery advice.
    let err = lifecycle_register(&mut config, "https://forge.example/o/other", "dev").unwrap_err();
    assert!(err.to_string().contains("resolve unfinished work"), "{err}");
    assert!(lifecycle_register(&mut config, "https://forge.example/o/primary", "dev").is_err());
}

fn lifecycle_register(config: &mut Config, repository: &str, base: &str) -> Result<()> {
    // No fleet store is configured for an empty workspace: the roster is empty.
    let workspace = tempfile::tempdir().unwrap();
    lifecycle::register(workspace.path(), config, repository, base)
}

#[test]
fn repository_bound_job_keeps_the_account_wide_exclusive_lease() {
    let _fork = fork_guard();
    let root = tempfile::tempdir().unwrap();
    let lease = lease::Lease::acquire(root.path()).unwrap();
    let job = |repository: &str| lease::Job {
        owner: "o".into(),
        kind: JobKind::Role,
        issue: None,
        branch: None,
        container_id: "a".repeat(64),
        base_revision: "r".into(),
        host_pid: 1,
        control: "c".repeat(64),
        repository: repository.into(),
    };
    lease.begin(&job("https://forge.example/o/other")).unwrap();
    assert_eq!(
        lease::read(root.path()).unwrap().unwrap().repository,
        "https://forge.example/o/other"
    );
    // A second job for a DIFFERENT clone of the same seat is excluded.
    assert!(lease::Lease::acquire(root.path())
        .err()
        .unwrap()
        .to_string()
        .contains("busy"));
    // A lease written before #10795 (no repository field) still reads: primary.
    let mut value = serde_json::to_value(job("")).unwrap();
    assert!(value.get("repository").is_none());
    value.as_object_mut().unwrap().remove("repository");
    std::fs::write(root.path().join("job.json"), serde_json::to_vec(&value).unwrap()).unwrap();
    assert_eq!(lease::read(root.path()).unwrap().unwrap().repository, "");
}
