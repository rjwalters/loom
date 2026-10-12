//! Real-git tests for the ff-abort classifier added by #11539: every dirty
//! tracked file already identical to its `origin/<branch>` version.
//!
//! Temp dirs only: a bare `origin`, a `seed` clone standing for everyone else
//! pushing to it, and a `host` clone that is the daemon checkout under test.
//! `classify_and_report` is called directly, after a real `merge --ff-only`
//! has refused, so each case is the exact state the classifier sees in
//! production.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use tempfile::TempDir;

use super::*;

const BRANCH: &str = "main";
const ACCOUNTS: &str = ".loom/accounts.json";

fn git_in(dir: &Path, args: &[&str]) -> std::process::Output {
    Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .output()
        .unwrap()
}

fn git(dir: &Path, args: &[&str]) -> String {
    let out = git_in(dir, args);
    assert!(
        out.status.success(),
        "git {args:?} in {}: {}",
        dir.display(),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn write(path: &Path, contents: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, contents).unwrap();
}

fn read(path: &Path) -> String {
    fs::read_to_string(path).unwrap()
}

fn identify(repo: &Path) {
    git(repo, &["config", "user.name", "Test"]);
    git(repo, &["config", "user.email", "test@example.invalid"]);
    git(repo, &["config", "commit.gpgsign", "false"]);
}

struct Fixture {
    _tmp: TempDir,
    seed: PathBuf,
    host: PathBuf,
}

impl Fixture {
    /// origin and host both at a first commit holding two unmanaged files.
    fn new() -> Self {
        let tmp = TempDir::new().unwrap();
        let base = fs::canonicalize(tmp.path()).unwrap();
        let origin = base.join("origin.git");
        fs::create_dir_all(&origin).unwrap();
        git(
            &origin,
            &[
                "init",
                "--bare",
                "--quiet",
                &format!("--initial-branch={BRANCH}"),
            ],
        );
        git(&base, &["clone", "--quiet", origin.to_str().unwrap(), "seed"]);
        let seed = base.join("seed");
        identify(&seed);
        write(&seed.join(ACCOUNTS), "{\"seats\":[]}\n");
        write(&seed.join("README.md"), "one\n");
        git(&seed, &["add", "-A"]);
        git(&seed, &["commit", "--quiet", "-m", "first"]);
        git(&seed, &["push", "--quiet", "origin", &format!("HEAD:{BRANCH}")]);
        git(&base, &["clone", "--quiet", origin.to_str().unwrap(), "host"]);
        let host = base.join("host");
        identify(&host);
        Fixture {
            _tmp: tmp,
            seed,
            host,
        }
    }

    /// Someone else lands new content for `files` upstream; the host fetches.
    fn push(&self, files: &[(&str, &str)]) {
        for (path, contents) in files {
            write(&self.seed.join(path), contents);
        }
        git(&self.seed, &["add", "-A"]);
        git(&self.seed, &["commit", "--quiet", "-m", "upstream change"]);
        git(&self.seed, &["push", "--quiet", "origin", &format!("HEAD:{BRANCH}")]);
        git(&self.host, &["fetch", "--quiet", "--", "origin", BRANCH]);
    }

    fn head(&self) -> String {
        git(&self.host, &["rev-parse", "HEAD"])
    }

    fn origin_tip(&self) -> String {
        git(&self.host, &["rev-parse", &format!("origin/{BRANCH}")])
    }

    fn status(&self) -> String {
        git(&self.host, &["status", "--porcelain"])
    }

    /// The real ff the caller tried must refuse, or the case is not testing
    /// the abort path at all.
    fn assert_ff_refuses(&self) {
        let out =
            git_in(&self.host, &["merge", "--ff-only", &format!("origin/{BRANCH}"), "--quiet"]);
        assert!(!out.status.success(), "fixture bug: the fast-forward applied");
    }

    fn classify(&self, auto_resolve: bool) -> (bool, SyncState) {
        let argv: Vec<String> = if auto_resolve {
            vec!["--auto-resolve-safe-abort".to_string()]
        } else {
            Vec::new()
        };
        let args = Args::parse(&argv);
        let mut state = SyncState::new();
        let ok = classify_and_report(&self.host, BRANCH, &args, &mut state);
        (ok, state)
    }
}

const NEW_ACCOUNTS: &str = "{\"seats\":[\"joseph-2am\"]}\n";

#[test]
fn ff_abort_identical_dirty_file_resolves_and_fast_forwards() {
    let fx = Fixture::new();
    fx.push(&[(ACCOUNTS, NEW_ACCOUNTS)]);
    write(&fx.host.join(ACCOUNTS), NEW_ACCOUNTS);
    fx.assert_ff_refuses();

    let (ok, state) = fx.classify(true);
    assert!(ok);
    assert!(state.ff_synced);
    assert_eq!(fx.head(), fx.origin_tip());
    assert_eq!(fx.status(), "");
    assert_eq!(read(&fx.host.join(ACCOUNTS)), NEW_ACCOUNTS);
}

#[test]
fn ff_abort_identical_without_flag_only_names_the_command() {
    let fx = Fixture::new();
    fx.push(&[(ACCOUNTS, NEW_ACCOUNTS)]);
    write(&fx.host.join(ACCOUNTS), NEW_ACCOUNTS);
    fx.assert_ff_refuses();
    let before = fx.head();

    let (ok, state) = fx.classify(false);
    assert!(!ok);
    assert!(!state.ff_synced);
    assert_eq!(fx.head(), before);
    assert_eq!(fx.status(), format!("M {ACCOUNTS}"));
    assert_eq!(
        all_dirty_tracked_identical_to_origin(&fx.host, BRANCH),
        Some(vec![ACCOUNTS.to_string()])
    );
}

#[test]
fn ff_abort_identical_differing_file_hard_aborts() {
    let fx = Fixture::new();
    fx.push(&[(ACCOUNTS, NEW_ACCOUNTS)]);
    let local = "{\"seats\":[\"something-else\"]}\n";
    write(&fx.host.join(ACCOUNTS), local);
    fx.assert_ff_refuses();
    let before = fx.head();

    let (ok, _) = fx.classify(true);
    assert!(!ok);
    assert_eq!(fx.head(), before);
    assert_eq!(read(&fx.host.join(ACCOUNTS)), local);
    assert_eq!(all_dirty_tracked_identical_to_origin(&fx.host, BRANCH), None);
}

#[test]
fn ff_abort_identical_mix_of_identical_and_differing_hard_aborts() {
    let fx = Fixture::new();
    fx.push(&[(ACCOUNTS, NEW_ACCOUNTS), ("README.md", "two\n")]);
    write(&fx.host.join(ACCOUNTS), NEW_ACCOUNTS);
    write(&fx.host.join("README.md"), "mine\n");
    fx.assert_ff_refuses();
    let before = fx.head();

    let (ok, _) = fx.classify(true);
    assert!(!ok);
    assert_eq!(fx.head(), before);
    assert_eq!(read(&fx.host.join(ACCOUNTS)), NEW_ACCOUNTS);
    assert_eq!(read(&fx.host.join("README.md")), "mine\n");
}

#[test]
fn ff_abort_identical_staged_copy_of_origin_resolves_and_fast_forwards() {
    let fx = Fixture::new();
    fx.push(&[(ACCOUNTS, NEW_ACCOUNTS), ("README.md", "two\n")]);
    // README.md: index == origin and working tree == origin, so its staged
    // content is recoverable from origin and the relaxation applies. git's
    // own ff accepts an index entry already equal to the target, so an
    // unstaged identical ACCOUNTS is what makes the ff refuse here.
    write(&fx.host.join("README.md"), "two\n");
    git(&fx.host, &["add", "README.md"]);
    write(&fx.host.join(ACCOUNTS), NEW_ACCOUNTS);
    fx.assert_ff_refuses();

    let (ok, state) = fx.classify(true);
    assert!(ok);
    assert!(state.ff_synced);
    assert_eq!(fx.head(), fx.origin_tip());
    assert_eq!(fx.status(), "");
    assert_eq!(read(&fx.host.join(ACCOUNTS)), NEW_ACCOUNTS);
    assert_eq!(read(&fx.host.join("README.md")), "two\n");
}

#[test]
fn ff_abort_identical_staged_but_different_hard_aborts() {
    let fx = Fixture::new();
    fx.push(&[(ACCOUNTS, NEW_ACCOUNTS)]);
    // Stage a DIFFERENT content, then put origin's content in the working
    // tree: the working-tree diff against origin is empty, the index's is not.
    let staged = "{\"seats\":[\"staged-work\"]}\n";
    write(&fx.host.join(ACCOUNTS), staged);
    git(&fx.host, &["add", ACCOUNTS]);
    write(&fx.host.join(ACCOUNTS), NEW_ACCOUNTS);
    fx.assert_ff_refuses();
    let before = fx.head();

    let (ok, _) = fx.classify(true);
    assert!(!ok);
    assert_eq!(fx.head(), before);
    assert_eq!(read(&fx.host.join(ACCOUNTS)), NEW_ACCOUNTS);
    assert_eq!(git(&fx.host, &["show", &format!(":{ACCOUNTS}")]), staged.trim());
}

#[test]
fn ff_abort_identical_diverged_is_not_this_class() {
    let fx = Fixture::new();
    fx.push(&[(ACCOUNTS, NEW_ACCOUNTS)]);
    // A local-only commit makes local diverge from origin.
    write(&fx.host.join("local.txt"), "local\n");
    git(&fx.host, &["add", "local.txt"]);
    git(&fx.host, &["commit", "--quiet", "-m", "local only"]);
    write(&fx.host.join(ACCOUNTS), NEW_ACCOUNTS);
    fx.assert_ff_refuses();
    let before = fx.head();

    assert_eq!(all_dirty_tracked_identical_to_origin(&fx.host, BRANCH), None);
    let (ok, _) = fx.classify(true);
    assert!(!ok);
    assert_eq!(fx.head(), before);
    assert_eq!(read(&fx.host.join(ACCOUNTS)), NEW_ACCOUNTS);
}

impl Fixture {
    /// `paths` land upstream holding `"one\n"` and the host fast-forwards to
    /// them, so each is in HEAD before the change under test.
    fn in_head(&self, paths: &[&str]) {
        let files: Vec<(&str, &str)> = paths.iter().map(|p| (*p, "one\n")).collect();
        self.push(&files);
        git(&self.host, &["merge", "--ff-only", "--quiet", &format!("origin/{BRANCH}")]);
    }
}

#[test]
fn ff_abort_identical_non_ascii_path_resolves() {
    let fx = Fixture::new();
    fx.in_head(&["é.txt"]);
    fx.push(&[("é.txt", "two\n")]);
    write(&fx.host.join("é.txt"), "two\n");
    fx.assert_ff_refuses();

    assert_eq!(
        all_dirty_tracked_identical_to_origin(&fx.host, BRANCH),
        Some(vec!["é.txt".to_string()])
    );
    let (ok, state) = fx.classify(true);
    assert!(ok);
    assert!(state.ff_synced);
    assert_eq!(fx.head(), fx.origin_tip());
    assert_eq!(fx.status(), "");
}

#[test]
fn ff_abort_identical_non_ascii_path_that_differs_hard_aborts() {
    let fx = Fixture::new();
    fx.in_head(&["é.txt"]);
    fx.push(&[("é.txt", "two\n")]);
    write(&fx.host.join("é.txt"), "mine\n");
    fx.assert_ff_refuses();
    let before = fx.head();

    assert_eq!(all_dirty_tracked_identical_to_origin(&fx.host, BRANCH), None);
    let (ok, _) = fx.classify(true);
    assert!(!ok);
    assert_eq!(fx.head(), before);
    assert_eq!(read(&fx.host.join("é.txt")), "mine\n");
}

#[test]
fn ff_abort_identical_glob_path_does_not_vouch_for_its_lookalike() {
    let fx = Fixture::new();
    fx.in_head(&["a[1].txt", "a1.txt"]);
    fx.push(&[("a[1].txt", "two\n")]);
    write(&fx.host.join("a[1].txt"), "two\n");
    write(&fx.host.join("a1.txt"), "mine\n");
    fx.assert_ff_refuses();
    let before = fx.head();

    assert_eq!(all_dirty_tracked_identical_to_origin(&fx.host, BRANCH), None);
    let (ok, _) = fx.classify(true);
    assert!(!ok);
    assert_eq!(fx.head(), before);
    assert_eq!(read(&fx.host.join("a1.txt")), "mine\n", "a1.txt is untouched");
    assert_eq!(read(&fx.host.join("a[1].txt")), "two\n");
}

#[test]
fn ff_abort_identical_path_with_spaces_resolves() {
    let fx = Fixture::new();
    fx.in_head(&["with space.txt"]);
    fx.push(&[("with space.txt", "two\n")]);
    write(&fx.host.join("with space.txt"), "two\n");
    fx.assert_ff_refuses();

    let (ok, state) = fx.classify(true);
    assert!(ok);
    assert!(state.ff_synced);
    assert_eq!(fx.head(), fx.origin_tip());
    assert_eq!(read(&fx.host.join("with space.txt")), "two\n");
}

#[test]
fn ff_abort_identical_rename_hard_aborts() {
    // The new name is not in HEAD, so `checkout HEAD --` could not restore
    // it: never vouched for.
    let fx = Fixture::new();
    fx.push(&[("README.md", "two\n")]);
    git(&fx.host, &["mv", "README.md", "moved.md"]);
    fx.assert_ff_refuses();
    let before = fx.head();

    assert_eq!(all_dirty_tracked_identical_to_origin(&fx.host, BRANCH), None);
    let (ok, _) = fx.classify(true);
    assert!(!ok);
    assert_eq!(fx.head(), before);
    assert_eq!(read(&fx.host.join("moved.md")), "one\n");
}

// ── The filesystem behind a vouched-for path (#11558 P1) ────────────────

impl Fixture {
    /// Someone else deletes `path` upstream and lands `extra`; the host
    /// fetches. The host holds `path` in HEAD.
    fn delete_upstream(&self, path: &str) {
        git(&self.seed, &["rm", "--quiet", "--", path]);
        write(&self.seed.join(ACCOUNTS), NEW_ACCOUNTS);
        git(&self.seed, &["add", "-A"]);
        git(&self.seed, &["commit", "--quiet", "-m", &format!("delete {path}")]);
        git(&self.seed, &["push", "--quiet", "origin", &format!("HEAD:{BRANCH}")]);
        git(&self.host, &["fetch", "--quiet", "--", "origin", BRANCH]);
        // An identical unstaged ACCOUNTS is what makes the real ff refuse.
        write(&self.host.join(ACCOUNTS), NEW_ACCOUNTS);
    }
}

#[test]
fn ff_abort_identical_deleted_path_now_a_directory_hard_aborts() {
    let fx = Fixture::new();
    fx.in_head(&["victim"]);
    fx.delete_upstream("victim");
    fs::remove_file(fx.host.join("victim")).unwrap();
    write(&fx.host.join("victim/local-only.txt"), "precious\n");
    write(&fx.host.join("victim/build.log"), "ignored\n");
    write(&fx.host.join(".git/info/exclude"), "*.log\n");
    fx.assert_ff_refuses();
    let before = fx.head();
    let index = git(&fx.host, &["ls-files", "-s"]);

    assert_eq!(all_dirty_tracked_identical_to_origin(&fx.host, BRANCH), None);
    let (ok, state) = fx.classify(true);
    assert!(!ok);
    assert!(!state.ff_synced);
    assert_eq!(fx.head(), before);
    assert_eq!(git(&fx.host, &["ls-files", "-s"]), index);
    assert_eq!(read(&fx.host.join("victim/local-only.txt")), "precious\n");
    assert_eq!(read(&fx.host.join("victim/build.log")), "ignored\n");
}

#[test]
fn ff_abort_identical_untracked_file_on_an_unstaged_deletion_hard_aborts() {
    let fx = Fixture::new();
    fx.in_head(&["victim"]);
    fx.delete_upstream("victim");
    git(&fx.host, &["rm", "--quiet", "--cached", "--", "victim"]);
    write(&fx.host.join("victim"), "precious\n");
    fx.assert_ff_refuses();
    let before = fx.head();

    assert_eq!(all_dirty_tracked_identical_to_origin(&fx.host, BRANCH), None);
    let (ok, _) = fx.classify(true);
    assert!(!ok);
    assert_eq!(fx.head(), before);
    assert_eq!(read(&fx.host.join("victim")), "precious\n");
}

#[cfg(unix)]
#[test]
fn ff_abort_identical_file_to_symlink_type_change_hard_aborts() {
    let fx = Fixture::new();
    fx.push(&[(ACCOUNTS, NEW_ACCOUNTS), ("README.md", "two\n")]);
    write(&fx.host.join(ACCOUNTS), NEW_ACCOUNTS);
    write(&fx.host.join("elsewhere.txt"), "two\n");
    fs::remove_file(fx.host.join("README.md")).unwrap();
    std::os::unix::fs::symlink("elsewhere.txt", fx.host.join("README.md")).unwrap();
    fx.assert_ff_refuses();
    let before = fx.head();

    assert_eq!(all_dirty_tracked_identical_to_origin(&fx.host, BRANCH), None);
    let (ok, _) = fx.classify(true);
    assert!(!ok);
    assert_eq!(fx.head(), before);
    let meta = fs::symlink_metadata(fx.host.join("README.md")).unwrap();
    assert!(meta.file_type().is_symlink(), "the symlink is left alone");
}

#[test]
fn ff_abort_identical_plain_deletion_resolves() {
    let fx = Fixture::new();
    fx.in_head(&["victim"]);
    fx.delete_upstream("victim");
    fs::remove_file(fx.host.join("victim")).unwrap();
    fx.assert_ff_refuses();

    let (ok, state) = fx.classify(true);
    assert!(ok);
    assert!(state.ff_synced);
    assert_eq!(fx.head(), fx.origin_tip());
    assert_eq!(fx.status(), "");
    assert!(!fx.host.join("victim").exists());
}

// ── Raw bytes, never the filtered view (#11558 third pass) ──────────────

impl Fixture {
    /// A lossy clean filter `strip` (drops `OUTPUT` lines) on `path`, set in
    /// the host's own config and `.git/info/attributes`.
    fn lossy_filter(&self, path: &str) {
        git(&self.host, &["config", "filter.strip.clean", "grep -v '^OUTPUT' || true"]);
        git(&self.host, &["config", "filter.strip.smudge", "cat"]);
        write(&self.host.join(".git/info/attributes"), &format!("{path} filter=strip\n"));
    }
}

#[test]
fn ff_abort_identical_lossy_clean_filter_hard_aborts() {
    let fx = Fixture::new();
    fx.in_head(&["f.nb"]);
    fx.push(&[("f.nb", "two\n")]);
    fx.lossy_filter("f.nb");
    write(&fx.host.join("f.nb"), "two\nOUTPUT precious\n");
    fx.assert_ff_refuses();
    let before = fx.head();

    assert_eq!(all_dirty_tracked_identical_to_origin(&fx.host, BRANCH), None);
    let (ok, state) = fx.classify(true);
    assert!(!ok);
    assert!(!state.ff_synced);
    assert_eq!(fx.head(), before);
    assert_eq!(read(&fx.host.join("f.nb")), "two\nOUTPUT precious\n");
}

#[test]
fn ff_abort_identical_raw_identical_under_a_filter_resolves() {
    let fx = Fixture::new();
    fx.in_head(&["f.nb"]);
    fx.push(&[("f.nb", "two\n")]);
    fx.lossy_filter("f.nb");
    write(&fx.host.join("f.nb"), "two\n");
    fx.assert_ff_refuses();

    let (ok, state) = fx.classify(true);
    assert!(ok);
    assert!(state.ff_synced);
    assert_eq!(fx.head(), fx.origin_tip());
    assert_eq!(read(&fx.host.join("f.nb")), "two\n");
}

#[test]
fn ff_abort_identical_crlf_only_difference_hard_aborts() {
    let fx = Fixture::new();
    fx.in_head(&["t.txt"]);
    fx.push(&[("t.txt", "two\n")]);
    write(&fx.host.join(".git/info/attributes"), "t.txt text=auto\n");
    write(&fx.host.join("t.txt"), "two\r\n");
    fx.assert_ff_refuses();
    let before = fx.head();

    assert_eq!(all_dirty_tracked_identical_to_origin(&fx.host, BRANCH), None);
    let (ok, _) = fx.classify(true);
    assert!(!ok);
    assert_eq!(fx.head(), before);
    assert_eq!(fs::read(fx.host.join("t.txt")).unwrap(), b"two\r\n");
}
