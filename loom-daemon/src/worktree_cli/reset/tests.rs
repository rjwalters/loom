//! Unit tests for the race-safe stale-worktree reset (#8195 slice 6).
//!
//! # What is pinned here and what is pinned elsewhere
//!
//! These drive [`super::run`] end to end against throwaway repos, because that
//! is where the rung ORDER lives and the order is the contract: a worktree that
//! is both held by a live process and dirty must refuse for the liveness reason
//! and write no patch.
//!
//! `tests/worktree_reset_differential.rs` compares the same function against a
//! frozen copy of the retired shell on a generated corpus — that is the
//! equivalence evidence. `test-worktree-race-rescue.sh` (25 assertions,
//! unchanged from the shell implementation) drives the real binary through the
//! real shell entry point.
//!
//! Every case here is hermetic in the sense #8170 demanded: the only live
//! process any of them reasons about is one this test spawned, whose PID it
//! knows, inside a directory it just created.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

use super::*;

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// A scratch directory. `tag` may contain a space — several cases need one
/// (#7858's class).
fn tmpdir(tag: &str) -> PathBuf {
    let base = std::env::temp_dir().join(format!(
        "loom-reset-{tag}-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = fs::remove_dir_all(&base);
    fs::create_dir_all(&base).expect("create tmpdir");
    fs::canonicalize(&base).expect("canonicalize tmpdir")
}

fn git(repo: &Path, args: &[&str]) -> std::process::Output {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .expect("run git");
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    out
}

/// A one-commit repo at `<dir>/<name>`. `name` may contain spaces.
fn repo(dir: &Path, name: &str) -> PathBuf {
    let repo = dir.join(name);
    fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    git(&repo, &["config", "user.email", "t@t"]);
    git(&repo, &["config", "user.name", "t"]);
    fs::write(repo.join("tracked.txt"), "base content\n").unwrap();
    git(&repo, &["add", "tracked.txt"]);
    git(&repo, &["commit", "-q", "-m", "base"]);
    repo
}

fn head(repo: &Path) -> String {
    String::from_utf8_lossy(&git(repo, &["rev-parse", "HEAD"]).stdout)
        .trim()
        .to_string()
}

fn commit(repo: &Path, content: &str, msg: &str) -> String {
    fs::write(repo.join("tracked.txt"), content).unwrap();
    git(repo, &["add", "tracked.txt"]);
    git(repo, &["commit", "-q", "-m", msg]);
    head(repo)
}

fn opts(worktree: &Path, target_ref: &str, label: &str) -> Options {
    Options {
        worktree: worktree.to_path_buf(),
        target_ref: target_ref.to_string(),
        rescue_label: label.to_string(),
        // The tests' own process is excluded by the probe itself; nothing else
        // needs excluding unless a case says so.
        ignore_pids: Vec::new(),
    }
}

fn set_mode(path: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt as _;
    let mut perms = fs::metadata(path).unwrap().permissions();
    perms.set_mode(mode);
    fs::set_permissions(path, perms).unwrap();
}

fn patches(repo: &Path) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(repo.join(".snapshots")) else {
        return Vec::new();
    };
    let mut found: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "patch"))
        .collect();
    found.sort();
    found
}

/// A live process whose cwd is `dir`, killed on drop so a failing assertion
/// cannot leak a `sleep` onto a shared worker.
struct Holder(Child);

