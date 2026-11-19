//! A private, expiring stdin channel owns one persistent-container invocation.
//! No PID files or account-wide cancellation: Linux subreaping keeps even
//! setsid/double-fork descendants owned until they have all been reaped.

mod claude;
mod host;
#[cfg(test)]
pub(crate) use host::exec_command;
pub use host::run as run_host;
mod owner;
pub mod posture;
pub mod refusal;
#[cfg(target_os = "linux")]
mod worker;

use std::sync::atomic::{AtomicI32, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub const PROTOCOL: &str = "loom-session-exec-v1";
pub const LEASE_MS: u64 = 2000;
/// How long the worker waits for its FIRST lease before refusing launch
/// (#9067). A docker exec attach under CI load can take longer than one lease
/// horizon (`LEASE_MS`) to deliver the first heartbeat; refusing at `LEASE_MS`
/// 143'd healthy dispatches before the command ever started. Nothing is at
/// risk before launch, so patience here is free: EOF and the explicit `cancel`
/// line still refuse instantly, and a channel silent for the whole window
/// expires loudly. Run-time cancellation is NOT governed by this constant — a
/// running invocation is cancelled only by decisive channel events or by a
/// channel silent for `LEASE_MS` (the wedged-host revocation contract, kept at
/// its original timing; any arriving bytes reset that silence clock).
pub const STALL_MS: u64 = 30_000;
pub const GRACE: Duration = Duration::from_millis(1000);
pub const POLL: Duration = Duration::from_millis(20);
static SIGNAL: AtomicI32 = AtomicI32::new(0);

#[derive(clap::Subcommand)]
pub enum SessionExecCommand {
    /// Feature check. Old images must be updated; no unsupervised fallback.
    Protocol,
    /// Host transport; owns heartbeats, cancellation and cleanup acknowledgement.
    Host(HostArgs),
    /// Run the Claude adapter in the host-selected account's persistent seat.
    ClaudeSeat(claude::Args),
    /// Linux container endpoint. Stdin is the lease, never worker input.
    Worker(WorkerArgs),
    /// May Codex run with its own sandbox off in this container? Checks the
    /// posture label AND the actual HostConfig (issue #9979).
    Posture(posture::PostureArgs),
}

#[derive(clap::Args)]
pub struct HostArgs {
    /// Original launcher, before the adapter forks its captured child.
    #[arg(long)]
    pub owner_pid: Option<i32>,
    #[arg(long)]
    pub container: String,
    #[arg(long)]
    pub workdir: String,
    #[arg(long = "env")]
    pub env: Vec<String>,
    /// Variables set on the `docker` client process itself, never in argv
    /// (#13916): a forge token this host resolved for the exec, which `env`
    /// names as a bare `NAME` so `docker` copies it into the container
    /// process. Never a CLI flag, so no caller can pass a secret on argv.
    #[arg(skip)]
    pub child_env: Vec<(String, String)>,
    #[arg(long, env = "LOOM_SESSION_STDERR_FILE")]
    pub stderr_file: Option<std::path::PathBuf>,
    #[arg(last = true, required = true)]
    pub command: Vec<String>,
}

#[derive(clap::Args)]
pub struct WorkerArgs {
    #[arg(long)]
    pub id: String,
    #[arg(last = true, required = true)]
    pub command: Vec<String>,
}

impl SessionExecCommand {
    pub fn run(self) -> anyhow::Result<()> {
        let result = match self {
            Self::Protocol => {
                println!("{PROTOCOL}");
                Ok(0)
            }
            Self::ClaudeSeat(args) => claude::run(args),
            Self::Host(args) => crate::tokens_pool::private_workspace::transport::run(args),
            Self::Posture(args) => Ok(posture::run(&args)),
            #[cfg(target_os = "linux")]
            Self::Worker(args) => worker::run(args),
            #[cfg(not(target_os = "linux"))]
            Self::Worker(_) => Err(anyhow::anyhow!("session-exec worker requires Linux")),
        };
        match result {
            Ok(code) => std::process::exit(code),
            Err(error) => {
                if error.is::<crate::tokens_pool::private_workspace::AccountBusy>() {
                    refusal::announce_seat_busy();
                }
                eprintln!("session-exec: {error:#}");
                std::process::exit(refusal::REFUSAL_EXIT_CODE);
            }
        }
    }
}

extern "C" fn cancelled(signal: i32) {
    SIGNAL.store(signal, Ordering::Relaxed);
}

fn signals() {
    SIGNAL.store(0, Ordering::Relaxed);
    // Only this standalone CLI process installs handlers; never the daemon.
    unsafe {
        for signal in [libc::SIGTERM, libc::SIGINT, libc::SIGHUP] {
            libc::signal(signal, cancelled as *const () as libc::sighandler_t);
        }
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn ack(id: &str) -> Vec<u8> {
    format!("\x1eloom-session-clean:{id}\x1f").into_bytes()
}

#[cfg(test)]
mod tests;
