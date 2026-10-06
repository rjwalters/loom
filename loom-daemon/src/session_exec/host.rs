use super::*;
use anyhow::{bail, Context, Result};
use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::process::{Command, Stdio};
use std::sync::{atomic::AtomicBool, Arc};
use std::time::Instant;

/// Strip only this invocation's private acknowledgement. Everything else is
/// copied byte-for-byte, including partial lines and non-UTF8 worker output.
pub(super) fn forward_stderr(
    mut input: impl Read,
    mut output: impl Write,
    mut capture: Option<File>,
    marker: &[u8],
    clean: &AtomicBool,
) -> std::io::Result<()> {
    let mut pending = Vec::new();
    let mut bytes = [0; 4096];
    loop {
        let count = input.read(&mut bytes)?;
        pending.extend_from_slice(&bytes[..count]);
        while let Some(at) = pending.windows(marker.len()).position(|w| w == marker) {
            pending.drain(at..at + marker.len());
            clean.store(true, Ordering::Release);
        }
        let flush = if count == 0 {
            pending.len()
        } else {
            pending.len().saturating_sub(marker.len() - 1)
        };
        output.write_all(&pending[..flush])?;
        output.flush()?;
        if let Some(file) = &mut capture {
            file.write_all(&pending[..flush])?;
        }
        pending.drain(..flush);
        if count == 0 {
            return Ok(());
        }
    }
}

/// How a bounded docker probe ended.
#[derive(Debug, PartialEq)]
pub(super) enum Probed {
    /// Docker exited zero; its trimmed stdout.
    Answered(String),
    /// Docker exited non-zero (for `inspect`: no such container, or it could
    /// not be inspected).
    Failed,
    /// The probe was given up on: its deadline, a host signal, or the
    /// launcher going away. It says nothing about the container.
    Abandoned,
}

impl Probed {
    fn answer(&self) -> Option<&str> {
        match self {
            Self::Answered(text) => Some(text),
            Self::Failed | Self::Abandoned => None,
        }
    }

    /// For the running-probe: whether docker actually said the container
    /// cannot take a dispatch, which is what `SESSION_DOWN` means (#10455).
    /// An abandoned probe did not say so, and stays a generic refusal.
    pub(super) fn says_not_running(&self) -> bool {
        match self {
            Self::Answered(state) => state != "true",
            Self::Failed => true,
            Self::Abandoned => false,
        }
    }
}

fn probe(args: &[&str], parent: i32) -> Result<Probed> {
    let mut child = Command::new("docker")
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()?;
    let started = Instant::now();
    loop {
        if let Some(status) = child.try_wait()? {
            let mut text = String::new();
            child.stdout.take().unwrap().read_to_string(&mut text)?;
            return Ok(if status.success() {
                Probed::Answered(text.trim().to_string())
            } else {
                Probed::Failed
            });
        }
        if started.elapsed() >= Duration::from_secs(3)
            || SIGNAL.load(Ordering::Relaxed) != 0
            || unsafe { libc::getppid() } != parent
        {
            let trigger = if started.elapsed() >= Duration::from_secs(3) {
                "probe-deadline"
            } else if SIGNAL.load(Ordering::Relaxed) != 0 {
                "host-signal"
            } else {
                "host-reparented"
            };
            eprintln!(
                "session-exec: probe abandoned (trigger={trigger}, docker {})",
                args.first().unwrap_or(&"")
            );
            child.kill()?;
            child.wait()?;
            return Ok(Probed::Abandoned);
        }
        std::thread::sleep(POLL);
    }
}

