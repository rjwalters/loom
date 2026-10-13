//! `loom-daemon affected-files check` — Curator's Affected-Files preflight
//! (issue #9400). The classifier, its marker grammar and the exit-code
//! contract live in [`loom_daemon::work_finder::affected_files::check`]; this
//! file is the argument surface.

use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::Result;
use loom_daemon::work_finder::affected_files::check::{check, EXIT_COULD_NOT_RUN};

#[derive(clap::Subcommand)]
pub(crate) enum AffectedFilesCommand {
    /// Classify each `## Affected Files` path of an issue body against the
    /// build base. No forge call.
    ///
    /// Prints one `<class>\t<path>` line per listed path:
    ///
    ///   present      on the base tree (a bare file name matches by name)
    ///   new          absent from the base, marked new: `(new)`, `(new file)`,
    ///                `(create)` or `(created)` on its line outside backticks,
    ///                or listed under a `### New ...` sub-heading
    ///   unknown      absent from the base and the working tree, unmarked
    ///   uncommitted  absent from the base, present in the working tree,
    ///                unmarked: a fresh Builder worktree would lack it
    ///
    /// Exit codes (the worst class decides):
    ///
    ///    0  clear: every path is present or new, or no path is listed
    ///   10  unknown: ask the author, do not park
    ///   11  uncommitted: the only parking verdict
    ///    3  could not run (body unreadable, not a git checkout, base ref
    ///       unreachable): not a verdict
    ///
    /// Any other code (2 from a daemon that predates this verb, 127) also
    /// means the check did not run. It is never an all-clear and never a
    /// reason to park.
    #[command(verbatim_doc_comment)]
    Check(CheckArgs),
}

#[derive(clap::Args)]
pub(crate) struct CheckArgs {
    /// File holding the issue body; `-` reads stdin.
    #[arg(long = "body-file", value_name = "PATH")]
    body_file: PathBuf,

    /// Ref the Builder's worktree is cut from.
    #[arg(long, value_name = "REF", default_value = "origin/main")]
    base: String,

    /// Any directory inside the checkout to compare against (default: the
    /// current directory).
    #[arg(long, value_name = "DIR")]
    workspace: Option<PathBuf>,
}

impl AffectedFilesCommand {
    /// Never returns: exits with the code documented on [`Self::Check`].
    pub(crate) fn run(self) -> Result<()> {
        let Self::Check(args) = self;
        let (code, stdout) = args.evaluate();
        print!("{stdout}");
        std::process::exit(code);
    }
}

impl CheckArgs {
    /// The exit code and stdout of the check; every failure to run is
    /// reported on stderr and mapped to [`EXIT_COULD_NOT_RUN`].
    fn evaluate(&self) -> (i32, String) {
        let outcome = read_body(&self.body_file).and_then(|body| {
            let workspace = self.workspace.clone().unwrap_or_else(|| PathBuf::from("."));
            check(&body, &workspace, &self.base)
        });
        match outcome {
            Ok(report) => (report.exit_code(), report.render()),
            Err(reason) => {
                eprintln!("loom-daemon affected-files check: could not run: {reason}");
                (EXIT_COULD_NOT_RUN, String::new())
            }
        }
    }
}

fn read_body(path: &Path) -> Result<String, String> {
    if path == Path::new("-") {
        let mut body = String::new();
        std::io::stdin()
            .read_to_string(&mut body)
            .map(|_| body)
            .map_err(|e| format!("could not read the body from stdin: {e}"))
    } else {
        std::fs::read_to_string(path)
            .map_err(|e| format!("could not read body file {}: {e}", path.display()))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
#[path = "affected_files_check_tests.rs"]
mod tests;
