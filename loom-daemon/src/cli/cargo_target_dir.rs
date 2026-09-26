//! `loom-daemon cargo-target-dir` — the creation-time half of the per-worktree
//! target-dir scheme (issue #8458), behind a CLI so `worktree.sh` and
//! `spawn-claude.sh` can use it without growing the shell budget's portable
//! pool (epic #7810, `.loom/docs/shell-language-policy.md`).
//!
//! Six verbs, deliberately narrow. Two write (the creation half):
//!
//! * `provision <worktree>` — decide, create the directory, write the marker.
//!   Prints the directory on stdout when there is one, so the caller can
//!   `export LOOM_WORKTREE_CARGO_TARGET_DIR="$(...)"`.
//! * `path [<worktree> | --issue <N>]` — what a worktree that does not exist
//!   yet *would* get. The spawn path runs before the sweep creates the
//!   worktree, so it cannot read a marker; the derivation is idempotent, so the
//!   marker `provision` later writes names this same directory rather than
//!   nesting a second level. `--issue <N>` resolves the worktree root here
//!   (`worktree_root::worktree_root`) rather than making the caller pre-derive
//!   it in shell.
//!
//! **Both exit 0 always.** "The feature is off", "this host has no redirect",
//! and "there is nothing to do" are answers, not errors, and a non-zero status
//! here would abort a worktree creation over a build-cache optimisation. Empty
//! stdout is how the caller learns there is no directory — the same shape
//! `skip-labels`/`worktree-state` use.
//!
//! Two read (the removal half's predicates, #8486 review):
//!
//! * `is-attributable <worktree> <candidate>…` — exit 0 when ANY candidate
//!   carries the per-worktree shape for `<worktree>`, 1 when none does.
//! * `marker <worktree>` — print the validated marker value, exit 0; exit 1
//!   when there is no usable marker.
//!
//! These two exist so `defaults/scripts/lib/cargo-target-dir.sh` can consult
//! [`per_worktree::is_attributable`] / [`per_worktree::marker_value`] instead of
//! carrying a second bash implementation of them. The *call sites* stay in the
//! bash resolver — this moves only the implementation, so there is still exactly
//! one resolution path (issue #8458's fourth acceptance criterion). Their exit
//! codes are **data, not errors** (the `retry-classify` convention), which is why
//! they are the two verbs here that may exit non-zero.
//!
//! And two that are the removal half itself (issue #9153, a slice of #8191 /
//! epic #7810):
//!
//! * `resolve <worktree>` — the marker-first resolution for a worktree that is
//!   still on disk, i.e. the pre-removal half of #7239's reclaim.
//! * `reclaim <worktree> --resolved <dir>` — decide and act on that directory
//!   once the worktree is gone, printing at most one `LEVEL<TAB>message` record.
//!
//! `merge-pr.sh` removes worktrees with its own `git worktree remove --force`
//! rather than through `worktree.sh remove`, so it could not inherit the Rust
//! reclaim that [`loom_daemon::worktree_ops::clean`], `worktree-remove` and the
//! reaper already share; it carried the last bash copy of the resolve/reclaim
//! *call sequence* instead. These two verbs are that sequence, split exactly
//! where the removal has to happen: `cargo metadata` needs the manifest that is
//! about to disappear, so resolution runs before and the decision after. Both
//! exit 0 always — the merge has already succeeded by the time either runs, so
//! "there is nothing to reclaim" and "the reclaim was refused" are answers, and
//! a non-zero status would only give the caller something to suppress.

use std::path::PathBuf;

use anyhow::Result;

use loom_daemon::worktree_ops::cargo_target::{self, per_worktree, provision};
use loom_daemon::worktree_root::worktree_root;

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

    /// Exit 0 when any CANDIDATE carries the Loom per-worktree shape for
    /// WORKTREE (`<root>/wt/<worktree's own directory name>`), 1 when none
    /// does. Purely structural — the worktree need not still exist.
    IsAttributable(IsAttributableArgs),

    /// Print WORKTREE's recorded per-worktree target dir and exit 0, or exit 1
    /// when there is no usable marker. The worktree MUST still be on disk.
    Marker(MarkerArgs),

    /// Print the cargo target dir WORKTREE actually builds into — its marker
    /// first, then `CARGO_TARGET_DIR`, then `cargo metadata`, degrading to
    /// `<worktree>/target`. The worktree MUST still be on disk.
    Resolve(ResolveArgs),

    /// Reclaim WORKTREE's already-resolved target dir now that the worktree
    /// itself is gone. Prints at most one `LEVEL<TAB>message` record; silent
    /// for the un-redirected layout. Always exits 0.
    Reclaim(ReclaimArgs),
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
    /// The worktree path to derive for. Need not exist yet. Omit it and pass
    /// `--issue <N>` instead to have the worktree root resolved here.
    #[arg(value_name = "WORKTREE")]
    pub worktree: Option<PathBuf>,

    /// Derive for `<worktree root>/issue-<N>` instead of an explicit path,
    /// resolving the worktree root (env > `worktree.root` config > the
    /// `.loom/worktrees` default) through `worktree_root::worktree_root`.
    #[arg(long, value_name = "N", conflicts_with = "worktree")]
    pub issue: Option<String>,

    /// Repo root whose `cargo.perWorktreeTargetDir` decides the opt-in, and
    /// whose cargo configuration supplies the shared root. Defaults to `.`.
    #[arg(long, value_name = "PATH")]
    pub repo_root: Option<PathBuf>,

    /// Create the directory as well as printing it. The spawn path needs it to
    /// exist before it exports `CARGO_TARGET_DIR`.
    #[arg(long)]
    pub create: bool,
}

