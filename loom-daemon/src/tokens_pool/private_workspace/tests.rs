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
fn lease_renew_residue_does_not_refuse_reuse_but_other_dirt_does() {
    let _fork = fork_guard();
    let f = RepoFixture::new();
    f.prepare().unwrap();
    let repo = f.root.join("repo");
    // Loom's own lease-renewal lock left in the clone (#14160): untracked in
    // a repo whose ignore rules do not cover it. Residue Loom itself wrote
    // must never refuse a seat.
    std::fs::create_dir_all(repo.join(".loom/lease-renew")).unwrap();
    std::fs::write(repo.join(".loom/lease-renew/.lock"), "").unwrap();
    f.prepare().unwrap();
    // A linked worktree holding the same residue is forgiven too.
    let worktree = f.root.join("issue-5");
    git(
        &repo,
        &[
            "worktree",
            "add",
            "-b",
            "issue-5",
            worktree.to_str().unwrap(),
        ],
    );
    std::fs::create_dir_all(worktree.join(".loom/lease-renew")).unwrap();
    std::fs::write(worktree.join(".loom/lease-renew/.lock"), "").unwrap();
    f.prepare().unwrap();
    // Any other dirt still refuses, and the refusal names the path.
    std::fs::write(repo.join("stray.txt"), "retain").unwrap();
    let error = f.prepare().unwrap_err().to_string();
    assert!(error.contains("dirty"), "{error}");
    assert!(error.contains("stray.txt"), "{error}");
    assert!(repo.join("stray.txt").exists());
    // The named list is bounded (#14160).
    std::fs::remove_file(repo.join("stray.txt")).unwrap();
    for i in 0..12 {
        std::fs::write(repo.join(format!("stray-{i:02}")), "retain").unwrap();
    }
    let error = f.prepare().unwrap_err().to_string();
    assert!(error.contains("stray-09"), "{error}");
    assert!(!error.contains("stray-10"), "{error}");
    assert!(error.contains("and 2 more"), "{error}");
}

#[test]
#[serial_test::serial(private_workspace_fork)]
#[serial_test::serial]
fn unpushed_and_detached_commits_survive_refused_reuse() {
    let _fork = fork_guard();
    let mut f = RepoFixture::new();
    f.prepare().unwrap();
    let repo = f.root.join("repo");
    git(&repo, &["switch", "-c", "issue-3"]);
    std::fs::write(repo.join("fixture"), "unpushed").unwrap();
    git(&repo, &["commit", "-am", "unpublished"]);
    let tip = git(&repo, &["rev-parse", "HEAD"]);
    // A branch names the work and preparation never moves a branch: a new
    // step on the same issue resumes its own unpublished tip (#14030).
    f.args.branch = Some("issue-3".into());
    f.prepare().unwrap();
    assert_eq!(git(&repo, &["rev-parse", "HEAD"]), tip);
    // A step for a different issue proceeds and leaves the branch untouched.
    f.args.branch = Some("issue-4".into());
    f.prepare().unwrap();
    assert_eq!(git(&repo, &["rev-parse", "issue-3"]), tip);
    assert_eq!(git(&repo, &["rev-parse", "HEAD"]), git(&repo, &["rev-parse", "issue-4"]));
    // With no requested branch the dispatch detaches to the base; the
    // branch keeps carrying the work.
    f.args.branch = None;
    f.prepare().unwrap();
    assert_eq!(git(&repo, &["rev-parse", "issue-3"]), tip);
    // Work no longer named by a branch still refuses (a detached HEAD could
    // be detached away from): the refusal preserves it, names the leftover
    // unpublished branch and the exact command that clears it.
    f.args.branch = Some("issue-3".into());
    f.prepare().unwrap();
    std::fs::write(repo.join("fixture"), "more").unwrap();
    git(&repo, &["commit", "-am", "more-unpublished"]);
    git(&repo, &["switch", "-c", "strand"]);
    std::fs::write(repo.join("fixture"), "stranded").unwrap();
    git(&repo, &["commit", "-am", "stranded"]);
    git(&repo, &["switch", "--detach"]);
    git(&repo, &["branch", "-D", "strand"]);
    let stranded_tip = git(&repo, &["rev-parse", "HEAD"]);
    let error = f.prepare().unwrap_err().to_string();
    assert!(error.contains("unpushed"), "{error}");
    assert!(error.contains("detached"), "{error}");
    assert!(error.contains("issue-3"), "{error}");
    assert!(error.contains("accounts session recover fixture --publish"), "{error}");
    assert_eq!(git(&repo, &["rev-parse", "HEAD"]), stranded_tip);
}

/// A clean linked worktree a contained step created under the clone
/// (#14122): invisible to both the dirty check and the detached-HEAD scan,
/// yet holding exactly the branch the next preparation switches to.
fn linked_worktree_holding(repo: &Path, worktree: &Path, branch: &str) {
    git(repo, &["switch", "-c", branch]);
    std::fs::write(repo.join("fixture"), "held").unwrap();
    git(repo, &["commit", "-am", "held"]);
    git(repo, &["switch", "--detach", "main"]);
    git(repo, &["worktree", "add", worktree.to_str().unwrap(), branch]);
}

