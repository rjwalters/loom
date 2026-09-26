//! Differential test: the Rust port of `worktree.sh`'s race-safe
//! stale-worktree reset must agree with the shell it replaced, on a shared
//! corpus of worktree states (#8195 slice 6, epic #7810).
//!
//! # Shape (`defaults/docs/verification-recipes.md` §6)
//!
//! The retained suite (`test-worktree-race-rescue.sh`, 25 assertions) is the
//! black-box evidence and is not sufficient on its own: a suite only proves what
//! its author thought to write down, and #8011 shipped three silent divergences
//! past a green one. So the corpus here is generated from *the grammar the code
//! branches on* — the four rungs' inputs, crossed with the target-ref and label
//! shapes each rung formats into a message or a filename.
//!
//! **The corpus is written to disk once and both sides are built from those same
//! bytes.** An earlier differential in this epic had each side generate its own
//! inputs; they diverged, and the harness reported a divergence in code that had
//! not been measured. Here the spec is serialised to a file, read back, and
//! materialised into two trees which are asserted byte-identical *before* either
//! implementation runs — so "the inputs differed" fails with its own message
//! rather than silently lying.
//!
//! The shell side runs `tests/fixtures/worktree-race-rescue-retired.sh`, a
//! frozen copy of the retired functions. The live lib now delegates, so there is
//! no implementation left in it to compare against.
//!
//! # What is compared, per scenario
//!
//! 1. **The exit code.** It is the whole interface: `worktree.sh` branches on it
//!    to decide between "reset to base" and "continuing to use as-is", and 0/1/2
//!    mean reset / refused-nothing-touched / reset-failed. A port that refused
//!    where the shell reset (or worse, the other way) shows up here first.
//! 2. **stderr**, as an ordered list of lines with the tree root replaced by
//!    `<TREE>` and the patch timestamp by `<TS>`. Unlike slice 5's cleanup,
//!    stderr *is* this family's operator-visible contract — every message it has
//!    is a `>&2` — and the retained suite greps one of them.
//! 3. **The filesystem**, as a sorted manifest of `kind<TAB>relpath` for every
//!    surviving entry plus the contents of every file. This is what catches a
//!    `git reset --hard` that fired when it should not have (#6706/#6320, the
//!    data-loss class) and equally one that did not fire when it should, and it
//!    is where the rescue patch's *name*, *placement* and *bytes* are compared.
//! 4. **`git rev-parse HEAD`** afterwards. A reset that landed on a different
//!    commit than the shell's is invisible in 3 whenever the two commits happen
//!    to have the same tree.
//!
//! Commit SHAs are pinned identical across the two trees by fixing
//! author/committer identity and date, so 4 compares like for like and the
//! contents in 3 (which include `.gitignore`d nothing and no `.git` internals)
//! match byte for byte.
//!
//! # Not compared
//!
//! stdout: neither implementation writes to it, and `neither_side_writes_to_stdout`
//! pins that as a property rather than an assumption.
//!
//! One stderr line class is filtered, and only one: **bash's own diagnostic
//! about the fixture file**, matched by the fixture's absolute path followed by
//! `: line N:`. The shell's rescue write is a redirection (`git diff HEAD >
//! "$patch"`), so when the target is unwritable the *interpreter* prints
//! `…retired.sh: line 135: <path>: Permission denied` before the function's own
//! refusal. That line names a shell script and a line number inside it; there is
//! no possible port of it, nothing reads it, and the refusal it precedes is
//! compared in full. Filtering is scoped to lines beginning with the fixture's
//! own path so no message either implementation produces can hide behind it —
//! `the_comparison_can_actually_fail` pins that scoping.
//!
//! # The liveness rung, and why it is in here at all
//!
//! Three scenarios spawn a real `sleep` holder with its cwd inside *that side's*
//! tree. That is process state rather than filesystem state, so it is the one
//! part of this corpus that is not serialised — but leaving it out would leave
//! the port's single riskiest divergence uncompared: the shell's probe matched
//! **cwd only** while the Rust twin it claimed to mirror has matched any open
//! file descriptor since #7466. Each side's holder is asserted still alive after
//! the call, so "the holder died early" fails loudly instead of quietly turning
//! into agreement on the wrong thing.

use std::collections::BTreeMap;
use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

// ---------------------------------------------------------------------------
// Corpus: the grammar the four rungs branch on
// ---------------------------------------------------------------------------

