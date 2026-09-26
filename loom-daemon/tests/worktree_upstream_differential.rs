//! Differential harness for #8195 slice 9 — `loom-daemon worktree-upstream`
//! against a frozen copy of the two `worktree.sh` blocks it retired.
//!
//! # What is compared
//!
//! Per scenario, on two independently materialised but **byte-identical**
//! repositories:
//!
//! | dimension | why it is here |
//! |---|---|
//! | exit code | the contract is "0, always" — a divergence here means one side found a way to fail |
//! | stdout | the whole observable product of this block is messages; `git branch --set-upstream-to`'s own confirmation line lands here too, and it is *not* `--quiet`-gated |
//! | stderr | must stay empty on both sides — every git call redirects its own stderr away |
//! | `branch.<b>.remote` / `.merge` | the only configuration this block writes; it is the actual repair |
//! | `HEAD` | the report is warn-only. If either side ever moves HEAD, that is the #6257 remediation being *run* instead of printed |
//! | `git status --porcelain` | nothing here may touch the working tree — the retained suite's "uncommitted WIP was NOT destroyed" assertion, made structural |
//!
//! # Why the two trees are proven identical first
//!
//! Both sides are built from the same [`Scenario`], but "the same generator
//! ran twice" is not the same claim as "the same bytes". Git object ids make
//! that checkable: every commit is made with a pinned identity and a pinned
//! timestamp, so two correct materialisations of one scenario have **the same
//! SHAs**. [`assert_trees_identical`] compares the full ref list, HEAD, and
//! the tracking config *before* either implementation runs, so a generator
//! that quietly diverged fails with its own message rather than presenting
//! itself as a behaviour difference.
//!
//! # Path normalisation, and the one place it is load-bearing
//!
//! The two trees necessarily live at different roots, so each side's own root
//! is rewritten to `<ROOT>` before the stdout comparison. That rewrite is what
//! makes the **space-bearing path** cases meaningful rather than vacuous: the
//! `registered-worktree` arm interpolates `$WORKTREE_PATH` straight into three
//! remediation hints, unquoted, and a root like `<tmp>/my repo/wt` is exactly
//! the #7858 shape. The normaliser matches on the root string, not on
//! whitespace, so a side that word-split the path would fail to match and be
//! reported.
//!
//! # The one pinned divergence
//!
//! Exactly one input is expected **not** to agree:
//! [`a_backslash_in_the_path_is_re_escaped_by_the_shell_and_not_by_the_port`].
//! The retired `print_*` helpers are `echo -e`, which re-interprets backslash
//! escapes in the *interpolated path*, so the shell emitted an unpasteable
//! remediation hint naming a directory that does not exist. That case lives in
//! its own test — with the port named as the correct side, so a regression to
//! the shell's answer fails rather than quietly re-agreeing — rather than in
//! the shared corpus the agreement tests walk.
//!
//! # Cost
//!
//! The corpus is 86 scenarios, each costing two full repository
//! materialisations; it is built **once per test binary** (see [`runs`]) and
//! fanned out across threads, because six tests ask different questions of the
//! same runs.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

// ---------------------------------------------------------------------------
// The corpus
// ---------------------------------------------------------------------------

/// Which retired block a scenario drives.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Arm {
    /// `worktree.sh`'s branch-reuse arm (#6095/#6100): repo is the main
    /// workspace, HEAD is on the default branch.
    LocalBranch,
    /// `worktree.sh`'s registered-worktree fast path (#6257/#6291): repo is
    /// the worktree, HEAD is on the feature branch.
    RegisteredWorktree,
}

impl Arm {
    fn as_flag(self) -> &'static str {
        match self {
            Arm::LocalBranch => "local-branch",
            Arm::RegisteredWorktree => "registered-worktree",
        }
    }
}