#[test]
#[serial_test::serial(private_workspace_fork)]
#[serial_test::serial]
fn a_clean_linked_worktree_holding_the_branch_refuses_and_names_the_cure() {
    let _fork = fork_guard();
    let mut f = RepoFixture::new();
    f.prepare().unwrap();
    let repo = f.root.join("repo");
    let worktree = f.root.join("issue-9");
    linked_worktree_holding(&repo, &worktree, "feature/issue-9");
    // A step for another issue proceeds: only the held branch refuses.
    f.args.branch = Some("feature/issue-10".into());
    f.prepare().unwrap();
    // The held branch itself refuses with the holder, the branch and both
    // recovery commands named, and the hold is left untouched.
    f.args.branch = Some("feature/issue-9".into());
    let error = f.prepare().unwrap_err().to_string();
    assert!(error.contains("linked worktree"), "{error}");
    assert!(error.contains(worktree.to_str().unwrap()), "{error}");
    assert!(error.contains("feature/issue-9"), "{error}");
    assert!(error.contains("recover fixture --discard --branch feature/issue-9"), "{error}");
    assert!(error.contains("recover fixture --publish"), "{error}");
    assert!(worktree.exists());
    // The named cure clears an unpublished hold: the discard removes the
    // holding worktree and the branch, and the next preparation proceeds.
    let mut recover = recover_args(&f.remote);
    recover.discard = true;
    recover.branch = vec!["feature/issue-9".into()];
    let out = repository::recover(&f.root, &recover).unwrap();
    assert_eq!(out.discarded, vec!["feature/issue-9".to_owned()]);
    assert!(!worktree.exists());
    let revision = f.prepare().unwrap();
    assert_eq!(revision, git(&f.remote, &["rev-parse", "HEAD"]));
}

#[test]
#[serial_test::serial(private_workspace_fork)]
#[serial_test::serial]
fn a_published_branch_held_by_a_worktree_names_the_hand_cure() {
    let _fork = fork_guard();
    let mut f = RepoFixture::new();
    f.prepare().unwrap();
    let repo = f.root.join("repo");
    let worktree = f.root.join("issue-9");
    linked_worktree_holding(&repo, &worktree, "feature/issue-9");
    // Publishing the branch (the real recover verb) preserves its commits on
    // origin but clears nothing: the hold survives, and the refusal no longer
    // names a discard — recovery never deletes a published branch, so the
    // only cure is removing the worktree by hand.
    let mut recover = recover_args(&f.remote);
    recover.publish = true;
    let out = repository::recover(&f.root, &recover).unwrap();
    assert_eq!(out.published, vec!["feature/issue-9".to_owned()]);
    let mut args = recover_args(&f.remote);
    args.publish = true;
    let noop = repository::recover(&f.root, &args).unwrap();
    assert!(noop.published.is_empty(), "the branch is no longer unpublished");
    f.args.branch = Some("feature/issue-9".into());
    let error = f.prepare().unwrap_err().to_string();
    assert!(error.contains("linked worktree"), "{error}");
    assert!(error.contains(worktree.to_str().unwrap()), "{error}");
    assert!(error.contains("worktree remove"), "{error}");
    assert!(error.contains("accounts session attach fixture"), "{error}");
    assert!(!error.contains("--discard --branch"), "{error}");
    assert!(worktree.exists());
    // The hand cure unwedges: the clean worktree carries nothing the branch
    // ref does not, so removing it releases the hold losslessly.
    git(&repo, &["worktree", "remove", worktree.to_str().unwrap()]);
    f.prepare().unwrap();
    assert_eq!(
        git(&repo, &["rev-parse", "feature/issue-9"]),
        git(&f.remote, &["rev-parse", "feature/issue-9"])
    );
}

#[test]
#[serial_test::serial(private_workspace_fork)]
#[serial_test::serial]
fn unpublished_commits_in_one_clone_do_not_block_another_clone_or_branch() {
    let _fork = fork_guard();
    let mut f = RepoFixture::new();
    f.prepare().unwrap();
    let repo = f.root.join("repo");
    git(&repo, &["switch", "-c", "issue-3"]);
    std::fs::write(repo.join("fixture"), "unpushed").unwrap();
    git(&repo, &["commit", "-am", "unpublished"]);
    let tip = git(&repo, &["rev-parse", "issue-3"]);
    // A second registered repository of the same account clones and
    // prepares independently of the primary's unpublished work (#14030).
    let additional = repository::PrepareArgs {
        account: "fixture".into(),
        repository: f.remote.to_string_lossy().into_owned(),
        base: "main".into(),
        container_id: "test-container".into(),
        branch: Some("feature/issue-9".into()),
        slug: Some("owner-repo-12345678".into()),
    };
    let add = repository::prepare(&f.root, &additional).unwrap();
    assert!(!add.is_empty());
    // Unpublished commits on one branch of the additional clone do not
    // block re-preparing it for its own branch, nor the primary at all.
    let clone = f.root.join("work").join("owner-repo-12345678");
    git(&clone, &["switch", "-c", "other-unpushed"]);
    std::fs::write(clone.join("fixture"), "other").unwrap();
    git(&clone, &["commit", "-am", "other-unpublished"]);
    f.args.branch = Some("issue-4".into());
    f.prepare().unwrap();
    assert_eq!(git(&repo, &["rev-parse", "issue-3"]), tip);
    repository::prepare(&f.root, &additional).unwrap();
    assert_eq!(
        git(&clone, &["rev-parse", "feature/issue-9"]),
        add,
        "the additional clone resumes its own branch"
    );
}

/// Fallible git probe for assertions on ref existence (`test_git_helpers::run`
/// panics on failure).
fn git_ok(dir: &Path, args: &[&str]) -> bool {
    crate::test_git_helpers::git_out(dir, args).status.success()
}