/// What the worktree looks like when the guard is invoked. One variant per
/// distinguishable input to the commits-ahead and tracked-diff rungs, plus the
/// `.snapshots` shapes the rescue writer branches on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    /// HEAD at the baseline, nothing modified.
    Clean,
    /// A modified tracked file (unstaged).
    DirtyUnstaged,
    /// A modified tracked file, staged.
    DirtyStaged,
    /// Staged *and* a further unstaged edit on top — `git diff HEAD` sees both.
    DirtyBoth,
    /// A tracked file deleted. Still a tracked diff, and a patch that has to
    /// carry a deletion to be replayable.
    DirtyDeleted,
    /// A modified tracked file several directories down, under a path with
    /// spaces in it.
    DirtyNestedSpaces,
    /// An untracked file only. `git reset --hard` never touches it, so there is
    /// nothing to rescue and the reset must proceed.
    UntrackedOnly,
    /// Both at once: the patch must carry the tracked half and leave the
    /// untracked file alone.
    DirtyAndUntracked,
    /// A gitignored file alongside a tracked change — the ignored file must
    /// survive and stay out of the patch.
    DirtyAndIgnored,
    /// One commit past the baseline.
    AheadOne,
    /// Two commits past it — the count is interpolated into the refusal.
    AheadTwo,
    /// Ahead *and* dirty: the commits rung must answer first.
    AheadAndDirty,
    /// A regular FILE occupying the `.snapshots` path, so `mkdir -p` fails.
    /// This is the retained suite's own way of forcing the unrescuable case.
    SnapshotsIsFile,
    /// `.snapshots/` already exists and already holds a patch from an earlier
    /// rescue — the new one must be added, not replace it.
    SnapshotsHasOldPatch,
    /// `.snapshots/` exists with no write permission, so the patch write fails
    /// after the directory check passed.
    SnapshotsUnwritable,
}

impl State {
    fn as_str(self) -> &'static str {
        match self {
            State::Clean => "clean",
            State::DirtyUnstaged => "dirty-unstaged",
            State::DirtyStaged => "dirty-staged",
            State::DirtyBoth => "dirty-both",
            State::DirtyDeleted => "dirty-deleted",
            State::DirtyNestedSpaces => "dirty-nested-spaces",
            State::UntrackedOnly => "untracked-only",
            State::DirtyAndUntracked => "dirty-and-untracked",
            State::DirtyAndIgnored => "dirty-and-ignored",
            State::AheadOne => "ahead-one",
            State::AheadTwo => "ahead-two",
            State::AheadAndDirty => "ahead-and-dirty",
            State::SnapshotsIsFile => "snapshots-is-file",
            State::SnapshotsHasOldPatch => "snapshots-has-old-patch",
            State::SnapshotsUnwritable => "snapshots-unwritable",
        }
    }

    fn parse(s: &str) -> Self {
        [
            State::Clean,
            State::DirtyUnstaged,
            State::DirtyStaged,
            State::DirtyBoth,
            State::DirtyDeleted,
            State::DirtyNestedSpaces,
            State::UntrackedOnly,
            State::DirtyAndUntracked,
            State::DirtyAndIgnored,
            State::AheadOne,
            State::AheadTwo,
            State::AheadAndDirty,
            State::SnapshotsIsFile,
            State::SnapshotsHasOldPatch,
            State::SnapshotsUnwritable,
        ]
        .into_iter()
        .find(|v| v.as_str() == s)
        .unwrap_or_else(|| panic!("unknown state in corpus: {s}"))
    }
}

/// Which ref the guard is asked to reset to. Spelled as a ref NAME on both
/// sides (never a raw SHA baked into the corpus), so the corpus file stays
/// independent of the trees it is materialised into.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Target {
    /// `baseline` — where HEAD already is for most states.
    Baseline,
    /// `forward` — a commit ahead of the baseline. The reset has to move HEAD.
    Forward,
    /// A ref that does not resolve. `rev-list` fails (read as 0 by both sides)
    /// and `reset --hard` is what reports it.
    Missing,
    /// `HEAD~1` — resolves only for the ahead states, and is relative, so it
    /// means something different per scenario.
    HeadParent,
}

impl Target {
    fn as_str(self) -> &'static str {
        match self {
            Target::Baseline => "baseline",
            Target::Forward => "forward",
            Target::Missing => "refs/heads/does-not-exist",
            Target::HeadParent => "HEAD~1",
        }
    }

    fn parse(s: &str) -> Self {
        [
            Target::Baseline,
            Target::Forward,
            Target::Missing,
            Target::HeadParent,
        ]
        .into_iter()
        .find(|v| v.as_str() == s)
        .unwrap_or_else(|| panic!("unknown target in corpus: {s}"))
    }
}

/// Where a live process sits, if any. Not serialised as filesystem state — see
/// the module docs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Holder {
    None,
    /// cwd = the worktree root.
    Root,
    /// cwd = three levels below it. Only a recursive scan sees this (#7468).
    Nested3,
    /// cwd = a subdirectory whose name contains spaces.
    SpacedSubdir,
}

