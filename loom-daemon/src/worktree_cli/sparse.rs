//! `worktree.sh`'s sparse-checkout family — `--sparse <paths...>` and `--full`
//! (#8195 slice 10, epic #7810).
//!
//! # What moved here
//!
//! Everything the two flags did, on both of the arms they reach:
//!
//! | arm | retired shell | what it does |
//! |---|---|---|
//! | [`Arm::Create`] | the post-`git worktree add --no-checkout` block | configure the cone, materialize it, log the size; hand the cone back for the final JSON document |
//! | [`Arm::Reconfigure`] | the "worktree dir already exists + `--sparse`/`--full`" early exit | check the directory is a registered worktree, apply or disable the cone, log the size, back-fill the sentinel (#3548), print the final lines / JSON document |
//!
//! plus the helpers only they used: `apply_sparse_cone`,
//! `materialize_sparse_cone`, `disable_sparse_checkout`, `log_worktree_size`,
//! the always-included safety set (`.claude .loom .githooks scripts` +
//! `$LOOM_WORKTREE_ALWAYS_INCLUDE`), and the `printf | awk` cone-to-JSON
//! builder that was written out twice.
//!
//! # Why this family, now
//!
//! It is the one part of the create path that is **opt-in**. Every remaining
//! arm of `worktree.sh <N>` runs on the always-taken path, where a hard daemon
//! dependency is exactly what got slice 1's first delegation reverted (#8226),
//! and where a full shell fallback would have to be kept alongside the port —
//! growing a file whose portable-shell budget admits no growth. `--sparse` and
//! `--full` are flags no role prompt, command or script in the tree passes, so
//! this family can leave the shell entirely, today, without moving the
//! ordinary `worktree.sh <N>` onto a built binary. See "No daemon" below for
//! what that costs and why it is the right trade.
//!
//! It also carried three defects, all reproduced against the retired script
//! before this port was written:
//!
//! 1. **A silent exit 128.** The two `git sparse-checkout` calls ran bare under
//!    `set -e` with both streams sent to `/dev/null`. A cone git rejects — a
//!    glob such as `src/*`, a leading `/` — killed the script mid-create with
//!    git's exit code 128, no message, and (under `--json`) no JSON document,
//!    leaving behind an empty `--no-checkout` worktree. The port reports what
//!    git said and exits 1.
//! 2. **Invalid JSON.** The cone array was built with `awk '{printf "\"%s\""}'`
//!    — no escaping. A cone path containing `"` or `\` produced a document
//!    `jq` cannot parse, from the one mode whose whole contract is "stdout is
//!    exactly one JSON document" (#3546). Every string here goes through
//!    `serde_json`.
//! 3. **A registration check that answered the wrong question.** The
//!    re-configure arm decided "is this a registered worktree?" with
//!    `git worktree list | grep -q "$WORKTREE_PATH"` — an unanchored regex
//!    *substring* match against symlink-*resolved* paths. That fails in both
//!    directions: a repo reached through a symlink (`~/code` → `/Volumes/…`)
//!    has a LIVE worktree reported as unregistered, while an unregistered
//!    `issue-4` matches a registered `issue-44` — after which the retired arm
//!    ran `git -C issue-4 sparse-checkout …`, which git resolves UP to the
//!    main workspace, and then wrote a `.loom-managed` sentinel into the
//!    unregistered directory, authorizing cleanup tooling to `rm -rf` it. The
//!    port asks [`super::cleanup::is_registered`] — the exact, canonicalizing
//!    predicate slice 5 already uses for the orphan guard. The issue body's
//!    "two implementations of *is this worktree safe to touch*", again: the
//!    orphan guard and this arm asked the same question of the same directory
//!    and could disagree.
//!
//! # Behaviour deliberately preserved
//!
//! - The git commands and their order: `sparse-checkout init --cone`, then
//!   `sparse-checkout set <cone>`, then `checkout` to materialize; for
//!   `--full`, `sparse-checkout disable`, falling back to unsetting
//!   `core.sparseCheckout`/`core.sparseCheckoutCone`, then `checkout`. All
//!   with `-C <worktree>`, all with both streams discarded, and the two
//!   `checkout`s' failures ignored, as before.
//! - Every message text, its order, and its silence under `--json`.
//! - `$LOOM_WORKTREE_ALWAYS_INCLUDE` is split on the characters bash's default
//!   `IFS` splits on — space, tab, newline — and appended after the defaults.
//! - The JSON documents' field order and spacing, byte for byte, for every
//!   input the retired builder produced valid JSON for.
//! - The size line is `du -sh`'s first field and is omitted when `du` prints
//!   nothing, as `awk '{print $1}'` on empty input did.
//!
//! # Deliberate differences
//!
//! The three defects above, plus two narrower ones in the same direction:
//!
//! - `$LOOM_WORKTREE_ALWAYS_INCLUDE` is no longer glob-expanded. The retired
//!   `EXTRA_INCLUDE=(${LOOM_WORKTREE_ALWAYS_INCLUDE})` was unquoted, so a
//!   `docs/*` entry expanded against whatever the *caller's* cwd happened to
//!   be — and cone mode rejects the pattern either way (defect 1).
//! - Messages are not passed through `echo -e`, so a backslash in a path is
//!   printed as a backslash. Same divergence, same side, as slice 9's
//!   `worktree_cli::upstream`.
//!
//! # Exit codes
//!
//! - **0** — applied (and, for [`Arm::Reconfigure`], the sentinel is written
//!   and the final lines/document printed).
//! - **1** — refused or failed: the directory is not a registered worktree, or
//!   git rejected the cone, or the sentinel could not be written. The same
//!   code the retired script's own refusals used, and the one it would have
//!   produced for a git failure had `set -e` not propagated git's 128 instead.
//! - **2** — could not run: a malformed invocation (clap's own code, and the
//!   `--full` + `create` combination no caller produces).
//!
//! # No daemon
//!
//! `worktree.sh` refuses `--sparse`/`--full` with exit 2 **before touching
//! anything** — before the crash-debris cleanup, the add lock, the lease, the
//! fetch — when no binary resolves or the one that does predates this
//! subcommand. The same contract, and the same code, as the `remove` and WIP
//! verbs (slices 2/3): 2 is what every epic-#7810 stub reserves for "could not
//! run at all". What must never happen is the alternative a best-effort skip
//! would give: `--sparse` without a cone step creates the worktree with
//! `--no-checkout` and nothing in it, and reports success.
//!
//! A plain `worktree.sh <N>` never reaches this module and is unaffected.

