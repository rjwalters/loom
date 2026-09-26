//! A private, expiring stdin channel owns one persistent-container invocation.
//! No PID files or account-wide cancellation: Linux subreaping keeps even
//! setsid/double-fork descendants owned until they have all been reaped.

mod host;
pub use host::run as run_host;
mod owner;
#[cfg(target_os = "linux")]
mod worker;

use std::sync::atomic::{AtomicI32, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub const PROTOCOL: &str = "loom-session-exec-v1";
pub const LEASE_MS: u64 = 2000;
pub const GRACE: Duration = Duration::from_millis(1000);
pub const POLL: Duration = Duration::from_millis(20);
static SIGNAL: AtomicI32 = AtomicI32::new(0);

#[derive(clap::Subcommand)]
pub enum SessionExecCommand {
    /// Feature check. Old images must be updated; no unsupervised fallback.
    Protocol,
    /// Host transport; owns heartbeats, cancellation and cleanup acknowledgement.
    Host(HostArgs),
    /// Linux container endpoint. Stdin is the lease, never worker input.
    Worker(WorkerArgs),
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
            Self::Host(args) => crate::tokens_pool::private_workspace::transport::run(args),
            #[cfg(target_os = "linux")]
            Self::Worker(args) => worker::run(args),
            #[cfg(not(target_os = "linux"))]
            Self::Worker(_) => Err(anyhow::anyhow!("session-exec worker requires Linux")),
        };
        match result {
            Ok(code) => std::process::exit(code),
            Err(error) => {
                eprintln!("session-exec: {error:#}");
                std::process::exit(78);
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