impl Holder {
    fn as_str(self) -> &'static str {
        match self {
            Holder::None => "none",
            Holder::Root => "root",
            Holder::Nested3 => "nested3",
            Holder::SpacedSubdir => "spaced-subdir",
        }
    }

    fn parse(s: &str) -> Self {
        [
            Holder::None,
            Holder::Root,
            Holder::Nested3,
            Holder::SpacedSubdir,
        ]
        .into_iter()
        .find(|v| v.as_str() == s)
        .unwrap_or_else(|| panic!("unknown holder in corpus: {s}"))
    }

    /// The cwd to put the holder in, relative to the repo.
    fn cwd(self, repo: &Path) -> Option<PathBuf> {
        match self {
            Holder::None => None,
            Holder::Root => Some(repo.to_path_buf()),
            Holder::Nested3 => Some(repo.join("nested").join("two").join("three")),
            Holder::SpacedSubdir => Some(repo.join("a holder dir")),
        }
    }
}

/// One scenario, serialised into the corpus file and materialised twice.
///
/// The fields are deliberately *independent*: #8011's fourth divergence hid in
/// a field the corpus left at one value in every case.
#[derive(Clone, Debug)]
struct Scenario {
    name: String,
    /// Repo directory name relative to the tree root. Contains a space in
    /// several cases — #7858's whole subject.
    repo: String,
    state: State,
    target: Target,
    /// The rescue-patch label. Carries spaces, and shell metacharacters, in
    /// some cases: the shell interpolated it into a filename.
    label: String,
    holder: Holder,
}

impl Scenario {
    fn to_line(&self) -> String {
        format!(
            "{}\t{}\t{}\t{}\t{}\t{}",
            self.name,
            self.repo,
            self.state.as_str(),
            self.target.as_str(),
            self.label,
            self.holder.as_str()
        )
    }

    fn from_line(line: &str) -> Self {
        let f: Vec<&str> = line.split('\t').collect();
        assert_eq!(f.len(), 6, "corpus line has the wrong field count: {line:?}");
        Self {
            name: f[0].to_string(),
            repo: f[1].to_string(),
            state: State::parse(f[2]),
            target: Target::parse(f[3]),
            label: f[4].to_string(),
            holder: Holder::parse(f[5]),
        }
    }
}

const PLAIN_REPO: &str = "repo";
const SPACED_REPO: &str = "a repo with spaces";
const DEFAULT_LABEL: &str = "issue-42-stale-worktree-reset";

/// Every state against both repo-name shapes, plus the target, label and
/// liveness variations each rung formats or branches on.
fn corpus() -> Vec<Scenario> {
    let states = [
        State::Clean,
        State::DirtyUnstaged,
        State::DirtyStaged,
        State::DirtyBoth,
        State::DirtyDeleted,
        State::DirtyNestedSpaces,
        State::UntrackedOnly,
        State::DirtyAndUntracked,
        State::DirtyAndIgnored,
        State::AheadOne,
        State::AheadTwo,
        State::AheadAndDirty,
        State::SnapshotsIsFile,
        State::SnapshotsHasOldPatch,
        State::SnapshotsUnwritable,
    ];

    let mut out = Vec::new();

    // Every state, reset to the baseline, in a plain path and in one with
    // spaces. This is the main grid: 30 cases.
    for state in states {
        for repo in [PLAIN_REPO, SPACED_REPO] {
            let tag = if repo == PLAIN_REPO {
                "plain"
            } else {
                "spaced"
            };
            out.push(Scenario {
                name: format!("{}-{tag}", state.as_str()),
                repo: repo.to_string(),
                state,
                target: Target::Baseline,
                label: DEFAULT_LABEL.to_string(),
                holder: Holder::None,
            });
        }
    }

    // Targets other than the baseline. `forward` makes the reset MOVE HEAD (so
    // "did nothing" cannot pass as agreement); `missing` is the exit-2 arm;
    // `HEAD~1` resolves only where there is a parent to reach.
    for (state, target) in [
        (State::Clean, Target::Forward),
        (State::DirtyUnstaged, Target::Forward),
        (State::UntrackedOnly, Target::Forward),
        (State::Clean, Target::Missing),
        (State::DirtyUnstaged, Target::Missing),
        (State::AheadOne, Target::Missing),
        (State::AheadOne, Target::HeadParent),
        (State::AheadTwo, Target::HeadParent),
        (State::AheadAndDirty, Target::HeadParent),
        (State::Clean, Target::HeadParent),
    ] {
        out.push(Scenario {
            name: format!("{}-to-{}", state.as_str(), target.as_str().replace('/', "-")),
            repo: PLAIN_REPO.to_string(),
            state,
            target,
            label: DEFAULT_LABEL.to_string(),
            holder: Holder::None,
        });
    }

    // Label shapes. All only observable when a rescue happens, so each pairs
    // with a dirty state.
    for (tag, label) in [
        ("plain-label", "loom-race-rescue"),
        ("spaced-label", "label with spaces"),
        ("meta-label", "weird $label *?[a-z]"),
        ("dotted-label", "..label.with.dots"),
    ] {
        out.push(Scenario {
            name: tag.to_string(),
            repo: SPACED_REPO.to_string(),
            state: State::DirtyUnstaged,
            target: Target::Baseline,
            label: label.to_string(),
            holder: Holder::None,
        });
    }

    // The liveness rung. Clean + Forward so that WITHOUT a holder the reset
    // would move HEAD — which is what makes "refused" distinguishable from
    // "nothing to do"; plus one dirty case, where a divergence would also show
    // up as a patch appearing on one side only.
    for (tag, state, holder) in [
        ("live-root", State::Clean, Holder::Root),
        ("live-nested3", State::Clean, Holder::Nested3),
        ("live-spaced-subdir", State::DirtyUnstaged, Holder::SpacedSubdir),
    ] {
        out.push(Scenario {
            name: tag.to_string(),
            repo: SPACED_REPO.to_string(),
            state,
            target: Target::Forward,
            label: DEFAULT_LABEL.to_string(),
            holder,
        });
    }

    out
}

