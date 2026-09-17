//! The report line: `<ts> [<LEVEL>] <msg>`, appended to the watchdog log and
//! echoed to stderr where the supervisor captures it.
//!
//! The shape is contract. The retained suite greps the log for `DIVERGENCE`,
//! `[OK]`, `DEGRADED` and `UNKNOWN`, and an operator's log-scraper does the
//! same, so neither the bracketed level nor the timestamp format may drift.

use std::io::Write;
use std::path::{Path, PathBuf};

/// Severity, rendered verbatim inside the brackets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Ok,
    Warn,
    Degraded,
    Unknown,
    Divergence,
}

impl Level {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Level::Ok => "OK",
            Level::Warn => "WARN",
            Level::Degraded => "DEGRADED",
            Level::Unknown => "UNKNOWN",
            Level::Divergence => "DIVERGENCE",
        }
    }
}

/// ANSI colours, applied only when stderr is a terminal — the shell gated on
/// `[[ -t 2 ]]`, and the retained suite redirects stderr to a file, so under
/// test the output is always plain.
fn colour_for(level: Level) -> &'static str {
    match level {
        Level::Divergence => "\x1b[0;31m",
        Level::Ok => "\x1b[0;32m",
        _ => "\x1b[1;33m",
    }
}

fn stderr_is_tty() -> bool {
    // SAFETY: `isatty` takes a file descriptor and has no preconditions beyond
    // it being a valid int; it cannot alias or free anything.
    unsafe { libc::isatty(libc::STDERR_FILENO) == 1 }
}

/// Emits report lines and carries the per-tick divergence state that
/// [`Reporter::heartbeat_ok`] and the exit code both depend on.
pub struct Reporter {
    log_path: PathBuf,
    verbose: bool,
    colours: bool,
    /// Set when the IPC probe diverged on this tick. Drives both the
    /// `DEGRADED`-instead-of-`OK` substitution below and the tick's exit code.
    pub probe_diverged: bool,
    /// Overrides the historical note text for a divergence that did NOT come
    /// from this tick's own round-trip (#5944 — the windowed/rate signal can
    /// fire on a tick whose round-trip actually succeeded, and pointing the
    /// reader at a `DIVERGENCE` line that was never printed is a lie).
    pub diverged_note: Option<String>,
}

impl Reporter {
    #[must_use]
    pub fn new(log_path: PathBuf, verbose: bool) -> Self {
        Self {
            log_path,
            verbose,
            colours: stderr_is_tty(),
            probe_diverged: false,
            diverged_note: None,
        }
    }

    /// Force colours on or off, for tests that must assert on plain text.
    pub fn set_colours(&mut self, on: bool) {
        self.colours = on;
    }

    /// Append to the log and echo to stderr.
    ///
    /// Every filesystem failure here is swallowed, exactly as the shell's
    /// `|| true` did on both the `mkdir -p` and the append: a watchdog that
    /// dies because its own log directory is unwritable reports nothing at all,
    /// which is strictly worse than reporting only to stderr.
    pub fn report(&self, level: Level, msg: &str) {
        let ts = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ");
        let line = format!("{ts} [{}] {msg}", level.as_str());

        if let Some(dir) = self.log_path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.log_path)
        {
            let _ = writeln!(f, "{line}");
        }

        // OK is the quiet level: logged always, echoed only under --verbose.
        if level == Level::Ok && !self.verbose {
            return;
        }
        if self.colours {
            eprintln!("{}{line}\x1b[0m", colour_for(level));
        } else {
            eprintln!("{line}");
        }
    }

    /// The heartbeat section's OK-shaped exits (#5790).
    ///
    /// Before #5790 these called `report OK` unconditionally, so a tick that
    /// had already logged a sub-threshold `DIVERGENCE` still ended with a clean
    /// `[OK] daemon healthy` line. An operator reading the last line, or a
    /// scraper grepping `[OK]`, saw a clean bill of health in the same tick a
    /// divergence was reported moments earlier. The exit code was already
    /// correct; this closes the matching gap in the log TEXT.
    pub fn heartbeat_ok(&self, msg: &str) {
        if self.probe_diverged {
            let note = self.diverged_note.clone().unwrap_or_else(|| {
                "the IPC probe diverged earlier this tick (see the DIVERGENCE line above) — \
                 dispatch may be degraded despite a fresh/liveness-only-OK heartbeat signal; \
                 the exit code for this tick reflects the divergence, not this line."
                    .to_string()
            });
            self.report(Level::Degraded, &format!("{msg} NOTE: {note}"));
        } else {
            self.report(Level::Ok, msg);
        }
    }

    #[must_use]
    pub fn log_path(&self) -> &Path {
        &self.log_path
    }
}