use std::ffi::{OsStr, OsString};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};

use super::sentinel;
use super::wip::Out;

/// The always-included safety set: even with `--sparse`, these must
/// materialize or the worktree is unusable by an agent. `.claude/` (skills and
/// hooks), `.loom/` (scripts, roles, hooks), `.githooks/` (`core.hooksPath` is
/// pointed at it after creation) and `scripts/`. Top-level tracked files are
/// always included implicitly by cone mode.
pub const ALWAYS_INCLUDE: [&str; 4] = [".claude", ".loom", ".githooks", "scripts"];

/// Which retired block this invocation is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Arm {
    /// Right after `git worktree add --no-checkout` created the worktree.
    Create,
    /// The worktree directory already existed and `--sparse`/`--full` was
    /// passed: apply the mode to it and conclude the run.
    Reconfigure,
}

/// Cone paths to apply, or `--full`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mode {
    /// The caller's `--sparse` paths, *before* the always-included set is
    /// appended (that happens here, see [`cone_paths`]).
    Sparse(Vec<OsString>),
    /// `--full`: disable sparse-checkout.
    Full,
}

/// Everything one invocation needs.
pub struct Options {
    /// Where `git worktree list` runs for the registration check: the main
    /// workspace, which is the caller's cwd (the retired `git worktree list`
    /// ran there with no `-C`).
    pub repo: PathBuf,
    /// `$WORKTREE_PATH`: quoted verbatim into the human messages, and resolved
    /// the way `cd … && pwd` would for the JSON `worktreePath`.
    pub worktree: PathBuf,
    pub arm: Arm,
    pub mode: Mode,
    /// `$ISSUE_NUMBER`, already validated as ASCII digits by the caller.
    pub issue: String,
    /// `$BRANCH_NAME`.
    pub branch: String,
    /// `--json`: no narration; [`Arm::Reconfigure`] prints its JSON document
    /// and [`Arm::Create`] prints the cone array for the caller's.
    pub json: bool,
    /// `$LOOM_WORKTREE_ALWAYS_INCLUDE`, passed in rather than read here so the
    /// tests can drive it without touching the process environment.
    pub extra_include: Option<OsString>,
}

