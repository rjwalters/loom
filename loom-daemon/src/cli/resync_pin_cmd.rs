//! `loom-daemon resync-pin …` — pin a file in `.loom/resync-ignore` and record
//! its upstream fork point in `.loom/resync-pin-base` (issue #8726).
//!
//! Brand-new logic, native from the start per the shell-language policy; the
//! vendored `resync-installed.sh` is not modified and never reads the sidecar,
//! so pin protection is identical with or without a daemon that knows this
//! command. Logic lives in [`loom_daemon::resync_pin`]; see
//! `defaults/docs/repo-owned-files.md` §"Recording a pin's fork point".

use std::path::PathBuf;

use anyhow::{anyhow, Result};

use loom_daemon::resync_pin::{self, AddOptions, BaseOutcome};

#[derive(clap::Subcommand)]
pub(crate) enum ResyncPinCommand {
    /// Pin a path (append it to `.loom/resync-ignore` if absent) and record the
    /// upstream Loom commit its content forked from. The base defaults to
    /// `loom_commit` in `.loom/install-metadata.json` for a NEW pin only; an
    /// already-pinned path needs `--base`. An existing recorded base is kept
    /// unless `--replace-base`. A base that cannot be verified in a Loom source
    /// checkout is reported unknown and not recorded (the pin is still written).
    /// Exit 0 when the pin is in place, 1 on an invalid path or write failure.
    Add(AddArgs),

    /// Per-pin fork point and drift (upstream commits and lines changed since
    /// the base), with the `git diff` that answers "can this pin be lifted?".
    /// Pins without a recorded base, or whose base cannot be resolved, are
    /// reported as unknown — never as zero drift. Read-only; exit 0.
    Status(StatusArgs),
}

#[derive(clap::Args)]
pub(crate) struct AddArgs {
    /// Path to pin: the `.loom/resync-ignore` label (`roles/curator.md`) or
    /// the repo-relative installed path (`.loom/roles/curator.md`).
    path: String,
    /// Upstream Loom revision the local content forked from (any rev that
    /// resolves in the source checkout; stored as a full sha).
    #[arg(long, value_name = "REV")]
    base: Option<String>,
    /// Loom source checkout. Defaults to `.loom/loom-source-path`, else the
    /// workspace itself when it is the Loom source repo.
    #[arg(long, value_name = "DIR")]
    source: Option<PathBuf>,
    /// Source path inside the Loom repo, for a label with no known `defaults/`
    /// counterpart (e.g. `defaults/roles/curator.md`).
    #[arg(long, value_name = "PATH")]
    source_path: Option<String>,
    /// Overwrite an existing recorded base (e.g. after rebasing the local
    /// patch onto a newer upstream).
    #[arg(long)]
    replace_base: bool,
    /// Consumer repository root. Defaults to the current directory.
    #[arg(long, value_name = "DIR")]
    workspace: Option<PathBuf>,
}

#[derive(clap::Args)]
pub(crate) struct StatusArgs {
    /// Loom source checkout (same default as `add`).
    #[arg(long, value_name = "DIR")]
    source: Option<PathBuf>,
    /// Upstream revision to measure drift against.
    #[arg(long, value_name = "REV", default_value = "HEAD")]
    upstream: String,
    /// Consumer repository root. Defaults to the current directory.
    #[arg(long, value_name = "DIR")]
    workspace: Option<PathBuf>,
}

fn workspace(w: Option<PathBuf>) -> PathBuf {
    w.or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| PathBuf::from("."))
}

impl ResyncPinCommand {
    pub(crate) fn run(self) -> Result<()> {
        match self {
            Self::Add(a) => {
                let ws = workspace(a.workspace);
                let opts = AddOptions {
                    path: a.path,
                    base: a.base,
                    source: a.source,
                    source_path: a.source_path,
                    replace_base: a.replace_base,
                };
                let r = resync_pin::add_pin(&ws, &opts).map_err(|e| anyhow!(e))?;
                let verb = if r.pinned_now {
                    "pinned"
                } else {
                    "already pinned"
                };
                println!("{verb}: {} ({})", r.label, resync_pin::IGNORE_FILE);
                match &r.base {
                    BaseOutcome::Recorded(e) => {
                        println!("recorded fork point: {} {} (via {})", e.sha, e.source_path, e.via)
                    }
                    BaseOutcome::Kept(e) => println!(
                        "kept existing fork point: {} {} (pass --replace-base to change it)",
                        e.sha, e.source_path
                    ),
                    BaseOutcome::Unknown(why) => {
                        eprintln!("fork point UNKNOWN, nothing recorded: {why}")
                    }
                }
                if let Some(note) = &r.content_note {
                    println!("{note}");
                }
                Ok(())
            }
            Self::Status(s) => {
                let ws = workspace(s.workspace);
                let report = resync_pin::status(&ws, s.source.as_deref(), &s.upstream);
                print!("{}", report.render());
                Ok(())
            }
        }
    }
}