/// The repository shapes the two blocks branch on. Each is a deterministic
/// recipe, not a captured directory — see the module docs.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum State {
    /// Branch pushed; local tracking wrongly points at `origin/main`. The
    /// #6086/PR #6093 incident shape.
    UpstreamWrong,
    /// Branch pushed; no `branch.<b>.merge` at all (a plain `git push`
    /// without `-u`).
    UpstreamUnset,
    /// Branch pushed and correctly tracking. The no-op case the retained
    /// suite pins negatively ("did NOT print a correction message").
    UpstreamCorrect,
    /// Local branch exists and was never pushed — no `origin/<branch>` ref.
    /// Nothing may run, and above all no upstream may be fabricated.
    NeverPushed,
    /// HEAD is a strict ancestor of `origin/<branch>`, upstream also wrong.
    /// The #5609 drift shape.
    BehindTipUpstreamWrong,
    /// HEAD strictly behind, upstream already correct — isolates the drift
    /// report from the correction.
    BehindTipUpstreamCorrect,
    /// One unpushed local commit: ahead, not behind. The retained suite's
    /// false-positive guard.
    AheadOfTip,
    /// Both sides moved: neither is an ancestor of the other. Also not drift.
    Diverged,
    /// HEAD equals the pushed tip, upstream correct. The common case.
    Synced,
    /// `origin/<branch>` exists but there is no local branch of that name, so
    /// `rev-parse <b>@{u}` fails and `--set-upstream-to` fails after it. Not
    /// reachable from either live call site; here because "what does this do
    /// when git refuses" is precisely where a hand-written port drifts.
    NoLocalBranch,
}

#[derive(Clone, Debug)]
struct Scenario {
    name: String,
    arm: Arm,
    state: State,
    /// Directory name for the repo. Some contain a space (#7858's class).
    dir: String,
    /// Branch name. Some contain `'` or `;` — legal in a refname, and both
    /// are metacharacters the retired shell interpolated unquoted into
    /// messages.
    branch: String,
    /// `--json` on the shell side, `--quiet` on the port's.
    json: bool,
    /// The caller's pre-fetch `git status --porcelain` verdict.
    uncommitted: bool,
    issue: String,
}

fn corpus() -> Vec<Scenario> {
    use State::*;
    let mut out = Vec::new();
    let mut push = |arm: Arm,
                    state: State,
                    dir: &str,
                    branch: &str,
                    json: bool,
                    uncommitted: bool,
                    issue: &str| {
        out.push(Scenario {
            name: format!(
                "{}/{state:?}/{}/{}{}{}",
                arm.as_flag(),
                dir.replace(' ', "_SP_"),
                branch.replace('/', "_"),
                if json { "/json" } else { "" },
                if uncommitted { "/dirty" } else { "" },
            ),
            arm,
            state,
            dir: dir.to_string(),
            branch: branch.to_string(),
            json,
            uncommitted,
            issue: issue.to_string(),
        });
    };

    // --- the branch-reuse arm, across every state and both output modes ----
    for state in [
        UpstreamWrong,
        UpstreamUnset,
        UpstreamCorrect,
        NeverPushed,
        BehindTipUpstreamWrong,
        AheadOfTip,
        Diverged,
        Synced,
        NoLocalBranch,
    ] {
        for json in [false, true] {
            push(Arm::LocalBranch, state, "plain", "feature/issue-901", json, false, "901");
        }
    }

    // --- the registered-worktree arm, same states, both output modes -------
    //     and both `--uncommitted` values wherever the drift report can fire,
    //     since that flag selects between a one-line and a four-line hint.
    for state in [
        UpstreamWrong,
        UpstreamUnset,
        UpstreamCorrect,
        NeverPushed,
        BehindTipUpstreamWrong,
        BehindTipUpstreamCorrect,
        AheadOfTip,
        Diverged,
        Synced,
        NoLocalBranch,
    ] {
        for json in [false, true] {
            for uncommitted in [false, true] {
                push(
                    Arm::RegisteredWorktree,
                    state,
                    "plain",
                    "feature/issue-902",
                    json,
                    uncommitted,
                    "902",
                );
            }
        }
    }

    // --- #7858's class: a repo path containing a space. On the
    //     registered-worktree arm that path is interpolated into three hint
    //     lines, unquoted, so a drift-reporting state is the one that matters.
    for state in [BehindTipUpstreamWrong, UpstreamWrong, Synced] {
        for uncommitted in [false, true] {
            push(
                Arm::RegisteredWorktree,
                state,
                "my repo dir",
                "feature/issue-903",
                false,
                uncommitted,
                "903",
            );
        }
        push(Arm::LocalBranch, state, "my repo dir", "feature/issue-903", false, false, "903");
    }

    // --- refname metacharacters the shell interpolated unquoted -----------
    //     No space in any of them: `git check-ref-format` rejects spaces, so a
    //     name containing one is not a reachable input and a scenario using one
    //     would fail at `git checkout -b` rather than at the comparison. The
    //     injection shape is intact without it.
    for branch in [
        "feature/issue-904;echo-pwned",
        "feature/issue-904'q",
        "feature/i$904",
    ] {
        for arm in [Arm::LocalBranch, Arm::RegisteredWorktree] {
            for state in [UpstreamWrong, BehindTipUpstreamWrong, Synced] {
                push(arm, state, "plain", branch, false, true, "904");
            }
        }
    }

    // --- an issue "number" that is not one. `worktree.sh` validates it long
    //     before here, but the value reaches a message body, and a port that
    //     formats it as an integer would only be caught by a case like this.
    push(
        Arm::RegisteredWorktree,
        BehindTipUpstreamWrong,
        "plain",
        "feature/issue-905",
        false,
        true,
        "905-beta",
    );

    out
}