/// Run one invocation. Returns the process exit code — see the module docs.
pub fn run(opts: &Options) -> i32 {
    let out = Out::new(false);
    let say = |f: &dyn Fn(&Out)| {
        if !opts.json {
            f(&out);
        }
    };

    if opts.arm == Arm::Create && opts.mode == Mode::Full {
        Out::error(
            "worktree-sparse: --full applies only to an existing worktree (--arm reconfigure)",
        );
        return 2;
    }
    if let Mode::Sparse(paths) = &opts.mode {
        if paths.is_empty() {
            Out::error("worktree-sparse: --sparse requires at least one path");
            return 2;
        }
    }

    if opts.arm == Arm::Reconfigure
        && !super::cleanup::is_registered_in(Some(&opts.repo), &opts.worktree)
    {
        if opts.json {
            println!(
                r#"{{"success": false, "error": "Directory exists but is not a registered worktree"}}"#
            );
        } else {
            Out::error(&format!(
                "Directory exists but is not a registered worktree: {}",
                opts.worktree.to_string_lossy()
            ));
        }
        return 1;
    }

    let cone = match &opts.mode {
        Mode::Sparse(paths) => {
            let cone = cone_paths(paths, opts.extra_include.as_deref());
            say(&|o| o.info("Configuring sparse-checkout cone..."));
            if let Err(failure) = apply_cone(&opts.worktree, &cone) {
                report_git_failure(opts, &failure);
                return 1;
            }
            materialize(&opts.worktree);
            Some(cone)
        }
        Mode::Full => {
            say(&|o| o.info("Disabling sparse-checkout (full mode)..."));
            disable(&opts.worktree);
            None
        }
    };

    if !opts.json {
        let label = match (&opts.arm, &cone) {
            (Arm::Create, _) => "Sparse worktree size",
            (Arm::Reconfigure, Some(_)) => "Worktree size (sparse)",
            (Arm::Reconfigure, None) => "Worktree size (full)",
        };
        if let Some(size) = disk_usage(&opts.worktree) {
            out.info(&format!("{label}: {size}"));
        }
    }

    match opts.arm {
        Arm::Create => {
            // The caller splices this into its own final document; there is
            // nothing else for this arm to say.
            if opts.json {
                println!("{}", cone_json(cone.as_deref().unwrap_or_default()));
            }
        }
        Arm::Reconfigure => {
            // Back-fill/refresh the sentinel so re-configuring an existing
            // (possibly sentinel-less) worktree keeps it cleanup-eligible
            // (#3548). Only reachable past the registration check above: a
            // sentinel in an unregistered directory authorizes its deletion.
            if let Err(e) = sentinel::write(&opts.worktree, &opts.issue, &opts.branch) {
                Out::error(&format!(
                    "Could not write {} in {}: {e}",
                    sentinel::FILE_NAME,
                    opts.worktree.to_string_lossy()
                ));
                return 1;
            }
            if opts.json {
                println!("{}", reconfigure_document(opts, cone.as_deref()));
            } else {
                out.success(if cone.is_some() {
                    "Sparse-checkout cone applied"
                } else {
                    "Worktree converted to full checkout"
                });
                out.info(&format!("To use this worktree: cd {}", opts.worktree.to_string_lossy()));
            }
        }
    }
    0
}