fn recover_args(remote: &Path) -> repository::RecoverArgs {
    repository::RecoverArgs {
        account: "fixture".into(),
        repository: remote.to_string_lossy().into_owned(),
        base: "main".into(),
        publish: false,
        discard: false,
        issue: None,
        branch: Vec::new(),
        worktree: Vec::new(),
        slug: None,
    }
}

/// Two unpublished branches (one ahead each) with the root checkout
/// detached at the base, and the base revision.
fn two_unpublished_branches() -> (RepoFixture, PathBuf, String, String) {
    let f = RepoFixture::new();
    f.prepare().unwrap();
    let repo = f.root.join("repo");
    let base = git(&repo, &["rev-parse", "origin/main"]);
    let branch = |name: &str, content: &str, repo: &Path| {
        git(repo, &["switch", "-c", name]);
        std::fs::write(repo.join("fixture"), content).unwrap();
        git(repo, &["commit", "-am", content]);
        let tip = git(repo, &["rev-parse", name]);
        git(repo, &["switch", "--detach", &base]);
        tip
    };
    let issue3 = branch("issue-3", "first", &repo);
    let issue4 = branch("issue-4", "second", &repo);
    (f, repo, issue3, issue4)
}

#[test]
#[serial_test::serial(private_workspace_fork)]
#[serial_test::serial]
fn recover_lists_unpublished_branches_and_stranded_detached_work() {
    let _fork = fork_guard();
    let (f, repo, issue3, issue4) = two_unpublished_branches();
    let list = repository::recover(&f.root, &recover_args(&f.remote)).unwrap();
    assert!(list.cloned);
    assert_eq!(
        list.branches
            .iter()
            .map(|b| (b.branch.as_str(), b.ahead))
            .collect::<Vec<_>>(),
        vec![("issue-3", 1), ("issue-4", 1)]
    );
    assert!(list.published.is_empty() && list.discarded.is_empty());
    assert!(list.ignored.is_empty() && list.detached.is_empty());
    // Detached, unnamed work is reported (never touched) by the listing.
    git(&repo, &["switch", "-c", "strand"]);
    std::fs::write(repo.join("fixture"), "stranded").unwrap();
    git(&repo, &["commit", "-am", "stranded"]);
    let stranded = git(&repo, &["rev-parse", "HEAD"]);
    git(&repo, &["switch", "--detach"]);
    git(&repo, &["branch", "-D", "strand"]);
    let list = repository::recover(&f.root, &recover_args(&f.remote)).unwrap();
    assert_eq!(list.detached.len(), 1);
    assert_eq!(list.detached[0].revision, stranded);
    assert_eq!(list.detached[0].ahead, 1);
    // The detached tip is preserved exactly (nothing was reset).
    assert_eq!(git(&repo, &["rev-parse", "HEAD"]), stranded);
    assert_eq!(git(&repo, &["rev-parse", "issue-3"]), issue3);
    assert_eq!(git(&repo, &["rev-parse", "issue-4"]), issue4);
    // A clone the account never materialized reports cloned=false.
    let mut absent = recover_args(&f.remote);
    absent.slug = Some("owner-other-12345678".into());
    let list = repository::recover(&f.root, &absent).unwrap();
    assert!(!list.cloned);
}

#[test]
#[serial_test::serial(private_workspace_fork)]
#[serial_test::serial]
fn recover_publishes_scoped_branches_and_leaves_others_unpublished() {
    let _fork = fork_guard();
    let (f, repo, issue3, issue4) = two_unpublished_branches();
    let mut args = recover_args(&f.remote);
    args.publish = true;
    args.branch = vec!["issue-3".into()];
    let out = repository::recover(&f.root, &args).unwrap();
    assert_eq!(out.published, vec!["issue-3".to_owned()]);
    // The remote carries the branch now, and the listing agrees.
    assert_eq!(git(&f.remote, &["rev-parse", "issue-3"]), issue3);
    assert_eq!(
        out.branches
            .iter()
            .map(|b| b.branch.as_str())
            .collect::<Vec<_>>(),
        vec!["issue-4"]
    );
    // issue-4 was never touched: still unpublished at its own tip.
    assert_eq!(git(&repo, &["rev-parse", "issue-4"]), issue4);
    // Publishing an absent branch is ignored, not an error.
    args.branch = vec!["absent".into()];
    let out = repository::recover(&f.root, &args).unwrap();
    assert_eq!(out.ignored, vec!["absent".to_owned()]);
    assert!(out.published.is_empty());
}

#[test]
#[serial_test::serial(private_workspace_fork)]
#[serial_test::serial]
fn recover_discard_requires_a_scope_and_only_touches_named_branches() {
    let _fork = fork_guard();
    let (f, repo, _issue3, issue4) = two_unpublished_branches();
    let mut args = recover_args(&f.remote);
    args.discard = true;
    let error = repository::recover(&f.root, &args).unwrap_err().to_string();
    assert!(error.contains("scope"), "{error}");
    // Nothing was discarded without a scope.
    assert!(git_ok(&repo, &["show-ref", "--verify", "refs/heads/issue-3"]));
    // Only the named branch is discarded.
    args.branch = vec!["issue-3".into()];
    let out = repository::recover(&f.root, &args).unwrap();
    assert_eq!(out.discarded, vec!["issue-3".to_owned()]);
    assert!(!git_ok(&repo, &["show-ref", "--verify", "refs/heads/issue-3"]));
    assert_eq!(git(&repo, &["rev-parse", "issue-4"]), issue4);
    // A published branch (and an absent one) is ignored: a discard never
    // deletes a branch it was not named for, nor one carrying no unpublished
    // commits.
    args.branch = vec!["main".into(), "ghost".into()];
    let out = repository::recover(&f.root, &args).unwrap();
    assert_eq!(out.ignored, vec!["ghost".to_owned(), "main".to_owned()]);
    assert!(out.discarded.is_empty());
    assert!(git_ok(&repo, &["show-ref", "--verify", "refs/heads/main"]));
}