// ---------------------------------------------------------------------------
// Materialisation
// ---------------------------------------------------------------------------

/// Pinned identity + timestamps, so two materialisations of one scenario
/// produce identical object ids. Without this the trees would differ by
/// commit SHA and every stdout comparison that quotes one would fail for the
/// wrong reason.
fn deterministic_env(cmd: &mut Command) -> &mut Command {
    cmd.env("GIT_AUTHOR_NAME", "Loom Diff")
        .env("GIT_AUTHOR_EMAIL", "diff@example.invalid")
        .env("GIT_COMMITTER_NAME", "Loom Diff")
        .env("GIT_COMMITTER_EMAIL", "diff@example.invalid")
        .env("GIT_AUTHOR_DATE", "2026-01-01T00:00:00+0000")
        .env("GIT_COMMITTER_DATE", "2026-01-01T00:00:00+0000")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("LC_ALL", "C")
}

fn git_ok(dir: &Path, args: &[&str]) {
    let mut cmd = Command::new("git");
    cmd.arg("-C").arg(dir).args(args);
    let out = deterministic_env(&mut cmd).output().expect("spawn git");
    assert!(
        out.status.success(),
        "git -C {dir:?} {args:?} failed:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn git_out(dir: &Path, args: &[&str]) -> String {
    let mut cmd = Command::new("git");
    cmd.arg("-C").arg(dir).args(args);
    let out = deterministic_env(&mut cmd).output().expect("spawn git");
    String::from_utf8_lossy(&out.stdout).trim_end().to_string()
}

fn write(path: &Path, body: &str) {
    fs::write(path, body).expect("write fixture file");
}

/// Build one scenario's repository under `root`, returning the directory the
/// implementation under test will be pointed at.
///
/// The shape is always the same skeleton — a bare `origin`, a clone with
/// `main`, and a feature branch — mutated per [`State`]. `git push` is used
/// rather than hand-written refs so `origin/<branch>` is a genuine
/// remote-tracking ref with genuine reachability, which is what
/// `merge-base --is-ancestor` is actually being asked about.
fn materialize(root: &Path, s: &Scenario) -> PathBuf {
    let base = root.join(&s.dir);
    fs::create_dir_all(&base).expect("create scenario dir");
    let origin = base.join("origin.git");
    let repo = base.join("work");

    let mut init = Command::new("git");
    init.args(["init", "-q", "-b", "main", "--bare"])
        .arg(&origin);
    assert!(deterministic_env(&mut init)
        .status()
        .expect("git init")
        .success());

    let mut init2 = Command::new("git");
    init2.args(["init", "-q", "-b", "main"]).arg(&repo);
    assert!(deterministic_env(&mut init2)
        .status()
        .expect("git init")
        .success());

    git_ok(&repo, &["config", "user.name", "Loom Diff"]);
    git_ok(&repo, &["config", "user.email", "diff@example.invalid"]);
    git_ok(
        &repo,
        &[
            "remote",
            "add",
            "origin",
            origin.to_str().expect("utf-8 origin"),
        ],
    );

    write(&repo.join("base.txt"), "base\n");
    git_ok(&repo, &["add", "base.txt"]);
    git_ok(&repo, &["commit", "-q", "-m", "base"]);
    git_ok(&repo, &["push", "-q", "origin", "main"]);

    let branch = s.branch.as_str();
    git_ok(&repo, &["checkout", "-q", "-b", branch]);
    write(&repo.join("work.txt"), "branch work\n");
    git_ok(&repo, &["add", "work.txt"]);
    git_ok(&repo, &["commit", "-q", "-m", "branch work"]);

    let tracking = format!("origin/{branch}");

    match s.state {
        State::NeverPushed => {
            // No push at all: `origin/<branch>` never exists.
        }
        State::BehindTipUpstreamWrong | State::BehindTipUpstreamCorrect => {
            // Push one commit further than the local checkout keeps, then
            // move the local branch back so HEAD is a strict ancestor of the
            // pushed tip.
            write(&repo.join("pushed.txt"), "only on origin\n");
            git_ok(&repo, &["add", "pushed.txt"]);
            git_ok(&repo, &["commit", "-q", "-m", "pushed ahead"]);
            git_ok(&repo, &["push", "-q", "origin", branch]);
            git_ok(&repo, &["reset", "-q", "--hard", "HEAD~1"]);
        }
        State::AheadOfTip => {
            git_ok(&repo, &["push", "-q", "origin", branch]);
            write(&repo.join("local.txt"), "unpushed\n");
            git_ok(&repo, &["add", "local.txt"]);
            git_ok(&repo, &["commit", "-q", "-m", "unpushed local work"]);
        }
        State::Diverged => {
            git_ok(&repo, &["push", "-q", "origin", branch]);
            write(&repo.join("pushed.txt"), "only on origin\n");
            git_ok(&repo, &["add", "pushed.txt"]);
            git_ok(&repo, &["commit", "-q", "-m", "pushed ahead"]);
            git_ok(&repo, &["push", "-q", "origin", branch]);
            git_ok(&repo, &["reset", "-q", "--hard", "HEAD~1"]);
            write(&repo.join("local.txt"), "unpushed\n");
            git_ok(&repo, &["add", "local.txt"]);
            git_ok(&repo, &["commit", "-q", "-m", "divergent local work"]);
        }
        State::NoLocalBranch => {
            git_ok(&repo, &["push", "-q", "origin", branch]);
            git_ok(&repo, &["checkout", "-q", "main"]);
            git_ok(&repo, &["branch", "-q", "-D", branch]);
        }
        _ => {
            git_ok(&repo, &["push", "-q", "origin", branch]);
        }
    }

    // Tracking config, applied after the history is settled.
    if s.state != State::NoLocalBranch {
        match s.state {
            State::UpstreamWrong | State::BehindTipUpstreamWrong => {
                git_ok(&repo, &["branch", "-q", "--set-upstream-to=origin/main", branch]);
            }
            State::UpstreamUnset | State::NeverPushed => {
                // `git push` without `-u` leaves no upstream; nothing to do.
            }
            _ => {
                git_ok(
                    &repo,
                    &[
                        "branch",
                        "-q",
                        &format!("--set-upstream-to={tracking}"),
                        branch,
                    ],
                );
            }
        }
    }

    // HEAD placement mirrors the live call sites: the branch-reuse arm runs
    // with the MAIN workspace as cwd (the feature branch is not checked out
    // anywhere yet); the fast path runs inside the worktree, on the branch.
    if s.arm == Arm::LocalBranch && s.state != State::NoLocalBranch {
        git_ok(&repo, &["checkout", "-q", "main"]);
    }

    // The port must never touch the working tree, so give it one to damage.
    if s.uncommitted {
        write(&repo.join("work.txt"), "locally modified, must survive\n");
        write(&repo.join("untracked.txt"), "untracked, must survive\n");
    }

    repo
}

// ---------------------------------------------------------------------------
// Observation
// ---------------------------------------------------------------------------

/// Everything both sides must agree on *before* running: proof the two trees
/// really are the same repository.
fn tree_signature(repo: &Path, branch: &str) -> BTreeMap<String, String> {
    let mut sig = BTreeMap::new();
    sig.insert(
        "refs".into(),
        git_out(repo, &["for-each-ref", "--format=%(refname) %(objectname)"]),
    );
    sig.insert("head".into(), git_out(repo, &["rev-parse", "HEAD"]));
    sig.insert("status".into(), git_out(repo, &["status", "--porcelain"]));
    sig.extend(tracking_signature(repo, branch));
    sig
}

/// The configuration this block exists to write.
fn tracking_signature(repo: &Path, branch: &str) -> BTreeMap<String, String> {
    let mut sig = BTreeMap::new();
    sig.insert(
        "branch.remote".into(),
        git_out(repo, &["config", "--get", &format!("branch.{branch}.remote")]),
    );
    sig.insert(
        "branch.merge".into(),
        git_out(repo, &["config", "--get", &format!("branch.{branch}.merge")]),
    );
    sig
}

/// What must be unchanged by a warn-only check.
fn aftermath(repo: &Path, branch: &str) -> BTreeMap<String, String> {
    let mut sig = tracking_signature(repo, branch);
    sig.insert("head".into(), git_out(repo, &["rev-parse", "HEAD"]));
    sig.insert("status".into(), git_out(repo, &["status", "--porcelain"]));
    sig
}

fn assert_trees_identical(a: &Path, b: &Path, s: &Scenario) {
    let sa = tree_signature(a, &s.branch);
    let sb = tree_signature(b, &s.branch);
    assert_eq!(
        sa, sb,
        "[{}] the two materialisations are NOT the same repository — the \
         generator diverged, so no behaviour conclusion can be drawn",
        s.name
    );
}

// ---------------------------------------------------------------------------
// Running the two sides
// ---------------------------------------------------------------------------

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("loom-daemon has a parent")
        .to_path_buf()
}

