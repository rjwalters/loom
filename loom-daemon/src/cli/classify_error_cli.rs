//! `loom-daemon classify-error` (#11208): the classifier behind
//! `defaults/scripts/lib/classify-error.sh`, as a subcommand.
//!
//! Contract (the shell library and its consumers branch on every part of
//! this):
//!
//! * **Classify mode** (the default): the error text arrives on stdin — or in
//!   `--stdout-tail` / `--stderr-tail` files, for a caller that only has tail
//!   captures on disk (concatenated in that order when both are given).
//!   Prints exactly one category line — the shell library's uppercase enum —
//!   and exits `0`. `2` is a usage error (missing `--exit-code`, unreadable
//!   tail file); stdout stays empty on `2` so the shell library's delegation
//!   reads it as "could not ask" and gives its degraded answer.
//! * **Predicate mode** (`--transient <CATEGORY>`): mirrors
//!   `classification_is_transient` — exit `0` when a retry loop should retry
//!   that category, `1` when it is terminal. The deny-list answer travels in
//!   the exit code, never stdout; stdin is deliberately never read in this
//!   mode, so a caller can invoke it bare without stealing a terminal.
//!
//! The provider default chain (`--provider` > `$LOOM_RUNTIME` > `claude`) and
//! the category precedence live in [`loom_daemon::classify_error`]; this file
//! is argv plumbing only.

use anyhow::Result;
use loom_daemon::classify_error;
use std::io::Read as _;
use std::path::PathBuf;

#[derive(clap::Args)]
pub(crate) struct ClassifyErrorArgs {
    /// The child's exit code (`classify_error`'s 2nd positional). Required in
    /// classify mode; unused by `--transient`.
    #[arg(long = "exit-code", value_name = "N")]
    pub(crate) exit_code: Option<i32>,

    /// Provider table to consult (`classify_error`'s optional 3rd
    /// positional). Defaults to `$LOOM_RUNTIME`, then `claude` — the same
    /// precedence as the shell.
    #[arg(long, value_name = "PROVIDER")]
    pub(crate) provider: Option<String>,

    /// Read the error text from this file instead of stdin.
    #[arg(long = "stdout-tail", value_name = "FILE")]
    pub(crate) stdout_tail: Option<PathBuf>,

    /// Read a second capture (e.g. the stderr tail) and classify it after
    /// `--stdout-tail`'s text.
    #[arg(long = "stderr-tail", value_name = "FILE")]
    pub(crate) stderr_tail: Option<PathBuf>,

    /// Predicate mode: do not classify — answer whether CATEGORY is
    /// retryable. Exit 0 retryable, 1 terminal (`classification_is_transient`'s
    /// deny-list, #4501).
    #[arg(long, value_name = "CATEGORY")]
    pub(crate) transient: Option<String>,
}

impl ClassifyErrorArgs {
    /// Never returns: exits 0 (classified / transient), 1 (terminal), or
    /// 2 (usage) — the documented contract.
    pub(crate) fn run(self) -> Result<()> {
        if let Some(category) = &self.transient {
            std::process::exit(if classify_error::classification_is_transient(category) {
                0
            } else {
                1
            });
        }

        let Some(exit_code) = self.exit_code else {
            eprintln!("classify-error: --exit-code N is required (or use --transient CATEGORY)");
            std::process::exit(2);
        };

        let output = self.gather_input();
        let provider = classify_error::resolve_provider(self.provider.as_deref());
        println!("{}", classify_error::classify(&output, exit_code, &provider));
        std::process::exit(0);
    }

    /// The error text: the tail files when given (stdout then stderr, so a
    /// combined read reads in capture order), else stdin.
    fn gather_input(&self) -> String {
        if self.stdout_tail.is_some() || self.stderr_tail.is_some() {
            let mut out = String::new();
            for path in [&self.stdout_tail, &self.stderr_tail] {
                let Some(path) = path else { continue };
                match std::fs::read_to_string(path) {
                    Ok(text) => out.push_str(&text),
                    Err(e) => {
                        eprintln!("classify-error: cannot read {}: {e}", path.display());
                        std::process::exit(2);
                    }
                }
            }
            return out;
        }
        let mut buf = String::new();
        // Invalid UTF-8 in a child transcript must not fail the
        // classification; lossy conversion matches what the shell's `printf`
        // would have handed grep anyway. A stdin read failure exits 2 with
        // empty stdout, which the shell library's delegation reads as
        // "could not ask" and falls back on.
        let mut bytes = Vec::new();
        match std::io::stdin().read_to_end(&mut bytes) {
            Ok(_) => buf.push_str(&String::from_utf8_lossy(&bytes)),
            Err(e) => {
                eprintln!("classify-error: cannot read stdin: {e}");
                std::process::exit(2);
            }
        }
        buf
    }
}