#[test]
#[serial_test::serial(private_workspace_fork)]
#[serial_test::serial]
fn recover_discard_moves_the_checkout_and_removes_a_holding_worktree() {
    let _fork = fork_guard();
    let f = RepoFixture::new();
    f.prepare().unwrap();
    let repo = f.root.join("repo");
    let base = git(&repo, &["rev-parse", "origin/main"]);
    // A branch held by the main checkout.
    git(&repo, &["switch", "-c", "held"]);
    std::fs::write(repo.join("fixture"), "held").unwrap();
    git(&repo, &["commit", "-am", "held"]);
    // A branch held by a linked worktree.
    let worktree = f.root.join("issue-5");
    git(
        &repo,
        &[
            "worktree",
            "add",
            "-b",
            "wt-held",
            worktree.to_str().unwrap(),
        ],
    );
    std::fs::write(worktree.join("fixture"), "wt-held").unwrap();
    git(&worktree, &["add", "."]);
    git(&worktree, &["commit", "-am", "wt-held"]);
    let mut args = recover_args(&f.remote);
    args.discard = true;
    args.branch = vec!["held".into(), "wt-held".into()];
    let out = repository::recover(&f.root, &args).unwrap();
    assert_eq!(out.discarded.len(), 2);
    assert!(!git_ok(&repo, &["show-ref", "--verify", "refs/heads/held"]));
    assert!(!git_ok(&repo, &["show-ref", "--verify", "refs/heads/wt-held"]));
    // The main checkout was moved to the base on origin, not left stranded.
    assert_eq!(git(&repo, &["rev-parse", "HEAD"]), base);
    assert!(!worktree.exists(), "the holding worktree was removed");
    // A dirty worktree holding a discarded branch refuses before any
    // mutation.
    let dirty = f.root.join("issue-6");
    git(
        &repo,
        &[
            "worktree",
            "add",
            "-b",
            "dirty-holder",
            dirty.to_str().unwrap(),
        ],
    );
    std::fs::write(dirty.join("fixture"), "committed").unwrap();
    git(&dirty, &["add", "."]);
    git(&dirty, &["commit", "-am", "committed"]);
    std::fs::write(dirty.join("untracked"), "keep").unwrap();
    args.branch = vec!["dirty-holder".into()];
    let error = repository::recover(&f.root, &args).unwrap_err().to_string();
    assert!(error.contains("uncommitted"), "{error}");
    assert!(git_ok(&repo, &["show-ref", "--verify", "refs/heads/dirty-holder"]));
    assert!(dirty.join("untracked").exists());
}

#[test]
#[serial_test::serial(private_workspace_fork)]
#[serial_test::serial]
fn recover_discard_refuses_a_dirty_main_checkout_holding_the_branch() {
    let _fork = fork_guard();
    let f = RepoFixture::new();
    f.prepare().unwrap();
    let repo = f.root.join("repo");
    // The main checkout holds the branch — where `prepare` itself leaves it —
    // with uncommitted work on top.
    git(&repo, &["switch", "-c", "held-dirty"]);
    std::fs::write(repo.join("fixture"), "committed").unwrap();
    git(&repo, &["commit", "-am", "committed"]);
    std::fs::write(repo.join("untracked"), "keep").unwrap();
    let mut args = recover_args(&f.remote);
    args.discard = true;
    args.branch = vec!["held-dirty".into()];
    let error = repository::recover(&f.root, &args).unwrap_err().to_string();
    assert!(error.contains("main checkout"), "{error}");
    assert!(error.contains("uncommitted"), "{error}");
    // The branch, the checkout, and the uncommitted work are all untouched:
    // no half-done discard.
    assert!(git_ok(&repo, &["show-ref", "--verify", "refs/heads/held-dirty"]));
    assert_eq!(git(&repo, &["rev-parse", "--abbrev-ref", "HEAD"]), "held-dirty");
    assert!(repo.join("untracked").exists());
}

#[test]
#[serial_test::serial(private_workspace_fork)]
#[serial_test::serial]
fn recover_discard_resolves_the_base_before_mutating_any_holder() {
    let _fork = fork_guard();
    let (f, repo, _issue3, _issue4) = two_unpublished_branches();
    // A linked worktree holds the branch that sorts first, so the old code
    // removed it before the main checkout's unresolvable base failed.
    let worktree = f.root.join("issue-7");
    git(
        &repo,
        &[
            "worktree",
            "add",
            "-b",
            "aaa-wt",
            worktree.to_str().unwrap(),
        ],
    );
    std::fs::write(worktree.join("fixture"), "aaa-wt").unwrap();
    git(&worktree, &["add", "."]);
    git(&worktree, &["commit", "-am", "aaa-wt"]);
    // The main checkout holds the other discarded branch.
    git(&repo, &["switch", "issue-3"]);
    let mut args = recover_args(&f.remote);
    args.discard = true;
    args.base = "no-such-base".into();
    args.branch = vec!["issue-3".into(), "aaa-wt".into()];
    let error = repository::recover(&f.root, &args).unwrap_err().to_string();
    assert!(error.contains("base"), "{error}");
    // Neither holder was touched: the base resolves before the first mutation.
    assert!(git_ok(&repo, &["show-ref", "--verify", "refs/heads/issue-3"]));
    assert!(git_ok(&repo, &["show-ref", "--verify", "refs/heads/aaa-wt"]));
    assert!(worktree.exists(), "the holding worktree was not removed");
    assert_eq!(git(&repo, &["rev-parse", "--abbrev-ref", "HEAD"]), "issue-3");
}