pub fn run(args: HostArgs) -> Result<i32> {
    signals();
    let parent = unsafe { libc::getppid() };
    // A unique temporary marker revokes the captured shell's invocation without
    // signalling a numeric child PID which Bash might already have reaped.
    // Checking it before launch also closes the trap-before-spawn race.
    let cancel_file = args.stderr_file.as_ref().map(|path| {
        let mut name = path.as_os_str().to_owned();
        name.push(".cancel");
        std::path::PathBuf::from(name)
    });
    let shell_cancelled = || cancel_file.as_ref().is_some_and(|path| path.exists());
    let owner = owner::Owner::new(args.owner_pid.unwrap_or(parent))?;
    if parent <= 1 {
        bail!("session adapter is already orphaned");
    }
    if args.owner_pid.is_some_and(|pid| pid != parent) {
        // Captured mode has one adapter shell between us and the launcher.
        // Verify the live relationship AFTER acquiring the stable exit watch:
        // an owner which died/recycled during shell preflight cannot be adopted.
        let ancestry = Command::new("ps")
            .args(["-o", "ppid=", "-p", &parent.to_string()])
            .output()?;
        let actual_owner = std::str::from_utf8(&ancestry.stdout)
            .unwrap_or("")
            .trim()
            .parse::<i32>()
            .ok();
        if !ancestry.status.success() || actual_owner != args.owner_pid {
            bail!("session launcher died before supervision started");
        }
    }
    // Running and not restarting: the rule `session_state::container_running`
    // applies to a full inspect object (#10455).
    let running = "{{and .State.Running (not .State.Restarting)}}";
    match probe(&["inspect", "-f", running, &args.container], parent) {
        Err(error)
            if error
                .downcast_ref::<std::io::Error>()
                .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound) =>
        {
            eprintln!("session-exec: 'docker' command not found in PATH");
            return Ok(127);
        }
        Ok(Probed::Answered(state)) if state == "true" => {}
        probed => {
            // #10455: name the cause for the daemon's terminal-record parser,
            // but only when docker said so. An abandoned probe (or one that
            // could not be run) refuses exactly as before, unnamed.
            if probed.as_ref().is_ok_and(Probed::says_not_running) {
                refusal::announce(crate::tokens_pool::health::TerminalClassification::SessionDown);
            }
            bail!("Session container '{}' is not running. Start it with: loom-daemon accounts session start {}", args.container, args.container.trim_start_matches("loom-codex-session-"))
        }
    }
    if probe(
        &[
            "exec",
            &args.container,
            "loom-daemon",
            "session-exec",
            "protocol",
        ],
        parent,
    )?
    .answer()
        != Some(PROTOCOL)
    {
        bail!("session container '{}' requires loom-daemon session-exec protocol {PROTOCOL}; update/recreate the session image before dispatch (no unsupervised fallback)", args.container);
    }
    if SIGNAL.load(Ordering::Relaxed) != 0
        || unsafe { libc::getppid() } != parent
        || !owner.alive()
        || shell_cancelled()
    {
        let trigger = if SIGNAL.load(Ordering::Relaxed) != 0 {
            "host-signal"
        } else if unsafe { libc::getppid() } != parent {
            "host-reparented"
        } else if !owner.alive() {
            "owner-dead"
        } else {
            "shell-cancelled"
        };
        eprintln!("session-exec: refusing dispatch before start (trigger={trigger})");
        return Ok(143);
    }
    let id = uuid::Uuid::new_v4().to_string();
    let capture = args.stderr_file.map(File::create).transpose()?;
    let mut command = Command::new("docker");
    command.args(["exec", "-i", "--workdir", &args.workdir]);
    for env in args.env {
        command.args(["--env", &env]);
    }
    // Docker's client must survive the caller's process-group TERM so it can
    // deliver cancellation and collect the container cleanup acknowledgement.
    let mut child = command
        .args([
            &args.container,
            "loom-daemon",
            "session-exec",
            "worker",
            "--id",
            &id,
            "--",
        ])
        .args(args.command)
        .stdin(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .spawn()
        .context("start supervised Docker exec")?;
    let mut input = child.stdin.take().unwrap();
    unsafe {
        let fd = input.as_raw_fd();
        let flags = libc::fcntl(fd, libc::F_GETFL);
        libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
    }
    let clean = Arc::new(AtomicBool::new(false));
    let reader_clean = clean.clone();
    let stderr = child.stderr.take().unwrap();
    let (reader_done, reader_result) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let result = forward_stderr(stderr, std::io::stderr(), capture, &ack(&id), &reader_clean);
        let _ = reader_done.send(result);
    });
    let mut last_beat = Instant::now() - Duration::from_secs(1);
    let mut stopping = None;
    loop {
        if let Some(status) = child.try_wait()? {
            let drained =
                matches!(reader_result.recv_timeout(Duration::from_millis(250)), Ok(Ok(())));
            if !drained || !clean.load(Ordering::Acquire) {
                // Transport failure is NOT a cleanup acknowledgement. Allow
                // the last lease + TERM grace to expire before returning.
                std::thread::sleep(Duration::from_millis(LEASE_MS) + GRACE);
                bail!("container cleanup acknowledgement missing; lease recovery window elapsed; verify container health before redispatch");
            }
            return Ok(if stopping.is_some() {
                143
            } else {
                status.code().unwrap_or(128 + status.signal().unwrap_or(1))
            });
        }
        if stopping.is_none()
            && (SIGNAL.load(Ordering::Relaxed) != 0
                || unsafe { libc::getppid() } != parent
                || !owner.alive()
                || shell_cancelled())
        {
            let trigger = if SIGNAL.load(Ordering::Relaxed) != 0 {
                "host-signal"
            } else if unsafe { libc::getppid() } != parent {
                "host-reparented"
            } else if !owner.alive() {
                "owner-dead"
            } else {
                "shell-cancelled"
            };
            eprintln!("session-exec: cancelling dispatch (trigger={trigger})");
            let _ = input.write_all(b"cancel\n");
            stopping = Some(Instant::now());
        }
        if stopping.is_none() && last_beat.elapsed() >= Duration::from_millis(250) {
            // One pre-formed line per beat (#9067): a single write of
            // <= PIPE_BUF bytes on an O_NONBLOCK pipe is all-or-EAGAIN, so a
            // full buffer can never emit a half-written lease line (the
            // fmt-per-piece writes in `writeln!` could split a number from
            // its newline under backpressure, and the reassembled garbage
            // parsed as a cancelled lease on the worker). A failed beat is
            // skipped and retried, not fatal: the worker's stall tolerance
            // rides out the backpressure, and real death is signalled by the
            // explicit cancel line above, EOF, or this side's own exit.
            let beat = format!("{}\n", now_ms() + LEASE_MS);
            if input.write_all(beat.as_bytes()).is_ok() {
                last_beat = Instant::now();
            }
        }
        if stopping.is_some_and(|start| start.elapsed() >= Duration::from_secs(4)) {
            child.kill()?;
            child.wait()?;
            bail!("container cleanup unconfirmed after cancellation; lease expired, inspect session health before redispatch");
        }
        std::thread::sleep(POLL);
    }
}