// ---------------------------------------------------------------------------
// The cone
// ---------------------------------------------------------------------------

/// The caller's paths, then [`ALWAYS_INCLUDE`], then
/// `$LOOM_WORKTREE_ALWAYS_INCLUDE` split the way bash's default `IFS` splits
/// an unquoted expansion (space, tab, newline; empty fields dropped).
///
/// Duplicates are kept: the retired array concatenation kept them, git's
/// `sparse-checkout set` tolerates them, and they are visible in the JSON
/// `cone` field, so removing them would be an observable change for no fix.
#[must_use]
pub fn cone_paths(user: &[OsString], extra: Option<&OsStr>) -> Vec<OsString> {
    let mut cone: Vec<OsString> = user.to_vec();
    cone.extend(ALWAYS_INCLUDE.iter().map(OsString::from));
    if let Some(extra) = extra {
        let extra = extra.to_string_lossy();
        cone.extend(
            extra
                .split([' ', '\t', '\n'])
                .filter(|field| !field.is_empty())
                .map(OsString::from),
        );
    }
    cone
}

/// `["a","b"]` — the retired `awk` builder's exact spacing (none), with each
/// element escaped as a JSON string, which the retired builder did not do.
#[must_use]
pub fn cone_json(cone: &[OsString]) -> String {
    let items: Vec<String> = cone
        .iter()
        .map(|p| json_string(&p.to_string_lossy()))
        .collect();
    format!("[{}]", items.join(","))
}

fn json_string(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_else(|_| "\"\"".to_string())
}

/// The re-configure arm's final document, in the retired field order and
/// spacing: `{"success": true, "worktreePath": …, "branchName": …,
/// "issueNumber": N, "sparse": …, "cone": […]}`.
fn reconfigure_document(opts: &Options, cone: Option<&[OsString]>) -> String {
    format!(
        r#"{{"success": true, "worktreePath": {}, "branchName": {}, "issueNumber": {}, "sparse": {}, "cone": {}}}"#,
        json_string(&logical_absolute(&opts.worktree).to_string_lossy()),
        json_string(&opts.branch),
        opts.issue,
        cone.is_some(),
        cone_json(cone.unwrap_or_default()),
    )
}

// ---------------------------------------------------------------------------
// git
// ---------------------------------------------------------------------------

/// A `git sparse-checkout` step that failed, with what git said about it.
struct GitFailure {
    step: &'static str,
    code: Option<i32>,
    stderr: String,
}

/// `git -C <wt> sparse-checkout init --cone`, then `… set <cone>`.
///
/// `init` writes `core.sparseCheckout`/`core.sparseCheckoutCone` to the
/// per-worktree config (`config.worktree`), NOT the shared `.git/config` —
/// which is what keeps a sparse worktree from breaking later
/// `actions/checkout` runs on the main clone. `set` replaces the cone, so the
/// same paths twice is a no-op and different paths replace it.
fn apply_cone(wt: &Path, cone: &[OsString]) -> Result<(), GitFailure> {
    git_step(wt, "sparse-checkout init --cone", &["sparse-checkout", "init", "--cone"])?;
    let mut args: Vec<&OsStr> = vec![OsStr::new("sparse-checkout"), OsStr::new("set")];
    args.extend(cone.iter().map(OsString::as_os_str));
    git_step(wt, "sparse-checkout set", &args)
}

fn git_step<S: AsRef<OsStr>>(wt: &Path, step: &'static str, args: &[S]) -> Result<(), GitFailure> {
    let output = Command::new("git")
        .arg("-C")
        .arg(wt)
        .args(args)
        .stdin(Stdio::null())
        .output();
    match output {
        Ok(o) if o.status.success() => Ok(()),
        Ok(o) => Err(GitFailure {
            step,
            code: o.status.code(),
            stderr: String::from_utf8_lossy(&o.stderr).trim().to_string(),
        }),
        Err(e) => Err(GitFailure {
            step,
            code: None,
            stderr: e.to_string(),
        }),
    }
}

