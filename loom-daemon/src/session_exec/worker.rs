use super::*;
use anyhow::{bail, Context, Result};
use std::io::{Read, Write};
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::time::Instant;

/// Direct children cannot have their PIDs reused until WE reap them. Repeated
/// subreaper adoption reaches detached descendants without scanning/killing
/// unrelated processes or trusting stale PID metadata.
fn children() -> Result<Vec<i32>> {
    let path = format!("/proc/self/task/{}/children", std::process::id());
    std::fs::read_to_string(path)?
        .split_whitespace()
        .map(|pid| pid.parse().context("invalid child PID"))
        .collect()
}

fn signal_children(signal: i32) -> Result<()> {
    for pid in children()? {
        if pid <= 1 {
            bail!("refusing unsafe child PID");
        }
        // No wait/reap between the owned-child snapshot and signalling.
        unsafe { libc::kill(pid, signal) };
    }
    Ok(())
}

fn reap(root: i32, result: &mut Option<i32>) -> Result<bool> {
    loop {
        let mut status = 0;
        let pid = unsafe { libc::waitpid(-1, &mut status, libc::WNOHANG) };
        if pid == 0 {
            return Ok(false);
        }
        if pid < 0 {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::ECHILD) {
                return Ok(true);
            }
            if error.kind() != std::io::ErrorKind::Interrupted {
                return Err(error.into());
            }
        } else if pid == root {
            *result = Some(if libc::WIFEXITED(status) {
                libc::WEXITSTATUS(status)
            } else {
                128 + libc::WTERMSIG(status)
            });
        }
    }
}

/// Absolute expiry prevents buffered heartbeats from resurrecting a job after
/// a delayed Docker startup. Host and VM clocks must agree within the lease.
fn read_lease(
    input: &mut impl Read,
    pending: &mut Vec<u8>,
    expiry: &mut u64,
    received: &mut Instant,
) -> Option<bool> {
    let mut bytes = [0; 256];
    loop {
        match input.read(&mut bytes) {
            Ok(0) => return Some(false),
            Ok(n) => pending.extend_from_slice(&bytes[..n]),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => return Some(false),
        }
        while let Some(end) = pending.iter().position(|b| *b == b'\n') {
            let line: Vec<_> = pending.drain(..=end).collect();
            let Ok(value) = std::str::from_utf8(&line)
                .unwrap_or("")
                .trim()
                .parse::<u64>()
            else {
                return Some(false);
            };
            if value <= now_ms() || value > now_ms() + LEASE_MS + 250 {
                return Some(false);
            }
            *expiry = value;
            *received = Instant::now();
        }
        if pending.len() > 32 {
            return Some(false);
        }
    }
    (*expiry != 0)
        .then(|| *expiry > now_ms() && received.elapsed() < Duration::from_millis(LEASE_MS))
}

pub(super) fn run(args: WorkerArgs) -> Result<i32> {
    // The ID binds only the acknowledgement, never a PID lookup or kill target.
    uuid::Uuid::parse_str(&args.id).context("invalid invocation ID")?;
    signals();
    unsafe {
        if libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0) != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        let flags = libc::fcntl(0, libc::F_GETFL);
        if flags < 0 || libc::fcntl(0, libc::F_SETFL, flags | libc::O_NONBLOCK) < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
    }
    // Validate procfs and the ownership mechanism BEFORE creating any worker.
    children()?;
    let mut input = std::io::stdin();
    let mut pending = Vec::new();
    let mut expiry = 0;
    let mut received = Instant::now();
    let start = Instant::now();
    let mut lease_valid = false;
    while start.elapsed() < Duration::from_millis(LEASE_MS) {
        if SIGNAL.load(Ordering::Relaxed) != 0 {
            std::io::stderr().write_all(&ack(&args.id))?;
            return Ok(143);
        }
        if let Some(valid) = read_lease(&mut input, &mut pending, &mut expiry, &mut received) {
            lease_valid = valid;
            break;
        }
        std::thread::sleep(POLL);
    }
    if !lease_valid || expiry <= now_ms() || SIGNAL.load(Ordering::Relaxed) != 0 {
        std::io::stderr().write_all(&ack(&args.id))?;
        return Ok(143);
    }
    let child = Command::new(&args.command[0])
        .args(&args.command[1..])
        .stdin(Stdio::null())
        .process_group(0)
        .spawn();
    let root = match child {
        Ok(child) => child.id() as i32,
        Err(error) => {
            eprintln!("session-exec: worker launch failed: {error}");
            std::io::stderr().write_all(&ack(&args.id))?;
            return Ok(127);
        }
    };
    let mut result = None;
    let mut stopping = None;
    let mut cancelled = false;
    loop {
        if reap(root, &mut result)? {
            std::io::stderr().write_all(&ack(&args.id))?;
            return Ok(if cancelled {
                143
            } else {
                result.unwrap_or(127)
            });
        }
        if stopping.is_none() {
            cancelled = SIGNAL.load(Ordering::Relaxed) != 0
                || !read_lease(&mut input, &mut pending, &mut expiry, &mut received)
                    .unwrap_or(false);
            if cancelled || result.is_some() {
                stopping = Some(Instant::now());
            }
        }
        if let Some(since) = stopping {
            signal_children(if since.elapsed() < GRACE {
                libc::SIGTERM
            } else {
                libc::SIGKILL
            })?;
        }
        std::thread::sleep(POLL);
    }
}