#[derive(clap::Args)]
pub(crate) struct IsAttributableArgs {
    /// The worktree the candidates are being attributed to. Need not exist.
    #[arg(value_name = "WORKTREE")]
    pub worktree: PathBuf,

    /// One or more candidate paths. Several are accepted because the bash
    /// callers always ask about a path AND its `realpath`, and one subprocess
    /// for the pair is cheaper than two.
    #[arg(value_name = "CANDIDATE", required = true)]
    pub candidates: Vec<PathBuf>,
}

#[derive(clap::Args)]
pub(crate) struct MarkerArgs {
    /// The worktree whose marker to read. Must still be on disk.
    #[arg(value_name = "WORKTREE")]
    pub worktree: PathBuf,
}

#[derive(clap::Args)]
pub(crate) struct ResolveArgs {
    /// The worktree to resolve for. Must still be on disk — `cargo metadata`
    /// needs its manifest, which is exactly why the caller runs this BEFORE
    /// removing it.
    #[arg(value_name = "WORKTREE")]
    pub worktree: PathBuf,
}

#[derive(clap::Args)]
pub(crate) struct ReclaimArgs {
    /// The worktree that was removed. Excluded from the sharing scan by path,
    /// so this is correct whether or not it is still on disk.
    #[arg(value_name = "WORKTREE")]
    pub worktree: PathBuf,

    /// What `resolve` said this worktree built into, captured before the
    /// removal.
    #[arg(long, value_name = "PATH")]
    pub resolved: PathBuf,

    /// Repo root the sharing scan enumerates live worktrees from, and whose own
    /// `target/` the refusal gates protect. Defaults to the current directory.
    #[arg(long, value_name = "PATH")]
    pub repo_root: Option<PathBuf>,

    /// Report what a real pass would remove without removing anything.
    #[arg(long)]
    pub dry_run: bool,
}

impl CargoTargetDirCommand {
    pub(crate) fn run(self) -> Result<()> {
        match self {
            Self::Provision(a) => a.run(),
            Self::Path(a) => a.run(),
            Self::IsAttributable(a) => a.run(),
            Self::Marker(a) => a.run(),
            Self::Resolve(a) => a.run(),
            Self::Reclaim(a) => a.run(),
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
        // `--issue N` ⇒ `<worktree root>/issue-N`, resolved HERE. The spawn path
        // used to derive this itself by sourcing `lib/worktree-root.sh`; that is
        // creation-time delivery logic with a Rust twin already in the binary it
        // is about to call, so it belongs on this side of the boundary.
        let Some(worktree) = self.worktree.or_else(|| {
            self.issue
                .map(|n| worktree_root(&root).join(format!("issue-{n}")))
        }) else {
            // Neither form given: nothing to derive. An answer, not an error —
            // same exit-0 contract as every other not-applicable case here.
            return Ok(());
        };
        if let Some(dir) = provision::planned_dir(&root, &worktree) {
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

impl IsAttributableArgs {
    fn run(self) -> Result<()> {
        let any = self
            .candidates
            .iter()
            .any(|c| per_worktree::is_attributable(&self.worktree, c));
        // Exit code IS the answer, so `std::process::exit` rather than an Err:
        // an `Err` would print an anyhow report the bash caller would then have
        // to suppress, and would conflate "not attributable" with "broke".
        std::process::exit(i32::from(!any));
    }
}

impl MarkerArgs {
    fn run(self) -> Result<()> {
        match per_worktree::marker_value(&self.worktree) {
            Some(dir) => {
                println!("{}", dir.display());
                Ok(())
            }
            // No usable marker — absent, empty, corrupt, or a manifest-less
            // tree. Exit 1 is the answer the bash caller branches on.
            None => std::process::exit(1),
        }
    }
}

impl ResolveArgs {
    fn run(self) -> Result<()> {
        // `resolve_for_worktree`, not `..._checked`: this answer is about the
        // worktree BEING removed, whose unreadable-redirect fallback is
        // `<worktree>/target` — an `Inside` outcome, i.e. a silent no-op. A
        // resolution failure must cost a missed reclaim, never a wrong
        // deletion; the sharing scan inside `reclaim` is where the checked form
        // (which fails closed) is used, and it stays there.
        println!("{}", cargo_target::resolve_for_worktree(&self.worktree).display());
        Ok(())
    }
}

impl ReclaimArgs {
    fn run(self) -> Result<()> {
        let root = repo_root_or_cwd(self.repo_root);
        let outcome = cargo_target::reclaim(&root, &self.worktree, &self.resolved, self.dry_run);
        // At most one record, and none at all for `Inside`/`Absent` — the
        // un-redirected layout, i.e. almost every repo — so post-merge output is
        // unchanged unless something was actually reclaimed or deliberately kept.
        if let Some((level, message)) = outcome.report_record() {
            println!("{level}\t{message}");
        }
        Ok(())
    }
}
