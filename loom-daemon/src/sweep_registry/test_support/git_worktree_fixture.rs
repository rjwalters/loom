//! The `git`-backed worktree fixture behind every mid-build-death watchdog
//! test (Issue #3895), split out of `test_support.rs` so its hermeticity
//! rationale (Issue #8170) can live next to the code it explains.

use std::path::{Path, PathBuf};
use std::process::Command;

/// Repo-local `.git/config` stanza appended to every fixture worktree
/// [`make_dirty_git_worktree`] builds, so the **code under test's** own git
/// invocations — `worktree_dirty`'s `git status --porcelain
/// --untracked-files=all`, `clean_worktree`'s `git reset --hard` / `git clean
/// -fd`, `log_worktree_discard`'s `git status`/`git diff` — are insulated
/// from whatever the host developer has in `~/.gitconfig` (Issue #8170).
///
/// Those production calls are plain `Command::new("git")` with no
/// `GIT_CONFIG_*` overrides (correctly: in production they run against a real
/// worktree in a real checkout, where the user's config is *supposed* to
/// apply), so a `GIT_CONFIG_GLOBAL=/dev/null` on the fixture's own commands
/// would not reach them. Repo-local config does, because git reads
/// `<repo>/.git/config` on every invocation in that repo.
///
/// Each setting neutralises a real host config that silently changes what the
/// mid-build watchdog's refuse-to-destroy tests exercise:
///
/// * `core.excludesFile` — a global ignore file matching `dirty.txt` would
///   make `git status --untracked-files=all` report the worktree **clean**,
///   collapsing `midbuild_watchdog_once`'s decision to `Healthy`, so every
///   "recovery must fire" assertion fails and every `assert_midbuild_refused`
///   positive control stops recording its refusal — with no hint that the
///   host, not the code, changed the answer.
/// * `commit.gpgsign` / `tag.gpgsign` — `true` (common on a signing host)
///   makes the fixture's `git commit` shell out to `gpg`/`gpg-agent`, a
///   host-global, single-instance dependency that turns a locked or absent
///   key into a hard fixture panic.
/// * `core.autocrlf`, `core.fsmonitor`, `gc.auto` — line-ending rewrites, an
///   external fsmonitor daemon, and background `git gc` respectively; all
///   introduce host-dependent timing or content into a two-file repo that
///   needs none of them.
const FIXTURE_GIT_LOCAL_CONFIG: &str = "\
[core]\n\
\texcludesFile = /dev/null\n\
\tautocrlf = false\n\
\tfsmonitor = false\n\
[commit]\n\
\tgpgsign = false\n\
[tag]\n\
\tgpgsign = false\n\
[gc]\n\
\tauto = 0\n";

/// Create a git repo at `.loom/worktrees/issue-<N>` with one commit plus an
/// untracked file, so `worktree_dirty` reports it dirty. Returns the path.
///
/// Hermetic with respect to the host's git configuration (Issue #8170): the
/// fixture's own commands run with `GIT_CONFIG_GLOBAL`/`GIT_CONFIG_SYSTEM`
/// pointed at `/dev/null`, and [`FIXTURE_GIT_LOCAL_CONFIG`] is written into
/// the repo so the production git calls under test are insulated too. The
/// post-condition below then proves the worktree really does read dirty to
/// the *exact* probe `SweepRegistry::worktree_dirty` runs, rather than
/// leaving a host-config surprise to surface as an unrelated-looking
/// assertion failure several layers up.
pub(crate) fn make_dirty_git_worktree(ws: &Path, issue: u32) -> PathBuf {
    let wt = ws
        .join(".loom")
        .join("worktrees")
        .join(format!("issue-{issue}"));
    std::fs::create_dir_all(&wt).unwrap();
    let git = |args: &[&str]| {
        let out = Command::new("git")
            .arg("-C")
            .arg(&wt)
            .args(args)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            // Ignore ~/.gitconfig and /etc/gitconfig for the fixture's own
            // commands (#8170) — `init.templateDir` hooks, `commit.gpgsign`,
            // `core.excludesFile` and friends are host state these tests must
            // not depend on. `GIT_CONFIG_NOSYSTEM` is the portable belt to
            // `GIT_CONFIG_SYSTEM`'s braces (the latter needs git >= 2.32).
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    };
    git(&["init", "-q"]);
    // Written straight into `.git/config` rather than via N `git config`
    // subprocesses: same effect, and every subprocess this fixture does not
    // spawn is one less child exposed to the shared-process env-mutation race
    // documented in `loom-daemon/src/lib.rs` (#4385).
    {
        use std::io::Write as _;
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(wt.join(".git").join("config"))
            .unwrap();
        f.write_all(FIXTURE_GIT_LOCAL_CONFIG.as_bytes()).unwrap();
        f.sync_all().unwrap();
    }
    std::fs::write(wt.join("committed.txt"), "base\n").unwrap();
    git(&["add", "-A"]);
    git(&["commit", "-q", "-m", "base"]);
    // Now dirty it with an untracked file (mimics mid-build edits).
    std::fs::write(wt.join("dirty.txt"), "uncommitted mid-build edit\n").unwrap();

    // Post-condition (#8170): run the production probe's EXACT command, with
    // no `GIT_CONFIG_*` overrides of its own, so this asserts what
    // `SweepRegistry::worktree_dirty` will actually see. Every mid-build
    // watchdog test's subject — recover, or refuse to destroy — is gated on
    // this answer being "dirty"; a silent "clean" makes the whole module look
    // like it is failing at something it never got to test.
    let probe = Command::new("git")
        .arg("-C")
        .arg(&wt)
        .args(["status", "--porcelain", "--untracked-files=all"])
        .output()
        .unwrap();
    assert!(
        probe.status.success() && !probe.stdout.iter().all(u8::is_ascii_whitespace),
        "fixture post-condition failed: {} must read DIRTY to `git status --porcelain \
         --untracked-files=all` (the exact probe `SweepRegistry::worktree_dirty` runs), but the \
         probe {}.\n  stdout: {:?}\n  stderr: {:?}\nTwo known causes: (a) host git config still \
         reaching this repo (see FIXTURE_GIT_LOCAL_CONFIG, #8170); (b) this binary was run under \
         plain `cargo test`, whose shared-process harness lets an unrelated test's \
         `env::set_var` tear a concurrent `Command::spawn`'s environ — run `cargo nextest run` \
         instead (see `loom-daemon/src/lib.rs` and `.config/nextest.toml`, #4385).",
        wt.display(),
        if probe.status.success() {
            "reported no changes"
        } else {
            "failed"
        },
        String::from_utf8_lossy(&probe.stdout),
        String::from_utf8_lossy(&probe.stderr),
    );
    wt
}