impl Holder {
    fn at(dir: &Path) -> Self {
        fs::create_dir_all(dir).unwrap();
        let child = Command::new("sleep")
            .arg("60")
            .current_dir(dir)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn a holder process");
        // The probe reads /proc/<pid>/cwd (or lsof); either way the child must
        // have been scheduled far enough to have one. Poll rather than sleep a
        // fixed amount.
        let holder = Self(child);
        for _ in 0..100 {
            if holder.is_visible(dir) {
                return holder;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        panic!("holder process never became visible to the cwd probe");
    }

    fn is_visible(&self, dir: &Path) -> bool {
        match safety::find_processes_with_cwd_in_directory(dir) {
            CwdProbe::Pids(pids) => pids.contains(&self.0.id()),
            // A host with neither /proc nor lsof cannot run the liveness cases
            // at all; `liveness_cases_are_runnable_on_this_host` skips them
            // rather than letting them assert nothing.
            CwdProbe::Unprobable => false,
        }
    }

    fn pid(&self) -> u32 {
        self.0.id()
    }
}

impl Drop for Holder {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Whether this host can see a process's cwd at all. Linux CI always can; a
/// host with neither `/proc` nor `lsof` makes the liveness rung untestable, and
/// a test that silently passed there would be worse than one that says so.
fn probe_available(dir: &Path) -> bool {
    safety::find_processes_with_cwd_in_directory(dir) != CwdProbe::Unprobable
}

// ---------------------------------------------------------------------------
// 1. The fast path
// ---------------------------------------------------------------------------

#[test]
fn a_clean_worktree_resets_and_writes_no_rescue_patch() {
    let dir = tmpdir("clean");
    let repo = repo(&dir, "repo");
    let base = head(&repo);
    let second = commit(&repo, "second\n", "second");
    git(&repo, &["reset", "-q", "--hard", &base]);

    assert_eq!(run(&opts(&repo, &second, "test-rescue")), 0);
    assert_eq!(head(&repo), second, "the reset must land on the target ref");
    assert!(patches(&repo).is_empty(), "a clean worktree has nothing to rescue");
}

#[test]
fn an_untracked_file_needs_no_rescue_and_survives_the_reset() {
    // `git reset --hard` never touches untracked files and this path never runs
    // `git clean`, so there is nothing for them to be rescued FROM. A patch
    // written here would be a false signal that work was nearly lost.
    let dir = tmpdir("untracked");
    let repo = repo(&dir, "repo");
    let base = head(&repo);
    let second = commit(&repo, "second\n", "second");
    git(&repo, &["reset", "-q", "--hard", &base]);
    fs::write(repo.join("scratch.txt"), "untracked scratch\n").unwrap();

    assert_eq!(run(&opts(&repo, &second, "test-rescue")), 0);
    assert_eq!(fs::read_to_string(repo.join("scratch.txt")).unwrap(), "untracked scratch\n");
    assert!(patches(&repo).is_empty());
}

// ---------------------------------------------------------------------------
// 2. The rescue
// ---------------------------------------------------------------------------

#[test]
fn foreign_tracked_changes_are_rescued_to_a_replayable_patch_before_the_reset() {
    let dir = tmpdir("rescue");
    let repo = repo(&dir, "repo");
    let base = head(&repo);
    fs::write(repo.join("tracked.txt"), "foreign edit\n").unwrap();

    assert_eq!(run(&opts(&repo, &base, "test-rescue")), 0);

    let found = patches(&repo);
    assert_eq!(found.len(), 1, "expected exactly one rescue patch: {found:?}");
    let patch = fs::read_to_string(&found[0]).unwrap();
    assert!(
        patch.contains("+foreign edit"),
        "the discarded content must be recoverable from the patch:\n{patch}"
    );
    assert_eq!(
        fs::read_to_string(repo.join("tracked.txt")).unwrap(),
        "base content\n",
        "the worktree must still reach the target ref's content"
    );

    // And the patch is a real `git apply` input, not merely non-empty.
    git(&repo, &["apply", found[0].to_str().unwrap()]);
    assert_eq!(fs::read_to_string(repo.join("tracked.txt")).unwrap(), "foreign edit\n");
}

#[test]
fn the_rescue_patch_is_named_from_the_label_and_a_utc_stamp() {
    let dir = tmpdir("label");
    let repo = repo(&dir, "repo");
    let base = head(&repo);
    fs::write(repo.join("tracked.txt"), "foreign\n").unwrap();

    assert_eq!(run(&opts(&repo, &base, "issue-42-stale-worktree-reset")), 0);

    let found = patches(&repo);
    let name = found[0].file_name().unwrap().to_string_lossy().to_string();
    assert!(
        name.starts_with("issue-42-stale-worktree-reset-"),
        "patch name must carry the caller's label: {name}"
    );
    let stamp = name
        .trim_start_matches("issue-42-stale-worktree-reset-")
        .trim_end_matches(".patch");
    assert_eq!(stamp.len(), 16, "date -u +%Y%m%dT%H%M%SZ is 16 chars: {stamp}");
    assert!(
        stamp.ends_with('Z') && stamp[8..9] == *"T",
        "stamp must keep the shell's format: {stamp}"
    );
}

#[test]
fn an_unwritable_rescue_directory_refuses_the_reset_and_keeps_the_foreign_work() {
    // The retained suite forces this by pre-creating a regular FILE at the
    // `.snapshots` path. Refusing here is the whole point: work that cannot be
    // captured must not be discarded.
    let dir = tmpdir("unwritable");
    let repo = repo(&dir, "repo");
    let base = head(&repo);
    fs::write(repo.join("tracked.txt"), "precious foreign work\n").unwrap();
    fs::write(repo.join(".snapshots"), "").unwrap();

    assert_eq!(run(&opts(&repo, &base, "test-rescue")), 1);
    assert_eq!(
        fs::read_to_string(repo.join("tracked.txt")).unwrap(),
        "precious foreign work\n",
        "the reset must never have been attempted"
    );
}

#[test]
fn an_empty_capture_is_deleted_rather_than_left_behind_as_a_false_rescue() {
    // `git diff HEAD` cannot answer "dirty" and then produce nothing, so this
    // pins the post-condition directly on the writer: an empty patch is not a
    // rescue, and the file must not survive to look like one.
    let dir = tmpdir("empty-capture");
    let repo = repo(&dir, "repo");
    let patch = repo.join(".snapshots").join("x.patch");
    fs::create_dir_all(patch.parent().unwrap()).unwrap();

    // A worktree git cannot read makes `git diff` fail after the file has
    // already been created — the same branch a write failure takes.
    assert!(!write_rescue_patch(Path::new("/nonexistent-worktree"), &patch));
    // …and it leaves an EMPTY file behind, which is precisely why `run`'s
    // `rm -f` (the shell's own) is load-bearing rather than defensive: without
    // it, `.snapshots/` would accumulate zero-byte files that look like
    // rescues. If this ever stops being true, the assertion below is what says
    // so rather than the deletion quietly becoming a no-op.
    assert!(patch.exists(), "the writer creates the file before git can fail");
    assert_eq!(fs::metadata(&patch).unwrap().len(), 0);
}

#[test]
fn a_refused_rescue_leaves_no_patch_behind_for_the_next_reader_to_trust() {
    // The end-to-end half of the test above: whatever the writer left, `run`
    // must not hand back a `.snapshots/` entry alongside a refusal. A stray
    // zero-byte patch would tell the next operator their work was captured.
    let dir = tmpdir("refused-no-patch");
    let repo = repo(&dir, "repo");
    let base = head(&repo);
    fs::write(repo.join("tracked.txt"), "precious foreign work\n").unwrap();
    let snapshots = repo.join(".snapshots");
    fs::create_dir_all(&snapshots).unwrap();
    // r-x: `create_dir_all` succeeds on the existing dir, the patch write does
    // not. Running as root would defeat this, so the case asserts the refusal
    // it is about only when the permission actually bites.
    set_mode(&snapshots, 0o500);
    let writable = fs::File::create(snapshots.join("probe")).is_ok();
    let code = run(&opts(&repo, &base, "test-rescue"));
    set_mode(&snapshots, 0o755);

    if writable {
        eprintln!("SKIP: this host ignores directory permissions (running as root?)");
        return;
    }
    assert_eq!(code, 1, "an uncapturable change must refuse the reset");
    assert!(
        patches(&repo).is_empty(),
        "a refusal must leave no patch behind: {:?}",
        patches(&repo)
    );
    assert_eq!(
        fs::read_to_string(repo.join("tracked.txt")).unwrap(),
        "precious foreign work\n",
        "the reset must never have been attempted"
    );
}

// ---------------------------------------------------------------------------
// 3. Refusals that protect committed work
// ---------------------------------------------------------------------------

#[test]
fn a_worktree_that_gained_commits_refuses_the_reset() {
    let dir = tmpdir("ahead");
    let repo = repo(&dir, "repo");
    let base = head(&repo);
    let raced = commit(&repo, "a real commit that landed in the race window\n", "raced");

    assert_eq!(run(&opts(&repo, &base, "test-rescue")), 1);
    assert_eq!(head(&repo), raced, "the raced commit must still be present");
    assert!(patches(&repo).is_empty());
}

#[test]
fn a_reset_onto_an_unresolvable_ref_reports_the_reset_failure_not_a_refusal() {
    // Exit 2 is reserved for "the reset itself failed", and a bad ref must not
    // be pre-empted by the commits-ahead rung (whose `rev-list` also fails on
    // it, and which reads that failure as 0 for exactly this reason).
    let dir = tmpdir("bad-ref");
    let repo = repo(&dir, "repo");
    let before = head(&repo);

    assert_eq!(run(&opts(&repo, "refs/heads/does-not-exist", "test-rescue")), 2);
    assert_eq!(head(&repo), before);
}

// ---------------------------------------------------------------------------
// 4. The liveness veto (#7463)
// ---------------------------------------------------------------------------

#[test]
fn a_live_process_holding_the_worktree_refuses_the_reset() {
    let dir = tmpdir("live");
    let repo = repo(&dir, "repo");
    if !probe_available(&repo) {
        eprintln!("SKIP: no /proc and no lsof on this host");
        return;
    }
    let base = head(&repo);
    let second = commit(&repo, "second\n", "second");
    git(&repo, &["reset", "-q", "--hard", &base]);

    let _holder = Holder::at(&repo);
    assert_eq!(run(&opts(&repo, &second, "test-rescue")), 1);
    assert_eq!(head(&repo), base, "the reset must never have been attempted");
}

#[test]
fn a_live_process_nested_several_levels_deep_is_still_detected() {
    // The `+d`-vs-`+D` gap (#7468): a holder one level down is matched even by
    // a non-recursive scan, so only depth >= 2 distinguishes them. Three levels
    // is the shape of real work (`loom-daemon/src/worktree_ops/`).
    let dir = tmpdir("nested");
    let repo = repo(&dir, "repo");
    if !probe_available(&repo) {
        eprintln!("SKIP: no /proc and no lsof on this host");
        return;
    }
    let base = head(&repo);
    let second = commit(&repo, "second\n", "second");
    git(&repo, &["reset", "-q", "--hard", &base]);

    let _holder = Holder::at(&repo.join("nested").join("two").join("three"));
    assert_eq!(run(&opts(&repo, &second, "test-rescue")), 1);
    assert_eq!(head(&repo), base);
}

#[test]
fn the_liveness_veto_runs_before_any_git_check_and_writes_no_patch() {
    // Rung order, stated as a property: a worktree that is BOTH held and dirty
    // must refuse for the liveness reason and leave `.snapshots` absent. If the
    // rungs were reordered, a patch would appear here.
    let dir = tmpdir("order");
    let repo = repo(&dir, "repo");
    if !probe_available(&repo) {
        eprintln!("SKIP: no /proc and no lsof on this host");
        return;
    }
    let base = head(&repo);
    fs::write(repo.join("tracked.txt"), "in-flight edits\n").unwrap();

    let _holder = Holder::at(&repo);
    assert_eq!(run(&opts(&repo, &base, "test-rescue")), 1);
    assert!(
        !repo.join(".snapshots").exists(),
        "the liveness rung must refuse before the rescue rung runs"
    );
    assert_eq!(fs::read_to_string(repo.join("tracked.txt")).unwrap(), "in-flight edits\n");
}

#[test]
fn an_ignored_pid_is_not_treated_as_a_foreign_holder() {
    // The caller's own shell sits in the worktree in the retained suite. Without
    // this exclusion the harness would veto every one of its own reset cases —
    // and the port would look correct while testing nothing.
    let dir = tmpdir("ignore-pid");
    let repo = repo(&dir, "repo");
    if !probe_available(&repo) {
        eprintln!("SKIP: no /proc and no lsof on this host");
        return;
    }
    let base = head(&repo);
    let second = commit(&repo, "second\n", "second");
    git(&repo, &["reset", "-q", "--hard", &base]);

    let holder = Holder::at(&repo);
    let mut o = opts(&repo, &second, "test-rescue");
    o.ignore_pids = vec![holder.pid()];

    assert_eq!(run(&o), 0, "an ignored holder must not veto the reset");
    assert_eq!(head(&repo), second);
}

#[test]
fn ignoring_one_pid_does_not_ignore_a_different_live_holder() {
    // The mutation this pairs with: an `ignore_pids` that swallowed every PID
    // would pass the test above and silently delete the whole veto.
    let dir = tmpdir("ignore-pid-narrow");
    let repo = repo(&dir, "repo");
    if !probe_available(&repo) {
        eprintln!("SKIP: no /proc and no lsof on this host");
        return;
    }
    let base = head(&repo);
    let second = commit(&repo, "second\n", "second");
    git(&repo, &["reset", "-q", "--hard", &base]);

    let ignored = Holder::at(&repo.join("ignored-holder"));
    let _other = Holder::at(&repo.join("other-holder"));
    let mut o = opts(&repo, &second, "test-rescue");
    o.ignore_pids = vec![ignored.pid()];

    assert_eq!(run(&o), 1);
    assert_eq!(head(&repo), base);
}

// ---------------------------------------------------------------------------
// 5. Paths containing spaces (#7858's class — the issue's own criterion)
// ---------------------------------------------------------------------------

#[test]
fn a_worktree_path_containing_spaces_is_rescued_and_reset_whole() {
    // #7858 was an unquoted path that turned a guard into an `rm -rf` on a live
    // worktree. Nothing here can word-split a path — it is a `PathBuf` handed
    // whole to `Command::arg` — but the criterion asks for the case to exist in
    // the port's own tests rather than only in the retained suite, because a
    // future refactor that reintroduced string interpolation would otherwise
    // have nothing standing in its way.
    let dir = tmpdir("spaces");
    let repo = repo(&dir, "a repo with spaces");
    let base = head(&repo);
    fs::write(repo.join("tracked.txt"), "foreign edit\n").unwrap();

    assert_eq!(run(&opts(&repo, &base, "label with spaces")), 0);

    let found = patches(&repo);
    assert_eq!(found.len(), 1, "expected one patch: {found:?}");
    assert!(found[0]
        .parent()
        .unwrap()
        .to_string_lossy()
        .contains("a repo with spaces"));
    assert!(found[0]
        .file_name()
        .unwrap()
        .to_string_lossy()
        .starts_with("label with spaces-"));
    assert!(fs::read_to_string(&found[0])
        .unwrap()
        .contains("+foreign edit"));
    assert_eq!(fs::read_to_string(repo.join("tracked.txt")).unwrap(), "base content\n");
}

#[test]
fn a_space_bearing_path_is_reported_whole_in_a_refusal() {
    // The other half of the same class: a message truncated at the first space
    // is how an operator ends up running `rm -rf` on the wrong directory.
    let dir = tmpdir("spaces-refusal");
    let repo = repo(&dir, "a repo with spaces");
    commit(&repo, "raced\n", "raced");

    // Captured through the same formatting the refusal uses, since `run` prints
    // to the process's real stderr.
    let shown = format!("{}", repo.display());
    assert!(shown.contains("a repo with spaces"));
    assert_eq!(run(&opts(&repo, "HEAD~1", "test-rescue")), 1);
}

#[test]
fn a_live_holder_under_a_space_bearing_path_is_still_detected() {
    let dir = tmpdir("spaces-live");
    let repo = repo(&dir, "a repo with spaces");
    if !probe_available(&repo) {
        eprintln!("SKIP: no /proc and no lsof on this host");
        return;
    }
    let base = head(&repo);
    let second = commit(&repo, "second\n", "second");
    git(&repo, &["reset", "-q", "--hard", &base]);

    let _holder = Holder::at(&repo.join("dir with spaces"));
    assert_eq!(run(&opts(&repo, &second, "test-rescue")), 1);
    assert_eq!(head(&repo), base);
}

// ---------------------------------------------------------------------------
// 6. Helpers
// ---------------------------------------------------------------------------

#[test]
fn an_unenterable_worktree_has_no_live_holder_and_no_probe_message() {
    // `cd "$worktree_path" || return 1` — nothing to protect, and the caller's
    // own `git -C` reports the real problem.
    assert!(!has_live_process(Path::new("/nonexistent-worktree"), &[]));
}

#[test]
fn utc_stamp_matches_dates_own_format_at_a_known_instant() {
    let epoch = std::time::UNIX_EPOCH;
    assert_eq!(utc_stamp(epoch), "19700101T000000Z");
    // 2026-09-26T06:33:39Z — the shape a real rescue writes.
    let t = epoch + std::time::Duration::from_secs(1_790_404_419);
    assert_eq!(utc_stamp(t), "20260926T063339Z");
    // A leap day, because the civil-date arithmetic is hand-rolled.
    let leap = epoch + std::time::Duration::from_secs(1_709_164_800);
    assert_eq!(utc_stamp(leap), "20240229T000000Z");
}

#[test]
fn git_stdout_is_none_on_failure_so_the_caller_can_apply_its_own_default() {
    let dir = tmpdir("git-stdout");
    let repo = repo(&dir, "repo");
    assert_eq!(git_stdout(&repo, &["rev-list", "--count", "HEAD..HEAD"]).as_deref(), Some("0"));
    assert!(git_stdout(&repo, &["rev-list", "--count", "nope..HEAD"]).is_none());
}

#[test]
fn git_status_code_reports_an_unrunnable_git_as_greater_than_one() {
    // >1 is the "could not determine" reading the diff rung refuses on, so a
    // git that cannot run must never look like a clean tree.
    assert!(git_status_code(Path::new("/nonexistent-worktree"), &["diff", "HEAD", "--quiet"]) > 1);
}
