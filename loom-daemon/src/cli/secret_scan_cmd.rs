//! `loom-daemon secret-scan` — refuse content carrying a credential shape
//! (#9133). The scan itself lives in [`loom_daemon::secret_scan`]; this is the
//! argument surface the three callers share:
//!
//! - `guard-loom-workflow.sh` (PreToolUse, every fleet repo): `--for-command`
//! - `.githooks/pre-commit` / `.githooks/pre-push`: `--staged` / `--pre-push`
//! - CI (`Daemon Checks`): `--range <base>..<head>`
//!
//! Exit 0 clean, 1 finding(s), 2 could not scan. Findings go to stderr, one
//! per line, as `path:line[ @commit]: class (fp <8-hex>)` — never the value.

use std::io::Read;
use std::path::PathBuf;

use anyhow::{Context, Result};
use loom_daemon::secret_scan::{modes_for_command, scan, Mode, Scanner};

const EX_FOUND: i32 = 1;
const EX_ERROR: i32 = 2;

#[derive(clap::Args)]
#[command(group(clap::ArgGroup::new("mode").required(true)))]
pub(crate) struct SecretScanArgs {
    /// Index vs HEAD (a git pre-commit hook).
    #[arg(long, group = "mode")]
    staged: bool,

    /// Staged + unstaged + untracked-unignored: everything a
    /// `git add -A && git commit` could sweep in.
    #[arg(long, group = "mode")]
    pending: bool,

    /// Every commit in `git log <REVS>…`, one at a time (e.g. `origin/main..HEAD`).
    #[arg(long, group = "mode", value_name = "REVS", num_args = 1.., allow_hyphen_values = true)]
    range: Option<Vec<String>>,

    /// Commits reachable from HEAD but from no remote-tracking ref.
    #[arg(long, group = "mode")]
    unpushed: bool,

    /// Read git's pre-push hook stdin and scan every commit being pushed.
    #[arg(long, group = "mode")]
    pre_push: bool,

    /// Pick the modes from a shell command's text: `git … commit` scans
    /// pending changes, `git … push` scans unpushed commits, anything else
    /// exits 0 without touching git. For the PreToolUse guard.
    #[arg(long, group = "mode", value_name = "COMMAND")]
    for_command: Option<String>,

    /// Repo to scan. Defaults to the one containing the current directory.
    #[arg(long, value_name = "PATH")]
    repo: Option<PathBuf>,

    /// Fingerprint allowlist. Defaults to `<repo>/.loom/secret-scan-allow`.
    #[arg(long, value_name = "FILE")]
    allow_file: Option<PathBuf>,
}

impl SecretScanArgs {
    fn modes(&self) -> Result<Vec<Mode>> {
        Ok(if self.staged {
            vec![Mode::Staged]
        } else if self.pending {
            vec![Mode::Pending]
        } else if let Some(revs) = &self.range {
            vec![Mode::Range(revs.clone())]
        } else if self.unpushed {
            vec![Mode::Unpushed]
        } else if self.pre_push {
            let mut stdin = String::new();
            std::io::stdin()
                .read_to_string(&mut stdin)
                .context("could not read pre-push stdin")?;
            vec![Mode::PrePush(stdin)]
        } else {
            modes_for_command(self.for_command.as_deref().unwrap_or_default())
        })
    }

    fn scan(&self) -> Result<usize> {
        let modes = self.modes()?;
        if modes.is_empty() {
            return Ok(0);
        }
        let start = self.repo.clone().unwrap_or_else(|| PathBuf::from("."));
        let top = std::process::Command::new("git")
            .arg("-C")
            .arg(&start)
            .args(["rev-parse", "--show-toplevel"])
            .output()
            .context("could not run git")?;
        anyhow::ensure!(top.status.success(), "{} is not inside a git work tree", start.display());
        let repo = PathBuf::from(String::from_utf8_lossy(&top.stdout).trim());
        let scanner = Scanner::for_repo(&repo, self.allow_file.as_deref());
        let mut found = 0;
        for mode in &modes {
            for finding in scan(&scanner, &repo, mode)? {
                eprintln!("secret-scan: {finding}");
                found += 1;
            }
        }
        Ok(found)
    }

    pub(crate) fn run(self) -> Result<()> {
        let code = match self.scan() {
            Ok(0) => 0,
            Ok(n) => {
                eprintln!(
                    "secret-scan: {n} credential-shaped value(s) found. Values are never printed."
                );
                eprintln!("secret-scan: if one is real, remove it and ROTATE it: it is disclosed once pushed.");
                eprintln!("secret-scan: a synthetic fixture is allowed by adding its fp to .loom/secret-scan-allow.");
                EX_FOUND
            }
            Err(e) => {
                eprintln!("secret-scan: could not scan: {e:#}");
                EX_ERROR
            }
        };
        std::process::exit(code);
    }
}