fn fixture_path() -> PathBuf {
    repo_root().join("loom-daemon/tests/fixtures/worktree-upstream-retired.sh")
}

fn run_shell(repo: &Path, s: &Scenario) -> (i32, String, String) {
    let script = fixture_path();
    assert!(script.exists(), "frozen fixture missing at {script:?}");
    let mut cmd = Command::new("bash");
    cmd.arg(&script)
        .arg(s.arm.as_flag())
        .arg(repo)
        .arg(&s.branch)
        .arg(if s.json { "true" } else { "false" })
        .arg(&s.issue)
        .arg(if s.uncommitted { " M work.txt" } else { "" });
    let out = deterministic_env(&mut cmd).output().expect("bash");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

fn run_rust(repo: &Path, s: &Scenario) -> (i32, String, String) {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_loom-daemon"));
    cmd.arg("worktree-upstream")
        .arg("--arm")
        .arg(s.arm.as_flag())
        .arg("--repo")
        .arg(repo)
        .arg("--branch")
        .arg(&s.branch)
        .arg("--issue")
        .arg(&s.issue);
    if s.json {
        cmd.arg("--quiet");
    }
    if s.uncommitted {
        cmd.arg("--uncommitted");
    }
    let out = deterministic_env(&mut cmd).output().expect("loom-daemon");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// Rewrite this side's own tree root to `<ROOT>`. See the module docs for why
/// this is what makes the space-bearing cases meaningful rather than vacuous.
fn normalize(text: &str, root: &Path) -> String {
    let mut s = text.to_string();
    for spelling in [
        root.to_string_lossy().into_owned(),
        format!("{}", root.display()),
    ] {
        if !spelling.is_empty() {
            s = s.replace(&spelling, "<ROOT>");
        }
    }
    s
}

fn tmproot(tag: &str) -> PathBuf {
    let base = std::env::temp_dir().join(format!(
        "loom-upstream-diff-{tag}-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = fs::remove_dir_all(&base);
    fs::create_dir_all(&base).expect("create tmproot");
    fs::canonicalize(&base).expect("canonicalize tmproot")
}

struct Comparison {
    shell: (i32, String, String),
    rust: (i32, String, String),
    shell_after: BTreeMap<String, String>,
    rust_after: BTreeMap<String, String>,
}

/// Materialise the scenario twice, prove the two trees identical, then run one
/// implementation against each.
fn compare(s: &Scenario) -> Comparison {
    let root = tmproot(&sanitize(&s.name));
    let shell_root = root.join("shell");
    let rust_root = root.join("rust");
    fs::create_dir_all(&shell_root).expect("shell root");
    fs::create_dir_all(&rust_root).expect("rust root");

    let shell_repo = materialize(&shell_root, s);
    let rust_repo = materialize(&rust_root, s);
    assert_trees_identical(&shell_repo, &rust_repo, s);

    let mut shell = run_shell(&shell_repo, s);
    let mut rust = run_rust(&rust_repo, s);
    shell.1 = normalize(&shell.1, &shell_root);
    shell.2 = normalize(&shell.2, &shell_root);
    rust.1 = normalize(&rust.1, &rust_root);
    rust.2 = normalize(&rust.2, &rust_root);

    Comparison {
        shell_after: aftermath(&shell_repo, &s.branch),
        rust_after: aftermath(&rust_repo, &s.branch),
        shell,
        rust,
    }
}

fn sanitize(name: &str) -> String {
    name.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

/// The whole corpus, materialised and run **exactly once** per test binary.
///
/// Each scenario costs two full `git init` + commit + push sequences and two
/// child processes, and six of the tests below walk the same corpus asking
/// different questions of the same runs. Re-materialising it per test made the
/// binary take six times as long to reach the identical verdict — and
/// "the same corpus" would then have meant six different sets of temporary
/// repositories, which is weaker, not stronger.
///
/// `OnceLock` rather than a single mega-test so a failure still names which
/// **dimension** diverged (stdout vs. exit code vs. the repository aftermath);
/// that is the first thing a reader of a red build needs, and a combined test
/// would report only the first assertion to trip.
fn runs() -> &'static [(Scenario, Comparison)] {
    static RUNS: std::sync::OnceLock<Vec<(Scenario, Comparison)>> = std::sync::OnceLock::new();
    RUNS.get_or_init(|| {
        // Scenarios are independent — each owns a private temporary directory
        // keyed on its own name, and neither implementation reads anything
        // outside it — so they are fanned out across threads. Results are
        // collected per chunk and re-joined in corpus order, so a failure
        // message names the same scenario on every host regardless of how the
        // work was divided.
        let all = corpus();
        let threads = std::thread::available_parallelism().map_or(4, |n| n.get().clamp(1, 8));
        let chunk = all.len().div_ceil(threads).max(1);
        let mut out: Vec<(Scenario, Comparison)> = Vec::with_capacity(all.len());
        std::thread::scope(|scope| {
            let handles: Vec<_> = all
                .chunks(chunk)
                .map(|slice| {
                    scope.spawn(move || {
                        slice
                            .iter()
                            .map(|s| (s.clone(), compare(s)))
                            .collect::<Vec<_>>()
                    })
                })
                .collect();
            for h in handles {
                out.extend(h.join().expect("scenario worker panicked"));
            }
        });
        assert_eq!(out.len(), all.len(), "lost scenarios while fanning out");
        out
    })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
fn the_corpus_is_not_empty_and_covers_both_arms() {
    // A guard on the guards: every test below is a `for` loop, and every one of
    // them passes vacuously over an empty or single-armed corpus.
    let all = runs();
    assert!(
        all.len() >= 80,
        "corpus shrank to {} scenarios (86 at the time of the port)",
        all.len()
    );
    for arm in [Arm::LocalBranch, Arm::RegisteredWorktree] {
        assert!(all.iter().any(|(s, _)| s.arm == arm), "no {arm:?} scenarios in the corpus");
    }
    assert!(
        all.iter().any(|(s, _)| s.dir.contains(' ')),
        "no space-bearing path in the corpus — #7858's class is not covered"
    );
    assert!(
        all.iter().any(|(_, c)| c.rust.1.contains("may be stale")),
        "no scenario produced a drift report — the corpus agrees on no-ops only"
    );
    assert!(
        all.iter().any(|(_, c)| c.rust.1.contains("correcting to")),
        "no scenario produced an upstream correction"
    );
}

#[test]
fn exit_codes_agree() {
    for (s, c) in runs() {
        assert_eq!(c.shell.0, c.rust.0, "[{}] exit code", s.name);
        assert_eq!(c.rust.0, 0, "[{}] the contract is exit 0, always", s.name);
    }
}

#[test]
fn stdout_agrees() {
    for (s, c) in runs() {
        assert_eq!(
            c.shell.1, c.rust.1,
            "[{}] stdout\n--- shell ---\n{}\n--- rust ---\n{}",
            s.name, c.shell.1, c.rust.1
        );
    }
}

#[test]
fn stderr_agrees_and_stays_empty() {
    for (s, c) in runs() {
        assert_eq!(c.shell.2, c.rust.2, "[{}] stderr", s.name);
        assert_eq!(
            c.rust.2, "",
            "[{}] the port wrote to stderr; every git call in this block \
             redirects its own stderr away, so nothing should reach it",
            s.name
        );
    }
}

#[test]
fn repository_aftermath_agrees() {
    for (s, c) in runs() {
        assert_eq!(
            c.shell_after, c.rust_after,
            "[{}] tracking config / HEAD / working tree after the run",
            s.name
        );
    }
}

/// The #6095/#6100 repair itself, stated positively and independently of the
/// shell: after a run over a branch whose upstream was wrong, the branch
/// tracks its **own** remote branch.
#[test]
fn a_wrong_upstream_is_actually_repaired() {
    let mut seen = 0;
    for (s, c) in runs()
        .iter()
        .filter(|(s, _)| matches!(s.state, State::UpstreamWrong | State::BehindTipUpstreamWrong))
    {
        seen += 1;
        assert_eq!(
            c.rust_after.get("branch.merge").map(String::as_str),
            Some(format!("refs/heads/{}", s.branch).as_str()),
            "[{}] branch.merge was not corrected to the branch's own remote ref",
            s.name
        );
        assert_eq!(
            c.rust_after.get("branch.remote").map(String::as_str),
            Some("origin"),
            "[{}] branch.remote was not corrected",
            s.name
        );
    }
    assert!(seen > 0, "no wrong-upstream scenarios — this test is vacuous");
}

/// The retained suite's Test 4, made structural: a branch that was never
/// pushed must come out with **no** upstream — not a fabricated one.
#[test]
fn a_never_pushed_branch_is_left_alone() {
    let mut seen = 0;
    for (s, c) in runs().iter().filter(|(s, _)| s.state == State::NeverPushed) {
        seen += 1;
        assert_eq!(
            c.rust_after.get("branch.merge").map(String::as_str),
            Some(""),
            "[{}] an upstream was fabricated for a branch with no origin ref",
            s.name
        );
        assert_eq!(c.rust.1, "", "[{}] a never-pushed branch produced output", s.name);
    }
    assert!(seen > 0, "no never-pushed scenarios — this test is vacuous");
}

/// Every dimension must be able to go RED. Without this the four `assert_eq!`
/// tests above could all be comparing constants and nobody would know.
#[test]
fn the_comparison_can_actually_fail() {
    let s = Scenario {
        name: "canary".into(),
        arm: Arm::RegisteredWorktree,
        state: State::BehindTipUpstreamWrong,
        dir: "my repo dir".into(),
        branch: "feature/issue-999".into(),
        json: false,
        uncommitted: true,
        issue: "999".into(),
    };
    let c = compare(&s);

    // The scenario is one the block actually acts on — otherwise "they agree"
    // would be the trivial agreement of two no-ops.
    assert!(
        c.rust.1.contains("may be stale"),
        "canary scenario produced no drift report; it is no longer exercising the thing"
    );
    assert!(
        c.rust.1.contains("<ROOT>"),
        "canary scenario's hints did not quote the worktree path, so the \
         space-bearing normalisation is not being exercised"
    );
    assert_eq!(c.shell.1, c.rust.1, "canary must agree before it can be perturbed");

    // stdout: a perturbed copy must not compare equal.
    let perturbed = c.rust.1.replacen("may be stale", "may be fine", 1);
    assert_ne!(c.shell.1, perturbed, "the stdout comparison cannot fail");

    // tracking config: a perturbed copy must not compare equal.
    let mut perturbed_after = c.rust_after.clone();
    perturbed_after.insert("branch.merge".into(), "refs/heads/something-else".into());
    assert_ne!(c.shell_after, perturbed_after, "the aftermath comparison cannot fail");

    // exit code: the contract is 0, and a different number must not match.
    assert_eq!(c.rust.0, 0, "the contract is exit 0, always");
    assert_ne!(c.shell.0, 1, "the exit-code comparison cannot fail");

    // The working tree survived, and the report did not run its own remedy.
    assert_eq!(
        c.rust_after.get("head"),
        c.shell_after.get("head"),
        "HEAD moved on one side — the drift report is warn-only"
    );
    assert!(
        c.rust_after
            .get("status")
            .is_some_and(|s| s.contains("work.txt")),
        "the uncommitted change did not survive the run"
    );
}

// ---------------------------------------------------------------------------
// The one pinned divergence
// ---------------------------------------------------------------------------

/// A worktree path containing a **backslash escape sequence** is the one input
/// on which the two implementations do not and must not agree — and the port is
/// the correct side.
///
/// `worktree.sh`'s `print_info`/`print_warning` are `echo -e "…$1…"`, so `-e`
/// re-interprets backslash escapes in the *interpolated value*, not just in the
/// helper's own colour codes. A worktree at `…/tab\there/work` — a legal POSIX
/// directory name — therefore came out of the retired shell as
/// `git -C …/tab<TAB>here/work pull --ff-only`: a remediation command the
/// operator cannot paste, naming a directory that does not exist. Same family
/// as #7858 (a path the shell rewrote on its way through a message), one layer
/// up: there the damage was `rm -rf` on the wrong path, here it is advice about
/// one.
///
/// The port prints the argv element it was given, so the hint stays pasteable.
/// This is pinned rather than reproduced for the same reason slice 8 pinned the
/// dead `--reference` arm: freezing a defect into Rust to keep a byte-identical
/// claim would make the port a worse artifact than the shell it replaces.
///
/// Kept out of the shared corpus deliberately — the four agreement tests above
/// assert equality, and a scenario that must *not* be equal does not belong in
/// a set they walk.
#[test]
fn a_backslash_in_the_path_is_re_escaped_by_the_shell_and_not_by_the_port() {
    let s = Scenario {
        name: "backslash-divergence".into(),
        arm: Arm::RegisteredWorktree,
        state: State::BehindTipUpstreamWrong,
        // A literal backslash followed by `t`: what `echo -e` turns into a TAB.
        dir: "tab\\there".into(),
        branch: "feature/issue-998".into(),
        json: false,
        uncommitted: true,
        issue: "998".into(),
    };
    let c = compare(&s);

    // Both sides agree the worktree is stale — the divergence is confined to
    // the hint lines that quote the path.
    assert!(
        c.shell.1.contains("may be stale") && c.rust.1.contains("may be stale"),
        "the fixture is no longer reporting drift, so it quotes no path at all"
    );

    assert!(
        c.shell.1.contains('\t'),
        "the retired shell no longer mangles a backslash escape in the path; if \
         `echo -e` was replaced with `printf '%s'` in the FROZEN fixture, that \
         is a defect — the fixture is the retired behaviour, warts included"
    );
    assert!(
        !c.rust.1.contains('\t'),
        "the port re-escaped the path — a remediation hint must name the \
         directory that exists, byte for byte as passed in argv"
    );
    assert!(
        c.rust.1.contains("tab\\there"),
        "the port's hint does not contain the path as given:\n{}",
        c.rust.1
    );
    assert_ne!(
        c.shell.1, c.rust.1,
        "shell and port agreed on a backslash-bearing path; one of them changed \
         behaviour and this pin no longer describes the tree"
    );

    // Nothing else moved: this is a message divergence, not a behaviour one.
    assert_eq!(c.shell.0, c.rust.0, "exit code diverged too");
    assert_eq!(
        c.shell_after, c.rust_after,
        "the repository aftermath diverged too — the divergence is supposed to \
         be confined to the printed hint"
    );
}
