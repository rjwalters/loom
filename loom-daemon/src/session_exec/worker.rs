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

/// Absolute expiry still gates lease ACCEPTANCE — a buffered heartbeat that
/// arrives already expired can neither authorize launch nor extend a lease —
/// but an out-of-window line is skipped, never treated as cancellation
/// (#9067): under CI load the docker-exec stdin transport buffers heartbeats
/// for longer than `LEASE_MS`, and the arriving data itself proves the host
/// is alive. A stale burst followed by a fresh tail therefore extends the
/// lease, exactly like a heartbeat that arrived on time. Cancellation is
/// decisive only via the explicit non-numeric `cancel` line, a read error, or
/// EOF with nothing accepted in the same pass; a merely silent channel is the
/// caller's `STALL_MS` decision, measured from the last data arrival
/// (`received` updates on any bytes, not only accepted leases). Host and VM
/// clocks must still agree within the lease for a heartbeat to be accepted,
/// but transient disagreement now stalls acceptance rather than killing the
/// dispatch.
fn read_lease(
    input: &mut impl Read,
    pending: &mut Vec<u8>,
    expiry: &mut u64,
    received: &mut Instant,
) -> Option<bool> {
    let mut bytes = [0; 256];
    let mut accepted = false;
    let mut eof = false;
    loop {
        if eof {
            break;
        }
        match input.read(&mut bytes) {
            Ok(0) => eof = true,
            Ok(n) => {
                pending.extend_from_slice(&bytes[..n]);
                // Any data is channel liveness, even when every line in it is
                // outside the acceptance window (#9067).
                *received = Instant::now();
            }
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
                continue;
            }
            *expiry = value;
            *received = Instant::now();
            accepted = true;
        }
        if pending.len() > 32 {
            return Some(false);
        }
    }
    // An EOF sharing a pass with an accepted lease cannot veto the grant
    // (#8793): the host's authorization is already in hand, so the recorded
    // outcome stays the invocation's own (launch failure 127), not a
    // cancellation the caller never requested. Only a pass that accepted
    // nothing (pure EOF, explicit cancel, broken expiry) cancels.
    if accepted {
        return Some(true);
    }
    if eof {
        return Some(false);
    }
    None
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
    let mut startup_trigger = "";
    // STALL_MS, not LEASE_MS: a docker exec attach under CI load can take
    // longer than one lease horizon to deliver the first heartbeat (#9067).
    // EOF and explicit `cancel` still refuse instantly; only a channel that
    // stays silent-and-open for the whole window expires here.
    while start.elapsed() < Duration::from_millis(STALL_MS) {
        if SIGNAL.load(Ordering::Relaxed) != 0 {
            startup_trigger = "worker-signal";
            break;
        }
        if let Some(valid) = read_lease(&mut input, &mut pending, &mut expiry, &mut received) {
            lease_valid = valid;
            if !valid {
                startup_trigger = "lease-channel-closed-or-corrupt";
            }
            break;
        }
        std::thread::sleep(POLL);
    }
    if lease_valid && (expiry <= now_ms() || SIGNAL.load(Ordering::Relaxed) != 0) {
        startup_trigger = if SIGNAL.load(Ordering::Relaxed) != 0 {
            "worker-signal"
        } else {
            "lease-expired-before-launch"
        };
        lease_valid = false;
    }
    if !lease_valid {
        if startup_trigger.is_empty() {
            startup_trigger = "startup-window-expired";
        }
        eprintln!(
            "session-exec: worker {} cancelled before launch (trigger={}, waited_ms={}, expiry={}, now={})",
            args.id,
            startup_trigger,
            start.elapsed().as_millis(),
            expiry,
            now_ms()
        );
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
            // #9067: only decisive channel events (explicit `cancel`, read
            // error, EOF) or a channel silent-and-open past STALL_MS cancel a
            // running invocation. A delayed or clock-skewed heartbeat used to
            // cancel here and SIGTERM healthy work under CI load.
            let signalled = SIGNAL.load(Ordering::Relaxed) != 0;
            let channel = read_lease(&mut input, &mut pending, &mut expiry, &mut received);
            cancelled = signalled
                || matches!(channel, Some(false))
                || received.elapsed() >= Duration::from_millis(STALL_MS);
            if cancelled || result.is_some() {
                if cancelled {
                    let trigger = if signalled {
                        "worker-signal"
                    } else if matches!(channel, Some(false)) {
                        "lease-channel-closed-or-corrupt"
                    } else {
                        "lease-channel-stalled"
                    };
                    eprintln!(
                        "session-exec: worker {} cancelling invocation (trigger={}, expiry={}, now={}, last_data_ago_ms={})",
                        args.id,
                        trigger,
                        expiry,
                        now_ms(),
                        received.elapsed().as_millis()
                    );
                }
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    /// Worker-stdin shape: return exactly what was written, then block
    /// forever (the channel stays open). Models a live-but-slow docker exec
    /// stdin transport (#9067).
    struct BytesThenWouldBlock(Vec<u8>, bool);
    impl Read for BytesThenWouldBlock {
        fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
            if self.1 {
                return Err(std::io::ErrorKind::WouldBlock.into());
            }
            self.1 = true;
            let n = self.0.len().min(out.len());
            out[..n].copy_from_slice(&self.0);
            self.0.drain(..n);
            Ok(n)
        }
    }

    /// Worker-stdin shape: return exactly what was written, then EOF (0). A
    /// single `read_lease` call over it is one read pass whose data and
    /// channel-close arrive together — the mixed burst #8793 races on.
    struct Pipe(Vec<u8>);
    impl Read for Pipe {
        fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
            let n = self.0.len().min(out.len());
            out[..n].copy_from_slice(&self.0[..n]);
            self.0.drain(..n);
            Ok(n)
        }
    }

    /// A channel that blocks once (no data yet), then closes.
    struct WouldBlockOnce {
        blocked: bool,
    }
    impl Read for WouldBlockOnce {
        fn read(&mut self, _out: &mut [u8]) -> std::io::Result<usize> {
            if self.blocked {
                self.blocked = false;
                Err(std::io::ErrorKind::WouldBlock.into())
            } else {
                Ok(0)
            }
        }
    }

    fn lease_line() -> Vec<u8> {
        format!("{}\n", now_ms() + LEASE_MS).into_bytes()
    }

    #[test]
    fn eof_in_the_same_pass_cannot_veto_an_accepted_lease() {
        let mut expiry = 0;
        let mut received = Instant::now();
        assert_eq!(
            read_lease(&mut Pipe(lease_line()), &mut Vec::new(), &mut expiry, &mut received),
            Some(true)
        );
        // A later pass over the now-closed channel is a pure EOF and cancels.
        assert_eq!(
            read_lease(&mut Pipe(Vec::new()), &mut Vec::new(), &mut expiry, &mut received),
            Some(false)
        );
    }

    #[test]
    fn eof_without_a_lease_cancels() {
        let mut expiry = 0;
        let mut received = Instant::now();
        assert_eq!(
            read_lease(&mut Pipe(Vec::new()), &mut Vec::new(), &mut expiry, &mut received),
            Some(false)
        );
    }

    #[test]
    fn explicit_cancel_still_wins_after_a_grant() {
        // The host cancels with the explicit line, never by closing stdin:
        // it must cancel even when it follows a valid lease in one burst
        // (the fifth-suite's fourth case).
        let mut bytes = lease_line();
        bytes.extend_from_slice(b"cancel\n");
        let mut expiry = 0;
        let mut received = Instant::now();
        assert_eq!(
            read_lease(&mut Pipe(bytes), &mut Vec::new(), &mut expiry, &mut received),
            Some(false)
        );
    }

    #[test]
    fn garbage_lease_lines_cancel() {
        for line in [b"cancel\n".to_vec(), b"not-a-number\n".to_vec()] {
            let mut expiry = 0;
            let mut received = Instant::now();
            assert_eq!(
                read_lease(&mut Pipe(line), &mut Vec::new(), &mut expiry, &mut received),
                Some(false)
            );
        }
    }

    #[test]
    fn stale_lease_lines_do_not_cancel_an_open_channel() {
        // #9067: a heartbeat buffered by the docker exec transport longer than
        // the lease horizon arrives expired. Old behavior: immediate
        // cancellation (Some(false)) — the exit-143 flake. The line is data,
        // so the host is provably alive; it must be skipped, not acted on.
        let mut expiry = 0;
        let mut received = Instant::now() - Duration::from_secs(5);
        assert_eq!(
            read_lease(
                &mut BytesThenWouldBlock(b"1\n".to_vec(), false),
                &mut Vec::new(),
                &mut expiry,
                &mut received
            ),
            None
        );
        assert_eq!(expiry, 0);
        // The stale data still proves the channel is delivering.
        assert!(received.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn stale_burst_with_fresh_tail_extends_the_lease() {
        // A transport backlog delivered as one burst: expired beats first, the
        // current heartbeat last. Only the fresh tail is accepted.
        let fresh = now_ms() + LEASE_MS;
        let mut bytes = b"1\n".to_vec();
        bytes.extend_from_slice(&format!("{fresh}\n").into_bytes());
        let mut expiry = 0;
        let mut received = Instant::now();
        assert_eq!(
            read_lease(
                &mut BytesThenWouldBlock(bytes, false),
                &mut Vec::new(),
                &mut expiry,
                &mut received
            ),
            Some(true)
        );
        assert_eq!(expiry, fresh);
    }

    #[test]
    fn far_future_lease_line_is_skipped_not_cancelled() {
        // Host/container clock skew beyond the acceptance band must not
        // cancel a dispatch whose host is demonstrably heartbeating (#9067).
        let skewed = format!("{}\n", now_ms() + LEASE_MS + 5_000);
        let mut expiry = 0;
        let mut received = Instant::now();
        assert_eq!(
            read_lease(
                &mut BytesThenWouldBlock(skewed.into_bytes(), false),
                &mut Vec::new(),
                &mut expiry,
                &mut received
            ),
            None
        );
        assert_eq!(expiry, 0);
    }

    #[test]
    fn pass_with_no_data_and_no_grant_yet_keeps_waiting() {
        // WouldBlock before the host's first beat: no decision (None) so the
        // launch gate keeps polling until the lease window is spent.
        let mut expiry = 0;
        let mut received = Instant::now();
        let mut input = WouldBlockOnce { blocked: true };
        assert_eq!(read_lease(&mut input, &mut Vec::new(), &mut expiry, &mut received), None);
        // The same channel closing later is a pure EOF: cancel.
        assert_eq!(
            read_lease(&mut input, &mut Vec::new(), &mut expiry, &mut received),
            Some(false)
        );
    }
}