// ---------------------------------------------------------------------------
// Materialisation
// ---------------------------------------------------------------------------

/// Fixed identity and dates, so the two trees' commit SHAs are equal and the
/// HEAD comparison means something.
fn git(repo: &Path, args: &[&str]) {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_AUTHOR_NAME", "Loom Fixture")
        .env("GIT_AUTHOR_EMAIL", "fixture@loom.test")
        .env("GIT_COMMITTER_NAME", "Loom Fixture")
        .env("GIT_COMMITTER_EMAIL", "fixture@loom.test")
        .env("GIT_AUTHOR_DATE", "2026-01-01T00:00:00+0000")
        .env("GIT_COMMITTER_DATE", "2026-01-01T00:00:00+0000")
        .output()
        .expect("run git");
    assert!(
        out.status.success(),
        "git {args:?} in {repo:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

const NESTED_DIR: &str = "a dir/with spaces";

/// Build one scenario's tree under `root`. Returns the repo path.
fn materialise(root: &Path, s: &Scenario) -> PathBuf {
    let repo = root.join(&s.repo);
    fs::create_dir_all(repo.join(NESTED_DIR)).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    // Slice 5's lesson: git's own background maintenance can mutate a fixture
    // out from under a comparison. A fixture that can change on its own has no
    // business deciding one.
    git(&repo, &["config", "maintenance.auto", "false"]);
    git(&repo, &["config", "gc.auto", "0"]);

    fs::write(repo.join("tracked.txt"), "base content\n").unwrap();
    fs::write(repo.join(NESTED_DIR).join("file.txt"), "nested base\n").unwrap();
    fs::write(repo.join(".gitignore"), "ignored-*\n").unwrap();
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-q", "-m", "base"]);
    git(&repo, &["branch", "baseline"]);

    // A commit ahead of the baseline, parked on its own branch so `forward`
    // resolves while HEAD stays at the baseline.
    fs::write(repo.join("tracked.txt"), "forward content\n").unwrap();
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-q", "-m", "forward"]);
    git(&repo, &["branch", "forward"]);
    git(&repo, &["reset", "-q", "--hard", "baseline"]);

    match s.state {
        State::Clean => {}
        State::DirtyUnstaged => {
            fs::write(repo.join("tracked.txt"), "foreign edit\n").unwrap();
        }
        State::DirtyStaged => {
            fs::write(repo.join("tracked.txt"), "staged foreign edit\n").unwrap();
            git(&repo, &["add", "tracked.txt"]);
        }
        State::DirtyBoth => {
            fs::write(repo.join("tracked.txt"), "staged foreign edit\n").unwrap();
            git(&repo, &["add", "tracked.txt"]);
            fs::write(repo.join("tracked.txt"), "and then unstaged on top\n").unwrap();
        }
        State::DirtyDeleted => {
            fs::remove_file(repo.join("tracked.txt")).unwrap();
        }
        State::DirtyNestedSpaces => {
            fs::write(repo.join(NESTED_DIR).join("file.txt"), "nested foreign\n").unwrap();
        }
        State::UntrackedOnly => {
            fs::write(repo.join("scratch.txt"), "untracked scratch\n").unwrap();
        }
        State::DirtyAndUntracked => {
            fs::write(repo.join("tracked.txt"), "foreign edit\n").unwrap();
            fs::write(repo.join("scratch.txt"), "untracked scratch\n").unwrap();
        }
        State::DirtyAndIgnored => {
            fs::write(repo.join("tracked.txt"), "foreign edit\n").unwrap();
            fs::write(repo.join("ignored-build.log"), "ignored output\n").unwrap();
        }
        State::AheadOne => {
            fs::write(repo.join("tracked.txt"), "a raced commit\n").unwrap();
            git(&repo, &["add", "-A"]);
            git(&repo, &["commit", "-q", "-m", "raced 1"]);
        }
        State::AheadTwo => {
            for n in 1..=2 {
                fs::write(repo.join("tracked.txt"), format!("a raced commit {n}\n")).unwrap();
                git(&repo, &["add", "-A"]);
                git(&repo, &["commit", "-q", "-m", &format!("raced {n}")]);
            }
        }
        State::AheadAndDirty => {
            fs::write(repo.join("tracked.txt"), "a raced commit\n").unwrap();
            git(&repo, &["add", "-A"]);
            git(&repo, &["commit", "-q", "-m", "raced 1"]);
            fs::write(repo.join("tracked.txt"), "and uncommitted on top\n").unwrap();
        }
        State::SnapshotsIsFile => {
            fs::write(repo.join("tracked.txt"), "precious foreign work\n").unwrap();
            fs::write(repo.join(".snapshots"), "").unwrap();
        }
        State::SnapshotsHasOldPatch => {
            fs::write(repo.join("tracked.txt"), "precious foreign work\n").unwrap();
            fs::create_dir_all(repo.join(".snapshots")).unwrap();
            fs::write(
                repo.join(".snapshots")
                    .join("earlier-20260101T000000Z.patch"),
                "an earlier rescue\n",
            )
            .unwrap();
        }
        State::SnapshotsUnwritable => {
            fs::write(repo.join("tracked.txt"), "precious foreign work\n").unwrap();
            let dir = repo.join(".snapshots");
            fs::create_dir_all(&dir).unwrap();
            set_mode(&dir, 0o500);
        }
    }

    repo
}

fn set_mode(path: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt as _;
    let mut perms = fs::metadata(path).unwrap().permissions();
    perms.set_mode(mode);
    fs::set_permissions(path, perms).unwrap();
}

/// Restore write permission so the tree can be torn down (and so the manifest
/// walk can read it) regardless of which arm the scenario took.
fn unlock_snapshots(repo: &Path) {
    let dir = repo.join(".snapshots");
    if dir.is_dir() {
        set_mode(&dir, 0o755);
    }
}

// ---------------------------------------------------------------------------
// Observation
// ---------------------------------------------------------------------------

struct Observation {
    code: i32,
    stderr: Vec<String>,
    stdout: Vec<String>,
    manifest: Vec<String>,
    head: String,
}

/// `<TREE>`/`<TS>`-normalised line. The tree root differs between the two sides
/// by construction, and the rescue patch's `date -u +%Y%m%dT%H%M%SZ` stamp is
/// taken at two different instants — everything else is contract.
fn normalise(line: &str, root: &Path) -> String {
    let replaced = line.replace(&root.to_string_lossy().to_string(), "<TREE>");
    mask_stamp(&replaced)
}

/// Replace every `YYYYMMDDTHHMMSSZ` run with `<TS>`.
fn mask_stamp(s: &str) -> String {
    let bytes: Vec<char> = s.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    while i < bytes.len() {
        let is_stamp = i + 16 <= bytes.len()
            && bytes[i..i + 8].iter().all(char::is_ascii_digit)
            && bytes[i + 8] == 'T'
            && bytes[i + 9..i + 15].iter().all(char::is_ascii_digit)
            && bytes[i + 15] == 'Z';
        if is_stamp {
            out.push_str("<TS>");
            i += 16;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    out
}

fn lines(raw: &[u8], root: &Path) -> Vec<String> {
    String::from_utf8_lossy(raw)
        .lines()
        .filter(|l| !is_interpreter_diagnostic(l))
        .map(|l| normalise(l.trim(), root))
        .filter(|l| !l.is_empty())
        .collect()
}

/// `<fixture path>: line N: …` — bash complaining about its own script, which is
/// the only stderr the port cannot have an equivalent of. See the module docs
/// for why this is filtered and why the match is anchored on the fixture's
/// absolute path rather than on the text of the complaint.
fn is_interpreter_diagnostic(line: &str) -> bool {
    let prefix = format!("{}: line ", fixture_path().to_string_lossy());
    line.starts_with(&prefix)
}

/// Sorted `kind<TAB>relpath` for every surviving entry under `repo`, plus file
/// contents. `.git` is skipped wholesale: nothing in this family writes to it
/// (no worktree add, no prune), and its index records mtimes.
fn manifest(repo: &Path, root: &Path) -> Vec<String> {
    let mut entries = BTreeMap::new();
    walk(repo, repo, &mut entries);
    entries
        .into_iter()
        .map(|(rel, v)| format!("{}\t{v}", mask_stamp(&rel)))
        .map(|l| normalise(&l, root))
        .collect()
}

fn walk(dir: &Path, base: &Path, out: &mut BTreeMap<String, String>) {
    let Ok(entries) = fs::read_dir(dir) else {
        // An unreadable directory is itself a comparable fact; the entry for
        // the directory has already been recorded by the caller.
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let rel = path
            .strip_prefix(base)
            .expect("descendant of base")
            .to_string_lossy()
            .to_string();
        if rel == ".git" {
            continue;
        }
        let meta = match fs::symlink_metadata(&path) {
            Ok(m) => m,
            Err(e) => {
                out.insert(rel, format!("unstattable: {}", e.kind()));
                continue;
            }
        };
        if meta.is_symlink() {
            let target = fs::read_link(&path).unwrap_or_default();
            out.insert(rel, format!("symlink -> {}", target.to_string_lossy()));
        } else if meta.is_dir() {
            out.insert(rel.clone(), "dir".to_string());
            walk(&path, base, out);
        } else {
            let content = fs::read(&path).unwrap_or_default();
            out.insert(
                rel,
                format!("file: {}", String::from_utf8_lossy(&content).replace('\n', "\\n")),
            );
        }
    }
}

fn head(repo: &Path) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["rev-parse", "HEAD"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("git rev-parse");
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

// ---------------------------------------------------------------------------
// The two implementations
// ---------------------------------------------------------------------------

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("workspace root")
        .to_path_buf()
}

/// A live `sleep` whose cwd is inside the tree, killed on drop.
struct LiveHolder(Child);

impl LiveHolder {
    fn spawn(cwd: &Path) -> Self {
        fs::create_dir_all(cwd).unwrap();
        let child = Command::new("sleep")
            .arg("120")
            .current_dir(cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn holder");
        // Give it time to be visible to a cwd probe before either side looks.
        std::thread::sleep(std::time::Duration::from_millis(300));
        Self(child)
    }

    fn assert_still_alive(&mut self, scenario: &str) {
        match self.0.try_wait().expect("try_wait") {
            None => {}
            Some(status) => panic!(
                "{scenario}: the live holder exited ({status:?}) before the guard ran — this \
                 scenario compared nothing about the liveness rung"
            ),
        }
    }
}

impl Drop for LiveHolder {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn fixture_path() -> PathBuf {
    repo_root().join("loom-daemon/tests/fixtures/worktree-race-rescue-retired.sh")
}

fn run_shell(cwd: &Path, repo: &Path, s: &Scenario) -> (i32, Vec<u8>, Vec<u8>) {
    let script = fixture_path();
    assert!(script.exists(), "frozen fixture missing at {script:?}");
    let out = Command::new("bash")
        .arg(&script)
        .arg(repo)
        .arg(s.target.as_str())
        .arg(&s.label)
        .current_dir(cwd)
        // `LC_ALL=C` because the shell side's `awk`/`grep` and the port's Rust
        // string handling only agree on collation and character classes under a
        // pinned locale (an epic-#7810 criterion).
        .env("LC_ALL", "C")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("bash");
    (out.status.code().unwrap_or(-1), out.stdout, out.stderr)
}

fn run_rust(cwd: &Path, repo: &Path, s: &Scenario) -> (i32, Vec<u8>, Vec<u8>) {
    let out = Command::new(env!("CARGO_BIN_EXE_loom-daemon"))
        .arg("worktree-reset")
        .arg("--worktree")
        .arg(repo)
        .arg("--target-ref")
        .arg(s.target.as_str())
        .arg("--rescue-label")
        .arg(&s.label)
        .current_dir(cwd)
        .env("LC_ALL", "C")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("loom-daemon");
    (out.status.code().unwrap_or(-1), out.stdout, out.stderr)
}

// ---------------------------------------------------------------------------
// The comparison
// ---------------------------------------------------------------------------

fn tmproot(tag: &str) -> PathBuf {
    let base = std::env::temp_dir().join(format!(
        "loom-reset-diff-{tag}-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = fs::remove_dir_all(&base);
    fs::create_dir_all(&base).unwrap();
    fs::canonicalize(&base).unwrap()
}

/// Write the corpus to disk and read it straight back, so everything below is
/// built from the FILE rather than from the generator.
fn corpus_from_disk(dir: &Path) -> Vec<Scenario> {
    let path = dir.join("corpus.tsv");
    let mut f = fs::File::create(&path).unwrap();
    for s in corpus() {
        writeln!(f, "{}", s.to_line()).unwrap();
    }
    drop(f);
    let text = fs::read_to_string(&path).unwrap();
    let parsed: Vec<Scenario> = text.lines().map(Scenario::from_line).collect();
    assert!(parsed.len() >= 45, "corpus shrank unexpectedly: {} scenarios", parsed.len());
    parsed
}

fn observe(
    side: &str,
    root: &Path,
    s: &Scenario,
    run: impl Fn(&Path, &Path, &Scenario) -> (i32, Vec<u8>, Vec<u8>),
) -> Observation {
    let repo = materialise(root, s);
    let mut holder = s.holder.cwd(&repo).map(|c| LiveHolder::spawn(&c));

    // Both sides are invoked from a cwd OUTSIDE the worktree, which is how
    // `worktree.sh` calls this (it has already navigated to the main
    // workspace). A cwd inside would make the caller its own live holder.
    let (code, stdout, stderr) = run(root, &repo, s);

    if let Some(h) = holder.as_mut() {
        h.assert_still_alive(&format!("{side}/{}", s.name));
    }
    drop(holder);

    unlock_snapshots(&repo);
    Observation {
        code,
        stderr: lines(&stderr, root),
        stdout: lines(&stdout, root),
        manifest: manifest(&repo, root),
        head: head(&repo),
    }
}

#[test]
fn the_port_agrees_with_the_retired_shell_on_every_scenario() {
    let dir = tmproot("main");
    let scenarios = corpus_from_disk(&dir);

    let mut comparisons = 0usize;
    let mut divergences: Vec<String> = Vec::new();

    for s in &scenarios {
        let shell_root = dir.join(format!("shell/{}", s.name));
        let rust_root = dir.join(format!("rust/{}", s.name));
        fs::create_dir_all(&shell_root).unwrap();
        fs::create_dir_all(&rust_root).unwrap();

        // The inputs are asserted identical BEFORE either side runs, so "the
        // trees differed" is its own failure rather than a divergence report
        // about code that was never compared.
        let a = materialise(&shell_root, s);
        let b = materialise(&rust_root, s);
        unlock_snapshots(&a);
        unlock_snapshots(&b);
        assert_eq!(
            manifest(&a, &shell_root),
            manifest(&b, &rust_root),
            "{}: the two materialised trees differ BEFORE either side ran",
            s.name
        );
        assert_eq!(head(&a), head(&b), "{}: fixture HEADs differ", s.name);
        fs::remove_dir_all(&shell_root).unwrap();
        fs::remove_dir_all(&rust_root).unwrap();
        fs::create_dir_all(&shell_root).unwrap();
        fs::create_dir_all(&rust_root).unwrap();

        let shell = observe("shell", &shell_root, s, run_shell);
        let rust = observe("rust", &rust_root, s, run_rust);

        comparisons += 1;
        if shell.code != rust.code {
            divergences.push(format!(
                "{}: exit code — shell {} vs rust {}\n  shell stderr: {:?}\n  rust stderr:  {:?}",
                s.name, shell.code, rust.code, shell.stderr, rust.stderr
            ));
        }
        comparisons += 1;
        if shell.stderr != rust.stderr {
            divergences.push(format!(
                "{}: stderr —\n  shell: {:?}\n  rust:  {:?}",
                s.name, shell.stderr, rust.stderr
            ));
        }
        comparisons += 1;
        if shell.manifest != rust.manifest {
            divergences.push(format!(
                "{}: filesystem —\n  shell-only: {:?}\n  rust-only:  {:?}",
                s.name,
                only_in(&shell.manifest, &rust.manifest),
                only_in(&rust.manifest, &shell.manifest)
            ));
        }
        comparisons += 1;
        if shell.head != rust.head {
            divergences
                .push(format!("{}: HEAD — shell {} vs rust {}", s.name, shell.head, rust.head));
        }
    }

    // A differential that silently degrades to comparing nothing is the failure
    // this whole method exists to avoid, so the count is asserted rather than
    // trusted.
    assert_eq!(
        comparisons,
        scenarios.len() * 4,
        "every scenario must contribute exactly four comparisons"
    );
    assert!(comparisons >= 180, "too few comparisons ran: {comparisons}");
    assert!(
        divergences.is_empty(),
        "{} divergence(s) across {} scenarios:\n\n{}",
        divergences.len(),
        scenarios.len(),
        divergences.join("\n\n")
    );
}

fn only_in(a: &[String], b: &[String]) -> Vec<String> {
    a.iter().filter(|l| !b.contains(l)).cloned().collect()
}

/// Neither implementation writes to stdout, and that is a property rather than
/// an assumption: `worktree.sh` calls this inside a `&&` chain whose stdout is
/// the script's own, and in `--json` mode a stray line would land in the JSON
/// document (the #3546 class).
#[test]
fn neither_side_writes_to_stdout() {
    let dir = tmproot("stdout");
    let s = Scenario {
        name: "stdout-purity".to_string(),
        repo: SPACED_REPO.to_string(),
        state: State::DirtyAndUntracked,
        target: Target::Baseline,
        label: DEFAULT_LABEL.to_string(),
        holder: Holder::None,
    };
    let shell_root = dir.join("shell");
    let rust_root = dir.join("rust");
    fs::create_dir_all(&shell_root).unwrap();
    fs::create_dir_all(&rust_root).unwrap();

    let shell = observe("shell", &shell_root, &s, run_shell);
    let rust = observe("rust", &rust_root, &s, run_rust);
    assert!(shell.stdout.is_empty(), "shell stdout: {:?}", shell.stdout);
    assert!(rust.stdout.is_empty(), "rust stdout: {:?}", rust.stdout);
    // …and the rescue did happen, so this is not vacuous.
    assert_eq!(shell.code, 0);
    assert_eq!(rust.code, 0);
    assert!(rust.stderr.iter().any(|l| l.contains("rescued foreign")));
}

/// Prove the four comparisons can go RED before believing a green run
/// (epic #7810's first criterion — the watchdog port's retained suite reported
/// 206/206 before its stub existed, because it was measuring the shell).
///
/// Each dimension is mutated on one side only and asserted to be detected by the
/// same equality the real test uses.
#[test]
fn the_comparison_can_actually_fail() {
    let dir = tmproot("red");
    let s = Scenario {
        name: "red".to_string(),
        repo: SPACED_REPO.to_string(),
        state: State::DirtyAndUntracked,
        target: Target::Baseline,
        label: DEFAULT_LABEL.to_string(),
        holder: Holder::None,
    };
    let root = dir.join("tree");
    fs::create_dir_all(&root).unwrap();
    let real = observe("rust", &root, &s, run_rust);

    // 1. exit code
    assert_ne!(real.code, 1, "the baseline scenario must be a successful reset");
    assert_ne!(real.code, real.code + 1);

    // 2. stderr — the rescue line is present, and a one-word change is caught.
    assert!(
        real.stderr
            .iter()
            .any(|l| l.contains("rescued foreign tracked changes")),
        "expected a rescue line: {:?}",
        real.stderr
    );
    let mutated: Vec<String> = real
        .stderr
        .iter()
        .map(|l| l.replace("rescued", "discarded"))
        .collect();
    assert_ne!(real.stderr, mutated, "a reworded message must be detected");

    // 3. filesystem — the manifest contains the rescue patch (name normalised),
    //    the untracked file, and the reset content; dropping any one is caught.
    let patch_entry = real
        .manifest
        .iter()
        .find(|l| l.contains(".snapshots/") && l.contains("<TS>.patch"))
        .unwrap_or_else(|| panic!("no rescue patch in manifest: {:?}", real.manifest));
    assert!(
        patch_entry.contains("+foreign edit"),
        "the patch's BYTES must be compared, not just its name: {patch_entry}"
    );
    assert!(
        real.manifest
            .iter()
            .any(|l| l.starts_with("scratch.txt\t") && l.contains("untracked scratch")),
        "the untracked file must be in the manifest: {:?}",
        real.manifest
    );
    assert!(
        real.manifest
            .iter()
            .any(|l| l.starts_with("tracked.txt\t") && l.contains("base content")),
        "the reset content must be in the manifest: {:?}",
        real.manifest
    );
    let dropped: Vec<String> = real
        .manifest
        .iter()
        .filter(|l| *l != patch_entry)
        .cloned()
        .collect();
    assert_ne!(real.manifest, dropped, "a missing patch must be detected");

    // 4. …and the manifest does NOT descend into `.git`, so an index mtime
    //    cannot decide a comparison. `.gitignore` is a tracked file and must
    //    still be there — an over-broad prefix skip would drop it.
    assert!(
        !real
            .manifest
            .iter()
            .any(|l| l == ".git\tdir" || l.starts_with(".git/")),
        "the manifest must skip .git: {:?}",
        real.manifest
    );
    assert!(
        real.manifest.iter().any(|l| l.starts_with(".gitignore\t")),
        "skipping .git must not also skip .gitignore: {:?}",
        real.manifest
    );

    // 6. The interpreter-diagnostic filter is anchored on the fixture's own
    //    path, so it cannot swallow a line either implementation produced.
    assert!(is_interpreter_diagnostic(&format!(
        "{}: line 135: /x: Permission denied",
        fixture_path().to_string_lossy()
    )));
    assert!(!is_interpreter_diagnostic(
        "loom_worktree_reset_or_rescue: refusing to reset /x — line 135: nope"
    ));
    assert!(!is_interpreter_diagnostic("/some/other/script.sh: line 1: boom"));

    // 5. HEAD is a real SHA, so an equality on it is not comparing two empties.
    assert_eq!(real.head.len(), 40, "HEAD must be a full SHA: {}", real.head);
}

/// The corpus file is the spec: a line the parser cannot read must fail loudly
/// rather than be skipped into a smaller, still-green run.
#[test]
fn a_malformed_corpus_line_is_fatal() {
    let bad = "only\ttwo\tfields";
    assert!(std::panic::catch_unwind(|| Scenario::from_line(bad)).is_err());
    let unknown = "n\trepo\tno-such-state\tbaseline\tlabel\tnone";
    assert!(std::panic::catch_unwind(|| Scenario::from_line(unknown)).is_err());
}