/// `git -C <wt> checkout >/dev/null 2>&1 || true` — materialize the files the
/// cone selects (or, after `disable`, the whole tree).
fn materialize(wt: &Path) {
    let _ = quiet_git(wt, &["checkout"]);
}

/// `sparse-checkout disable`, or — when that fails — unset the two
/// per-worktree keys by hand; then re-materialize the full tree. Every step is
/// best-effort, as it was: `--full` on an already-full worktree is a no-op.
fn disable(wt: &Path) {
    if !quiet_git(wt, &["sparse-checkout", "disable"]) {
        let _ = quiet_git(wt, &["config", "--unset", "core.sparseCheckout"]);
        let _ = quiet_git(wt, &["config", "--unset", "core.sparseCheckoutCone"]);
    }
    materialize(wt);
}

/// Run `git -C <wt> <args>` with every stream discarded; report success.
fn quiet_git(wt: &Path, args: &[&str]) -> bool {
    Command::new("git")
        .arg("-C")
        .arg(wt)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// Report a rejected cone — the case the retired script exited 128 on in
/// silence. Always stderr (both modes: `--json` only constrains stdout); the
/// re-configure arm also emits a failure document, since under `--json` it
/// owns stdout. The create arm does not: its stdout is the caller's captured
/// cone, and the caller exits on this code without printing a document, as
/// the retired script did.
fn report_git_failure(opts: &Options, failure: &GitFailure) {
    let code = failure
        .code
        .map_or_else(|| "no exit code".to_string(), |c| format!("exit {c}"));
    let detail = if failure.stderr.is_empty() {
        String::new()
    } else {
        format!(": {}", failure.stderr)
    };
    Out::error(&format!(
        "git {} failed in {} ({code}){detail}",
        failure.step,
        opts.worktree.to_string_lossy()
    ));
    match opts.arm {
        Arm::Create => eprintln!(
            "The worktree was created without a checkout. Re-run \
             `./.loom/scripts/worktree.sh {} --sparse <paths...>` with directory \
             paths (cone mode takes directories, not patterns) to configure it in \
             place, or remove it with `./.loom/scripts/worktree.sh remove {}`.",
            opts.issue, opts.issue
        ),
        Arm::Reconfigure => {
            if opts.json {
                println!(
                    r#"{{"success": false, "error": "sparse-checkout-failed", "worktreePath": {}, "detail": {}}}"#,
                    json_string(&logical_absolute(&opts.worktree).to_string_lossy()),
                    json_string(&failure.stderr),
                );
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Paths and sizes
// ---------------------------------------------------------------------------

/// `cd "$path" && pwd` for the absolute path the caller always passes: bash's
/// logical `cd` drops `.` components, repeated and trailing slashes, and
/// resolves `..` lexically against the logical path — never through a
/// symlink. A relative input is anchored at the process cwd first.
#[must_use]
pub fn logical_absolute(path: &Path) -> PathBuf {
    let anchored = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("/"))
            .join(path)
    };
    let mut normal = PathBuf::new();
    for component in anchored.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                // `/..` is `/`, as it is for bash.
                if normal.parent().is_some() {
                    normal.pop();
                }
            }
            other => normal.push(other),
        }
    }
    normal
}

/// `du -sh "$wt" 2>/dev/null | awk '{print $1}'`: the human-readable total,
/// or `None` when `du` printed nothing.
fn disk_usage(wt: &Path) -> Option<String> {
    let out = Command::new("du")
        .arg("-sh")
        .arg(wt)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    text.lines()
        .next()
        .and_then(|line| line.split_whitespace().next())
        .map(str::to_string)
}

#[cfg(test)]
mod tests;
