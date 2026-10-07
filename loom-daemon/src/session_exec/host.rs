use super::*;
use crate::tokens_pool::health::TerminalClassification;
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
}

fn probe(args: &[&str], parent: i32) -> Result<Probed> {
    let mut child = Command::new("docker")
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()?;
    // Drain stdout while waiting: a full `docker inspect` object can exceed
    // the pipe buffer, and a child blocked on a full pipe never exits, which
    // would read as the probe deadline (#10364).
    let mut stdout = child.stdout.take().unwrap();
    let reader = std::thread::spawn(move || {
        let mut text = String::new();
        stdout.read_to_string(&mut text).map(|_| text)
    });
    let started = Instant::now();
    loop {
        if let Some(status) = child.try_wait()? {
            let text = reader
                .join()
                .map_err(|_| anyhow::anyhow!("docker probe reader panicked"))??;
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

/// What the one pre-exec `docker inspect` says about a dispatch (#10364).
#[derive(Debug, PartialEq)]
pub(super) enum Preflight {
    /// Docker said the container cannot take a dispatch: it does not exist,
    /// or its inspect is not a running container. Refuse as `SESSION_DOWN`.
    NotRunning,
    /// Docker did not say anything (the probe was abandoned). Refuse, as
    /// before, but name no cause: the container may well be running (#10455).
    Unknown,
    /// Docker said the container is running and listed its mounts, and none
    /// covers the workdir. Refuse as `SESSION_MOUNT_STALE`. Carries the
    /// inspect object, for the operator's recreate command.
    MountStale(serde_json::Value),
    /// Running and the workdir is mounted.
    Ready,
}

/// Read both pre-exec answers from one `docker inspect --type container`.
///
/// Whether the container is running fails closed, exactly as the
/// `{{.State.Running}}` template probe this replaces did: anything but an
/// answered, parsed object that `session_state::container_running` accepts
/// refuses. The mount check then reads that same object, so it costs no
/// docker call and can only refuse on what docker actually answered: an
/// abandoned or failed inspect never reaches it, so it never becomes
/// `SESSION_MOUNT_STALE`.
pub(super) fn preflight(inspect: &Probed, workdir: &str) -> Preflight {
    use crate::tokens_pool::session_state::{container_running, workdir_unmounted};
    let text = match inspect {
        Probed::Abandoned => return Preflight::Unknown,
        Probed::Failed => return Preflight::NotRunning,
        Probed::Answered(text) => text,
    };
    let state = serde_json::from_str::<serde_json::Value>(text)
        .ok()
        .and_then(|value| value.get(0).cloned());
    match state {
        Some(state) if !container_running(&state) => Preflight::NotRunning,
        Some(state) if workdir_unmounted(&state, workdir) => Preflight::MountStale(state),
        Some(_) => Preflight::Ready,
        None => Preflight::NotRunning,
    }
}

/// What the operator is told when a dispatch is refused for a stale mount:
/// the cause and the recreate command, on one line. The machine-readable
/// cause is the `refusal::announce` line printed with it.
pub(super) fn mount_stale_line(
    container: &str,
    workdir: &str,
    state: &serde_json::Value,
) -> String {
    let account = container.trim_start_matches("loom-codex-session-");
    let workspace = crate::tokens_pool::session_state::workspace_label(state)
        .map_or_else(|| "<checkout parent>".to_string(), |w| w.display().to_string());
    format!(
        "session-exec: {workdir} is not mounted in {container}, which was created before this \
         repository was registered (#10364); when the daemon's session reconciler is enabled it \
         recreates the container once idle, otherwise run: loom-daemon accounts session stop \
         {account} && loom-daemon accounts session start {account} --mount-workspace \
         {workspace}"
    )
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
    // Held (shared) from before the first inspect until this function
    // returns, i.e. past the worker exec: a teardown of the container takes
    // it exclusively and defers while any dispatch holds it, including in
    // the gaps `docker top` cannot see (#10364). `None` (lock unusable, or a
    // teardown still running after the wait): dispatch proceeds as before.
    let _dispatch_lock =
        crate::tokens_pool::session_dispatch_lock::shared_for_dispatch(&args.container);
    // One inspect answers both pre-exec questions: is the container running
    // (#10455), and does it mount this tick's working directory (#10364)? A
    // host-mode container mounts each registered repo separately, fixed at
    // creation, so a repo registered since is not inside it and `docker exec
    // --workdir` would die with `chdir to cwd … no such file or directory`.
    // A refusal names its cause for the daemon's terminal-record parser only
    // when docker said so; an abandoned probe (or one that could not be run)
    // refuses exactly as before, unnamed.
    let preflight = match probe(&["inspect", "--type", "container", &args.container], parent) {
        Err(error)
            if error
                .downcast_ref::<std::io::Error>()
                .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound) =>
        {
            eprintln!("session-exec: 'docker' command not found in PATH");
            return Ok(127);
        }
        Ok(inspect) => preflight(&inspect, &args.workdir),
        Err(_) => Preflight::Unknown,
    };
    match preflight {
        Preflight::Ready => {}
        not_ready @ (Preflight::NotRunning | Preflight::Unknown) => {
            if not_ready == Preflight::NotRunning {
                refusal::announce(TerminalClassification::SessionDown);
            }
            bail!("Session container '{}' is not running. Start it with: loom-daemon accounts session start {}", args.container, args.container.trim_start_matches("loom-codex-session-"))
        }
        Preflight::MountStale(state) => {
            refusal::announce(TerminalClassification::SessionMountStale);
            eprintln!("{}", mount_stale_line(&args.container, &args.workdir, &state));
            return Ok(refusal::REFUSAL_EXIT_CODE);
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