#[test]
#[serial_test::serial(private_workspace_fork)]
#[serial_test::serial]
fn recover_refuses_an_unknown_clone_identity() {
    let _fork = fork_guard();
    let (f, _repo, _issue3, _issue4) = two_unpublished_branches();
    let mut args = recover_args(&f.remote);
    args.account = "mallory".into();
    let error = repository::recover(&f.root, &args).unwrap_err().to_string();
    assert!(error.contains("identity mismatch"), "{error}");
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

/// #13916: on an App-only host the prepare exec names `GH_TOKEN` as a bare
/// `--env` and the resolved value rides on the `docker` child alone — never
/// in argv. A token already in the process env adds nothing to the child.
#[test]
fn prepare_exec_carries_a_resolved_token_on_the_child_never_in_argv() {
    const SECRET: &str = "ghs_prepareTESTONLY13916";
    let mut config = config();
    config.repository = "https://github.com/acme/widget".into();
    let argv = std::rc::Rc::new(std::cell::RefCell::new(Vec::<String>::new()));
    let seen = argv.clone();
    let _fake = docker::fake::install(Box::new(move |args| {
        *seen.borrow_mut() = args.iter().map(|a| (*a).to_owned()).collect();
        Ok(serde_json::json!({"protocol": PROTOCOL, "revision": "abc"}).to_string())
    }));
    let ws = forge_auth::published_owner_workspace("acme");
    let resolved =
        forge_auth::ExecAuth::resolve_with(ws.path(), &config.repository, &|_| None, &|_| {
            Some(SECRET.into())
        });
    let revision =
        docker::prepare_with(&config, &config.primary(), &"a".repeat(64), None, &resolved).unwrap();
    assert_eq!(revision, "abc");
    let args = argv.borrow().clone();
    assert!(
        args.windows(2)
            .any(|w| w[0] == "--env" && w[1] == "GH_TOKEN"),
        "{args:?}"
    );
    assert!(!args.iter().any(|a| a.contains(SECRET)), "token leaked into argv");
    assert_eq!(docker::fake::seen_env(), vec![vec![("GH_TOKEN".to_owned(), SECRET.to_owned())]]);

    // The process already holds a token: forwarded by name, nothing added.
    let ambient = forge_auth::ExecAuth::resolve_with(
        Path::new("/nonexistent"),
        &config.repository,
        &|name| (name == "GH_TOKEN").then(|| "from-process".into()),
        &|_| panic!("must not read a token the process already has"),
    );
    docker::prepare_with(&config, &config.primary(), &"a".repeat(64), None, &ambient).unwrap();
    let args = argv.borrow().clone();
    assert!(args
        .windows(2)
        .any(|w| w[0] == "--env" && w[1] == "GH_TOKEN"));
    assert_eq!(docker::fake::seen_env().last().unwrap(), &Vec::new());
}

// ---- rebind after a repository transfer (#14357) ----

const OLD_SLUG: &str = "old-one-11111111";
const NEW_SLUG: &str = "new-one-22222222";

/// A transferred repository: the old remote's history under a new location.
fn transferred(f: &RepoFixture, old: &Path) -> PathBuf {
    let new = f._temp.path().join("transferred");
    git(
        f._temp.path(),
        &[
            "clone",
            "--bare",
            &old.to_string_lossy(),
            &new.to_string_lossy(),
        ],
    );
    new
}

fn rebind_args(old: &Path, new: &Path, dry_run: bool) -> repository::RebindArgs {
    repository::RebindArgs {
        account: "fixture".into(),
        from: old.to_string_lossy().into_owned(),
        from_slug: OLD_SLUG.into(),
        to: new.to_string_lossy().into_owned(),
        to_slug: NEW_SLUG.into(),
        base: "main".into(),
        dry_run,
    }
}

/// A primary plus a populated old clone carrying an unpublished branch and a
/// dirty file, and the transferred remote.
fn old_clone_with_work() -> (RepoFixture, PathBuf, PathBuf, PathBuf) {
    let f = RepoFixture::new();
    f.prepare().unwrap();
    let old = f.second_remote("old");
    repository::prepare(&f.root, &f.additional_args(&old.to_string_lossy(), OLD_SLUG)).unwrap();
    let clone = f.root.join("work").join(OLD_SLUG);
    git(&clone, &["switch", "-c", "feature/issue-9"]);
    std::fs::write(clone.join("fixture"), "unpublished").unwrap();
    git(&clone, &["commit", "-am", "work"]);
    std::fs::write(clone.join("scratch"), "dirty").unwrap();
    let new = transferred(&f, &old);
    (f, old, new, clone)
}

fn record_of(f: &RepoFixture, slug: &str) -> String {
    std::fs::read_to_string(f.root.join("work/.identity").join(format!("{slug}.json"))).unwrap()
}

#[test]
fn rebind_moves_an_old_clone_keeping_its_work_and_is_idempotent() {
    let _fork = fork_guard();
    let (f, old, new, clone) = old_clone_with_work();
    let moved = f.root.join("work").join(NEW_SLUG);
    // The dry run verifies and reports; nothing moves.
    let dry = repository::rebind(&f.root, &rebind_args(&old, &new, true)).unwrap();
    assert_eq!(dry.status, RebindStatus::DryRun);
    assert!(clone.exists() && !moved.exists());
    assert_eq!(git(&clone, &["remote", "get-url", "origin"]), old.to_string_lossy());

    let done = repository::rebind(&f.root, &rebind_args(&old, &new, false)).unwrap();
    assert_eq!(done.status, RebindStatus::Moved);
    assert!(!clone.exists());
    assert_eq!(done.from.branches, done.to.branches);
    assert_eq!(done.to.branches[0].branch, "feature/issue-9");
    assert!(done.to.dirty);
    assert_eq!(std::fs::read_to_string(moved.join("scratch")).unwrap(), "dirty");
    assert_eq!(git(&moved, &["rev-parse", "feature/issue-9"]).len(), 40);
    assert_eq!(git(&moved, &["remote", "get-url", "origin"]), new.to_string_lossy());
    assert!(record_of(&f, NEW_SLUG).contains(&*new.to_string_lossy()));
    assert!(!f
        .root
        .join("work/.identity")
        .join(format!("{OLD_SLUG}.json"))
        .exists());

    // Re-running verifies and changes nothing.
    let again = repository::rebind(&f.root, &rebind_args(&old, &new, false)).unwrap();
    assert_eq!(again.status, RebindStatus::AlreadyBound);
    assert_eq!(again.to.branches, done.to.branches);
    // The moved clone prepares under the canonical identity (dirty refusal
    // proves it got past the identity check).
    let error = repository::prepare(&f.root, &f.additional_args(&new.to_string_lossy(), NEW_SLUG))
        .unwrap_err()
        .to_string();
    assert!(error.contains("dirty"), "{error}");
}

#[test]
fn rebind_succeeds_when_the_canonical_remote_already_publishes_a_local_branch() {
    let _fork = fork_guard();
    let (f, old, new, clone) = old_clone_with_work();
    // The transferred repository already carries the branch (published by
    // someone else, or pushed after the transfer): the move's `fetch --prune`
    // shifts the unpublished count without any local work changing.
    git(&clone, &["push", &new.to_string_lossy(), "feature/issue-9"]);
    let tip = git(&clone, &["rev-parse", "feature/issue-9"]);
    let done = repository::rebind(&f.root, &rebind_args(&old, &new, false)).unwrap();
    assert_eq!(done.status, RebindStatus::Moved);
    assert_eq!(done.from.branches[0].branch, "feature/issue-9");
    assert!(done.to.branches.is_empty(), "published upstream by now");
    let moved = f.root.join("work").join(NEW_SLUG);
    assert_eq!(git(&moved, &["rev-parse", "feature/issue-9"]), tip);
    assert_eq!(std::fs::read_to_string(moved.join("scratch")).unwrap(), "dirty");
    assert_eq!(git(&moved, &["remote", "get-url", "origin"]), new.to_string_lossy());
}

#[test]
fn rebind_finishes_an_interrupted_move() {
    let _fork = fork_guard();
    let (f, old, new, clone) = old_clone_with_work();
    // Crash after the directory move, before the record or origin changed.
    std::fs::rename(&clone, f.root.join("work").join(NEW_SLUG)).unwrap();
    let done = repository::rebind(&f.root, &rebind_args(&old, &new, false)).unwrap();
    assert_eq!(done.status, RebindStatus::AlreadyBound);
    let moved = f.root.join("work").join(NEW_SLUG);
    assert_eq!(git(&moved, &["remote", "get-url", "origin"]), new.to_string_lossy());
    assert!(record_of(&f, NEW_SLUG).contains(&*new.to_string_lossy()));
    assert!(!f
        .root
        .join("work/.identity")
        .join(format!("{OLD_SLUG}.json"))
        .exists());
    assert_eq!(std::fs::read_to_string(moved.join("scratch")).unwrap(), "dirty");
}

#[test]
fn rebind_refuses_a_wrong_repository_without_changing_anything() {
    let _fork = fork_guard();
    let (f, old, _new, clone) = old_clone_with_work();
    let unrelated = f.second_remote("unrelated");
    let before = record_of(&f, OLD_SLUG);
    let error = repository::rebind(&f.root, &rebind_args(&old, &unrelated, false))
        .unwrap_err()
        .to_string();
    assert!(error.contains("wrong repository"), "{error}");
    assert!(clone.exists() && !f.root.join("work").join(NEW_SLUG).exists());
    assert_eq!(git(&clone, &["remote", "get-url", "origin"]), old.to_string_lossy());
    assert_eq!(record_of(&f, OLD_SLUG), before);
    assert_eq!(std::fs::read_to_string(clone.join("scratch")).unwrap(), "dirty");
}

#[test]
fn rebind_reports_two_populated_clones_and_leaves_both_intact() {
    let _fork = fork_guard();
    let (f, old, new, clone) = old_clone_with_work();
    repository::prepare(&f.root, &f.additional_args(&new.to_string_lossy(), NEW_SLUG)).unwrap();
    let other = f.root.join("work").join(NEW_SLUG);
    std::fs::write(other.join("mine"), "also work").unwrap();
    let outcome = repository::rebind(&f.root, &rebind_args(&old, &new, false)).unwrap();
    assert_eq!(outcome.status, RebindStatus::BothPopulated);
    assert!(outcome.from.populated && outcome.to.populated);
    assert_eq!(std::fs::read_to_string(clone.join("scratch")).unwrap(), "dirty");
    assert_eq!(std::fs::read_to_string(other.join("mine")).unwrap(), "also work");
    assert_eq!(git(&clone, &["remote", "get-url", "origin"]), old.to_string_lossy());
    assert_eq!(git(&other, &["remote", "get-url", "origin"]), new.to_string_lossy());
}

#[test]
fn rebind_of_an_uncloned_registration_moves_nothing_and_a_foreign_dir_is_kept() {
    let _fork = fork_guard();
    let f = RepoFixture::new();
    f.prepare().unwrap();
    let (old, new) = (f.second_remote("old"), f.second_remote("new"));
    let outcome = repository::rebind(&f.root, &rebind_args(&old, &new, false)).unwrap();
    assert_eq!(outcome.status, RebindStatus::NotCloned);
    std::fs::create_dir_all(f.root.join("work").join(OLD_SLUG)).unwrap();
    let error = repository::rebind(&f.root, &rebind_args(&old, &new, false))
        .unwrap_err()
        .to_string();
    assert!(error.contains("unrecognized"), "{error}");
    let mut same = rebind_args(&old, &new, false);
    same.to_slug = OLD_SLUG.into();
    assert!(repository::rebind(&f.root, &same).is_err());
}

/// A linked worktree on a fresh branch `name` cut from the base, under the
/// volume root.
fn linked_worktree(f: &RepoFixture, name: &str) -> PathBuf {
    let repo = f.root.join("repo");
    let worktree = f.root.join(name);
    git(&repo, &["worktree", "add", "-b", name, worktree.to_str().unwrap()]);
    worktree
}

#[test]
#[serial_test::serial(private_workspace_fork)]
#[serial_test::serial]
fn untracked_tool_caches_do_not_refuse_preparation_but_other_dirt_does() {
    let _fork = fork_guard();
    let f = RepoFixture::new();
    f.prepare().unwrap();
    // The measured wedge (#14458): one untracked `.cache/` of tool caches in a
    // leftover worktree, nothing unpublished.
    let worktree = linked_worktree(&f, "issue-5084");
    std::fs::create_dir_all(worktree.join(".cache/actionlint")).unwrap();
    std::fs::write(worktree.join(".cache/actionlint/bin"), "cache").unwrap();
    f.prepare().unwrap();
    assert!(worktree.join(".cache/actionlint/bin").exists(), "forgiven, not removed");
    // Real dirt next to it still refuses, names the seat_dirty reason, the
    // path, and the listing command.
    std::fs::write(worktree.join("stray.txt"), "retain").unwrap();
    let error = f.prepare().unwrap_err().to_string();
    assert!(error.contains("seat_dirty"), "{error}");
    assert!(error.contains("stray.txt"), "{error}");
    assert!(!error.contains(".cache"), "{error}");
    assert!(error.contains("accounts session recover fixture"), "{error}");
    assert!(error.contains(worktree.to_str().unwrap()), "{error}");
}

#[test]
#[serial_test::serial(private_workspace_fork)]
#[serial_test::serial]
fn recover_lists_dirty_worktrees_with_counts_when_nothing_is_unpublished() {
    let _fork = fork_guard();
    let f = RepoFixture::new();
    f.prepare().unwrap();
    // An abandoned merge state: staged paths, no unpublished commits.
    let staged = linked_worktree(&f, "issue-5550");
    for i in 0..3 {
        std::fs::write(staged.join(format!("staged-{i}")), "x").unwrap();
    }
    git(&staged, &["add", "."]);
    let untracked = linked_worktree(&f, "issue-5084");
    std::fs::write(untracked.join("stray.txt"), "x").unwrap();
    std::fs::write(untracked.join("other.txt"), "x").unwrap();
    let clean = linked_worktree(&f, "issue-1");
    let out = repository::recover(&f.root, &recover_args(&f.remote)).unwrap();
    assert!(out.branches.is_empty() && out.detached.is_empty());
    let listed = |path: &Path| {
        out.dirty_worktrees
            .iter()
            .find(|d| d.path == path.display().to_string())
            .cloned()
    };
    let staged_entry = listed(&staged).expect("staged worktree listed");
    assert_eq!(
        (staged_entry.branch.as_deref(), staged_entry.tracked, staged_entry.untracked),
        (Some("issue-5550"), 3, 0)
    );
    assert_eq!(staged_entry.unpublished, 0);
    let untracked_entry = listed(&untracked).expect("untracked worktree listed");
    assert_eq!((untracked_entry.tracked, untracked_entry.untracked), (0, 2));
    assert!(listed(&clean).is_none(), "a clean worktree is not listed");
    assert!(listed(&f.root.join("repo")).is_none());
}

#[test]
#[serial_test::serial(private_workspace_fork)]
#[serial_test::serial]
fn discard_worktree_removes_only_the_named_dirty_worktree_and_keeps_its_branch() {
    let _fork = fork_guard();
    let f = RepoFixture::new();
    f.prepare().unwrap();
    let repo = f.root.join("repo");
    let named = linked_worktree(&f, "issue-5550");
    std::fs::write(named.join("stray.txt"), "x").unwrap();
    git(&named, &["add", "."]);
    let other = linked_worktree(&f, "issue-6000");
    std::fs::write(other.join("keep.txt"), "retain").unwrap();
    let mut args = recover_args(&f.remote);
    args.discard = true;
    args.worktree = vec![named.clone()];
    let out = repository::recover(&f.root, &args).unwrap();
    assert_eq!(out.removed_worktrees, vec![named.display().to_string()]);
    assert!(!named.exists());
    assert!(git_ok(&repo, &["show-ref", "--verify", "refs/heads/issue-5550"]), "branch kept");
    assert!(other.join("keep.txt").exists(), "an unnamed worktree is never touched");
    assert_eq!(out.dirty_worktrees.len(), 1);
    assert_eq!(out.dirty_worktrees[0].path, other.display().to_string());
    // The seat is unwedged only once the other leftover is dealt with too.
    assert!(f.prepare().unwrap_err().to_string().contains("seat_dirty"));
    args.worktree = vec![other.clone()];
    repository::recover(&f.root, &args).unwrap();
    f.prepare().unwrap();
}

#[test]
#[serial_test::serial(private_workspace_fork)]
#[serial_test::serial]
fn discard_worktree_refuses_unpublished_commits_main_checkout_and_unknown_paths() {
    let _fork = fork_guard();
    let f = RepoFixture::new();
    f.prepare().unwrap();
    let repo = f.root.join("repo");
    // Dirty with two... one unpublished commit on its branch (the #6016 shape).
    let work = linked_worktree(&f, "issue-6016");
    std::fs::write(work.join("fixture"), "committed").unwrap();
    git(&work, &["commit", "-am", "unpublished"]);
    std::fs::write(work.join("fixture"), "modified").unwrap();
    let mut args = recover_args(&f.remote);
    args.discard = true;
    args.worktree = vec![work.clone()];
    let error = repository::recover(&f.root, &args).unwrap_err().to_string();
    assert!(error.contains("unpublished"), "{error}");
    assert!(work.exists());
    assert_eq!(std::fs::read_to_string(work.join("fixture")).unwrap(), "modified");
    // The main checkout is never removable this way.
    args.worktree = vec![repo.clone()];
    let error = repository::recover(&f.root, &args).unwrap_err().to_string();
    assert!(error.contains("main checkout"), "{error}");
    assert!(repo.join(".git").is_dir());
    // A path that is no worktree of this clone is reported, not acted on.
    let stray = f.root.join("not-a-worktree");
    std::fs::create_dir(&stray).unwrap();
    args.worktree = vec![stray.clone()];
    let out = repository::recover(&f.root, &args).unwrap();
    assert_eq!(out.ignored, vec![stray.display().to_string()]);
    assert!(stray.exists());
    // Discard still needs some scope.
    args.worktree.clear();
    assert!(repository::recover(&f.root, &args).is_err());
    // Once published, the same worktree is removable and the work survives
    // on origin.
    args.discard = false;
    args.publish = true;
    repository::recover(&f.root, &args).unwrap();
    args.publish = false;
    args.discard = true;
    args.worktree = vec![work.clone()];
    let out = repository::recover(&f.root, &args).unwrap();
    assert_eq!(out.removed_worktrees.len(), 1);
    assert!(git_ok(&f.remote, &["show-ref", "--verify", "refs/heads/issue-6016"]));
}

#[test]
#[serial_test::serial(private_workspace_fork)]
#[serial_test::serial]
fn preparation_reaps_a_dirty_worktree_whose_branch_is_merged_into_the_base() {
    let _fork = fork_guard();
    let f = RepoFixture::new();
    f.prepare().unwrap();
    let repo = f.root.join("repo");
    // A branch with a commit, pushed and merged (fast-forwarded) into the
    // base on the remote: its worktree carries nothing the base lacks.
    let merged = linked_worktree(&f, "issue-5550");
    std::fs::write(merged.join("fixture"), "merged work").unwrap();
    git(&merged, &["commit", "-am", "work"]);
    git(&merged, &["push", "origin", "issue-5550"]);
    git(&f.remote, &["merge", "--ff-only", "issue-5550"]);
    // The base moves on after the merge, as a live one does.
    std::fs::write(f.remote.join("later"), "later").unwrap();
    git(&f.remote, &["add", "later"]);
    git(&f.remote, &["commit", "-m", "later"]);
    std::fs::write(merged.join("leftover"), "abandoned").unwrap();
    git(&merged, &["add", "leftover"]);
    // A dirty worktree cut from the old base, with no commits of its own, is
    // unsaved work, not a merged one — even though the base has moved past it.
    let fresh = linked_worktree(&f, "issue-7000");
    std::fs::write(fresh.join("wip.txt"), "wip").unwrap();
    // The refusal is for the fresh one only: the merged leftover is reaped.
    let error = f.prepare().unwrap_err().to_string();
    assert!(!merged.exists(), "merged leftover reaped");
    assert!(error.contains(fresh.to_str().unwrap()), "{error}");
    assert!(fresh.join("wip.txt").exists());
    assert!(git_ok(&repo, &["show-ref", "--verify", "refs/heads/issue-5550"]), "branch kept");
    std::fs::remove_file(fresh.join("wip.txt")).unwrap();
    f.prepare().unwrap();
}

#[test]
#[serial_test::serial(private_workspace_fork)]
#[serial_test::serial]
fn a_dirty_worktree_in_one_clone_does_not_refuse_the_seats_other_clone() {
    let _fork = fork_guard();
    let f = RepoFixture::new();
    f.prepare().unwrap();
    let additional = repository::PrepareArgs {
        account: "fixture".into(),
        repository: f.remote.to_string_lossy().into_owned(),
        base: "main".into(),
        container_id: "test-container".into(),
        branch: None,
        slug: Some("owner-repo-12345678".into()),
    };
    repository::prepare(&f.root, &additional).unwrap();
    let wedged = linked_worktree(&f, "issue-5084");
    std::fs::write(wedged.join("stray.txt"), "x").unwrap();
    assert!(f.prepare().unwrap_err().to_string().contains("seat_dirty"));
    // The other repository's clone prepares regardless (#14458).
    repository::prepare(&f.root, &additional).unwrap();
}
