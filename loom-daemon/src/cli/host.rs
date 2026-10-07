//! `loom-daemon host disable|enable|status|check` — the durable host opt-out
//! (Issue #10179). The marker logic lives in [`loom_daemon::host_optout`]; this
//! is the thin CLI over it (and the reason it is not in the frozen `main.rs`).

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::Result;
use clap::Subcommand;
use loom_daemon::host_optout::{self, State};

#[derive(Subcommand)]
pub(crate) enum HostCommand {
    /// Opt this host out of autonomy: stop the daemon, remove its launchd/systemd
    /// daemon + watchdog jobs and the autonomy-desired marker, and write the
    /// `autonomy-disabled` marker. Every start / re-provision path then refuses
    /// until `host enable`. Idempotent.
    Disable {
        /// Why (required; shown by every refusal, `status` and `health`).
        #[arg(long)]
        reason: String,
    },
    /// Clear the opt-out marker. Does NOT start anything; run
    /// `loom-daemon-start.sh` afterwards.
    Enable,
    /// Print whether this host is disabled (always exits 0).
    Status,
    /// Exit 10, naming reason/who/when, when the host is disabled; 0 otherwise.
    /// For shell entry points (installer, resync-installed.sh) to call first;
    /// they refuse only on 10, so an older binary that exits 1/2 for an unknown
    /// subcommand is never mistaken for an opt-out.
    Check {
        /// What to name in the refusal.
        #[arg(long, default_value = "host check")]
        entry_point: String,
    },
}

impl HostCommand {
    pub(crate) fn run(self) -> Result<()> {
        match self {
            Self::Disable { reason } => disable(&reason),
            Self::Enable => enable(),
            Self::Status => {
                let s = host_optout::read_current();
                match host_optout::summary(&s) {
                    t if t.is_empty() => println!("enabled (no host opt-out marker)"),
                    t => println!("{t}"),
                }
                Ok(())
            }
            Self::Check { entry_point } => {
                host_optout::refuse_if_disabled_exit_with(
                    &entry_point,
                    host_optout::HOST_CHECK_DISABLED_EXIT,
                );
                Ok(())
            }
        }
    }
}

/// When the host is disabled, print `disabled by operator: <reason> (<when>)`
/// (or a JSON object) and return `true`; callers then skip their "daemon not
/// running" / repair-me verdict.
pub(crate) fn report_disabled(json: bool) -> bool {
    let line = host_optout::summary(&host_optout::read_current());
    if line.is_empty() {
        return false;
    }
    if json {
        println!("{}", serde_json::json!({ "host_disabled": true, "summary": line }));
    } else {
        println!("{line}");
    }
    true
}

fn marker_paths() -> Option<(PathBuf, PathBuf)> {
    host_optout::current_paths()
}

fn fail(msg: &str) -> ! {
    eprintln!("loom-daemon host: {msg}");
    std::process::exit(1);
}

fn disable(reason: &str) -> Result<()> {
    let Some((desired, disabled)) = marker_paths() else {
        fail("could not resolve the machine-level Loom dir");
    };
    // Marker FIRST: a watchdog tick or relaunch racing this command must lose.
    match host_optout::write_at(&disabled, reason, &host_optout::current_who()) {
        Ok(true) => println!("Wrote {}", disabled.display()),
        Ok(false) => {
            println!("Already disabled ({}); keeping the original record.", disabled.display())
        }
        Err(e) => fail(&e),
    }
    let _ = std::fs::remove_file(&desired);
    let _ = loom_daemon::operator_stop::discard(&desired);

    // Stop the daemon and tear down its (and the watchdog's) supervisor jobs
    // through the existing stop path - one implementation of that teardown.
    let ok = match locate_stop_script() {
        Some(script) => match Command::new(&script).status() {
            Ok(st) if st.success() => true,
            Ok(st) => {
                eprintln!("loom-daemon host: {} exited {st}", script.display());
                false
            }
            Err(e) => {
                eprintln!("loom-daemon host: could not run {}: {e}", script.display());
                false
            }
        },
        None => {
            eprintln!(
                "loom-daemon host: loom-daemon-stop.sh not found; no running daemon was stopped."
            );
            false
        }
    };
    println!("{}", host_optout::summary(&host_optout::read_at(&disabled)));
    if !ok {
        fail("the marker is written (starts are refused) but the daemon/jobs may still be running; fix the above and re-run `host disable`");
    }
    println!("Re-enable with: loom-daemon host enable");
    Ok(())
}

fn enable() -> Result<()> {
    let Some((_, disabled)) = marker_paths() else {
        fail("could not resolve the machine-level Loom dir");
    };
    match host_optout::clear_at(&disabled) {
        Ok(true) => println!("Removed {}.", disabled.display()),
        Ok(false) => println!("Host was not disabled; nothing to do."),
        Err(e) => fail(&e),
    }
    println!("Nothing was started. Next step: run ./.loom/scripts/cli/loom-daemon-start.sh");
    debug_assert!(matches!(host_optout::read_at(&disabled), State::Enabled));
    Ok(())
}

/// `LOOM_HOST_STOP_SCRIPT`, then the nearest `.loom/scripts/cli/` up from the
/// cwd, then `~/.loom/`, then `$PATH`.
fn locate_stop_script() -> Option<PathBuf> {
    const REL: &str = ".loom/scripts/cli/loom-daemon-stop.sh";
    if let Some(p) = std::env::var_os("LOOM_HOST_STOP_SCRIPT").filter(|v| !v.is_empty()) {
        return Some(PathBuf::from(p));
    }
    let mut dirs: Vec<PathBuf> = Vec::new();
    if let Some(m) = std::env::var_os("LOOM_MACHINE_CHECKOUT").filter(|v| !v.is_empty()) {
        dirs.push(PathBuf::from(m));
    }
    if let Ok(cwd) = std::env::current_dir() {
        dirs.extend(cwd.ancestors().map(Path::to_path_buf));
    }
    if let Some(h) = dirs::home_dir() {
        dirs.push(h);
    }
    dirs.iter()
        .map(|d| d.join(REL))
        .find(|p| p.is_file())
        .or_else(|| {
            std::env::split_paths(&std::env::var_os("PATH")?)
                .map(|d| d.join("loom-daemon-stop.sh"))
                .find(|p| p.is_file())
        })
}
