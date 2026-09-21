//! `loom-daemon cargo-target-dir` — the creation-time half of the per-worktree
//! target-dir scheme (issue #8458), behind a CLI so `worktree.sh` and
//! `spawn-claude.sh` can use it without growing the shell budget's portable
//! pool (epic #7810, `.loom/docs/shell-language-policy.md`).
//!
//! Two verbs, deliberately narrow:
//!
//! * `provision <worktree>` — decide, create the directory, write the marker.
//!   Prints the directory on stdout when there is one, so the caller can
//!   `export LOOM_WORKTREE_CARGO_TARGET_DIR="$(...)"`.
//! * `path <worktree>` — what a worktree that does not exist yet *would* get.
//!   The spawn path runs before the sweep creates the worktree, so it cannot
//!   read a marker; the derivation is idempotent, so the marker `provision`
//!   later writes names this same directory rather than nesting a second level.
//!
//! **Exit 0 in both cases, always.** "The feature is off", "this host has no
//! redirect", and "there is nothing to do" are answers, not errors, and a
//! non-zero status here would abort a worktree creation over a build-cache
//! optimisation. Empty stdout is how the caller learns there is no directory —
//! the same shape `skip-labels`/`worktree-state` use.

use std::path::PathBuf;

use anyhow::Result;

use loom_daemon::worktree_ops::cargo_target::provision;

#[derive(clap::Subcommand)]
pub(crate) enum CargoTargetDirCommand {
    /// Provision (or reuse) this worktree's own cargo target dir and record it
    /// in the worktree's `.loom-cargo-target-dir` marker. Prints the directory,
    /// or nothing when the feature is off / the host needs no redirect.
    Provision(ProvisionArgs),

    /// Print the per-worktree target dir a not-yet-created worktree would get,
    /// or nothing. Reads no marker and writes nothing except the directory
    /// itself.
    Path(PathArgs),
}

#[derive(clap::Args)]
pub(crate) struct ProvisionArgs {
    /// The worktree to provision for. Must exist on disk — `cargo metadata`
    /// needs its manifest.
    #[arg(value_name = "WORKTREE")]
    pub worktree: PathBuf,

    /// Repo root whose `cargo.perWorktreeTargetDir` decides the opt-in.
    /// Defaults to the current directory.
    #[arg(long, value_name = "PATH")]
    pub repo_root: Option<PathBuf>,

    /// Also print the human status line (to stderr) that `worktree.sh` shows.
    #[arg(long)]
    pub report: bool,
}

#[derive(clap::Args)]
pub(crate) struct PathArgs {
    /// The worktree path to derive for. Need not exist yet.
    #[arg(value_name = "WORKTREE")]
    pub worktree: PathBuf,

    /// Repo root whose `cargo.perWorktreeTargetDir` decides the opt-in, and
    /// whose cargo configuration supplies the shared root. Defaults to `.`.
    #[arg(long, value_name = "PATH")]
    pub repo_root: Option<PathBuf>,

    /// Create the directory as well as printing it. The spawn path needs it to
    /// exist before it exports `CARGO_TARGET_DIR`.
    #[arg(long)]
    pub create: bool,
}

impl CargoTargetDirCommand {
    pub(crate) fn run(self) -> Result<()> {
        match self {
            Self::Provision(a) => a.run(),
            Self::Path(a) => a.run(),
        }
    }
}

fn repo_root_or_cwd(explicit: Option<PathBuf>) -> PathBuf {
    explicit.unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))
}

impl ProvisionArgs {
    fn run(self) -> Result<()> {
        let root = repo_root_or_cwd(self.repo_root);
        let outcome = provision::provision(&root, &self.worktree);
        if self.report {
            if let Some(line) = outcome.report_line() {
                eprintln!("  {line}");
            }
        }
        if let Some(dir) = outcome.dir() {
            println!("{}", dir.display());
        }
        Ok(())
    }
}

impl PathArgs {
    fn run(self) -> Result<()> {
        let root = repo_root_or_cwd(self.repo_root);
        if let Some(dir) = provision::planned_dir(&root, &self.worktree) {
            if self.create && std::fs::create_dir_all(&dir).is_err() {
                // Could not create it ⇒ do not name it: a caller that exported
                // a CARGO_TARGET_DIR cargo then cannot write to would fail
                // every build in the sweep.
                return Ok(());
            }
            println!("{}", dir.display());
        }
        Ok(())
    }
}
